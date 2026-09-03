/**
 * ID token lifecycle for the portal.
 *
 * This is the part of portal sign-in that actually breaks in production, and it does
 * not break during a demo. Identity Platform ID tokens live for one hour. A
 * supervisor signs in at 09:00, works through the compliance queue, goes to a
 * stand-up, comes back at 10:05, clicks a flag — and every request 401s. Nothing
 * about that is visible in a five-minute review of a sign-in flow, so the lifecycle
 * gets its own module and its own tests.
 *
 * Three behaviours, in the order they matter:
 *
 *  1. **Refresh proactively.** A timer fires before expiry so the token in hand is
 *     almost never the reason a request fails. Reacting to 401s alone would mean the
 *     first click after any idle period pays for a refresh, and half the screens in
 *     this portal fire several requests at once on mount.
 *  2. **Refresh forcibly on demand.** `ApiClient` calls `refresh()` when the gateway
 *     rejects a token, and it must not be answered from cache — the cached copy is
 *     precisely what was just refused. That is why `IdTokenFetcher` takes an explicit
 *     `forceRefresh` flag rather than being a plain getter.
 *  3. **Fail to signed-out, not to a loop.** If a refresh fails, the credential is
 *     gone: the refresh token was revoked, the user was disabled, the tenant was
 *     deleted, or the device has been offline long enough for the refresh token to be
 *     rejected. The cache latches into a lost state, `get()` returns null from then
 *     on so `ApiClient` raises a clean `no_credentials`, and `onLost` puts the portal
 *     back on the sign-in screen. Retrying forever would produce the 401 loop
 *     `session.tsx` has always warned about.
 *
 * The clock and the timer are injected so all of this is testable without waiting an
 * hour, which is the only reason any of it is verified at all.
 */

/**
 * Mints an ID token. `forceRefresh` must bypass every cache between here and Google.
 * Satisfied by Firebase's `User.getIdToken(forceRefresh)`.
 */
export type IdTokenFetcher = (forceRefresh: boolean) => Promise<string>;

export type TimerHandle = ReturnType<typeof setTimeout>;

export interface IdTokenCacheOptions {
  /** Injectable clock, in epoch milliseconds. */
  now?: () => number;
  setTimer?: (fn: () => void, ms: number) => TimerHandle;
  clearTimer?: (handle: TimerHandle) => void;
  /**
   * Called once, when a refresh has failed and the credential is considered gone.
   * The portal's job on hearing this is to render the sign-in screen — not to retry.
   */
  onLost?: (reason: string) => void;
  /** How far ahead of expiry a token is treated as already stale. */
  skewMs?: number;
}

/**
 * Refresh this far before `exp`.
 *
 * Five minutes covers three things at once: a request in flight when the timer fires
 * still completes against a valid token; a collections desktop whose clock is a
 * couple of minutes off (the gateway allows leeway for exactly this reason, see
 * `Verifier.Leeway`) does not spend that skew on an expired token; and the window is
 * wide enough that a slow refresh has time to finish.
 */
const DEFAULT_SKEW_MS = 5 * 60 * 1000;

/**
 * Floor on the proactive timer. Without it, a token that arrives already inside the
 * skew window would schedule a 0 ms timer, refresh, get another such token, and spin
 * — turning a clock-skew problem into a request storm against Identity Platform.
 */
const MIN_REFRESH_DELAY_MS = 30 * 1000;

/**
 * Assumed lifetime when `exp` cannot be read. Deliberately much shorter than the real
 * hour: an unreadable token is refreshed early and often rather than trusted long. We
 * do not verify the signature — that is the gateway's job and it has the keys — so a
 * lie about `exp` costs at most a wasted refresh.
 */
const UNKNOWN_EXPIRY_TTL_MS = 10 * 60 * 1000;

export class IdTokenCache {
  readonly #fetch: IdTokenFetcher;
  readonly #now: () => number;
  readonly #setTimer: (fn: () => void, ms: number) => TimerHandle;
  readonly #clearTimer: (handle: TimerHandle) => void;
  readonly #onLost: (reason: string) => void;
  readonly #skewMs: number;

  #token: string | null = null;
  #expiresAtMs = 0;
  #timer: TimerHandle | null = null;
  #inFlight: Promise<string | null> | null = null;
  #inFlightForced = false;
  #lost = false;
  #disposed = false;

  constructor(fetch: IdTokenFetcher, options: IdTokenCacheOptions = {}) {
    this.#fetch = fetch;
    this.#now = options.now ?? (() => Date.now());
    this.#setTimer = options.setTimer ?? ((fn, ms) => setTimeout(fn, ms));
    this.#clearTimer = options.clearTimer ?? ((handle) => clearTimeout(handle));
    this.#onLost = options.onLost ?? (() => undefined);
    this.#skewMs = options.skewMs ?? DEFAULT_SKEW_MS;
  }

  /** True once a refresh has failed. The portal reads this to decide it is signed out. */
  get lost(): boolean {
    return this.#lost;
  }

  /**
   * The token to put in an Authorization header, or null when there is none.
   *
   * Null is a normal answer, not an error: `ApiClient` turns it into a
   * `no_credentials` failure before making a request, which is what the portal wants
   * — an honest "you are signed out" rather than a round trip that comes back 401 and
   * cannot be told apart from an expiry.
   */
  async get(): Promise<string | null> {
    if (this.#lost || this.#disposed) return null;
    if (this.#token !== null && this.#now() < this.#expiresAtMs - this.#skewMs) return this.#token;
    return this.#refresh(false);
  }

  /**
   * Forced refresh, for `ApiClient`'s single retry after a 401.
   *
   * Always goes to the provider even when the cached token still looks fresh by our
   * own reckoning, because the gateway has just told us otherwise and it is the one
   * holding the signing keys.
   */
  refresh(): Promise<string | null> {
    if (this.#lost || this.#disposed) return Promise.resolve(null);
    return this.#refresh(true);
  }

  /**
   * Stops the timer and drops the token. Called when the user signs out and when the
   * provider swaps users — a live timer holding a closure over the previous user is
   * how a signed-out portal quietly mints tokens for whoever just left.
   */
  dispose(): void {
    this.#disposed = true;
    this.#token = null;
    this.#expiresAtMs = 0;
    this.#cancelTimer();
  }

  /* ---------------------------------------------------------- internals */

  #refresh(force: boolean): Promise<string | null> {
    // Coalesce, but never let a non-forced refresh satisfy a forced one: the
    // non-forced call is allowed to return the cached token, which is the token the
    // gateway just rejected.
    if (this.#inFlight !== null && (!force || this.#inFlightForced)) return this.#inFlight;

    const run = this.#fetchToken(force).finally(() => {
      if (this.#inFlight === run) {
        this.#inFlight = null;
        this.#inFlightForced = false;
      }
    });
    this.#inFlight = run;
    this.#inFlightForced = force;
    return run;
  }

  async #fetchToken(force: boolean): Promise<string | null> {
    let token: string;
    try {
      token = await this.#fetch(force);
    } catch (cause) {
      this.#markLost(describeRefreshFailure(cause));
      return null;
    }
    if (this.#disposed) return null;
    if (typeof token !== 'string' || token === '') {
      // A provider that resolves with nothing has failed without saying so. Treating
      // it as success would put an empty bearer header on the wire, which the gateway
      // answers 400 rather than 401 — a failure that never reaches the sign-in path.
      this.#markLost('The identity provider returned an empty token.');
      return null;
    }

    this.#token = token;
    this.#expiresAtMs = expiryOf(token, this.#now());
    this.#scheduleRefresh();
    return token;
  }

  #markLost(reason: string): void {
    if (this.#lost) return;
    this.#lost = true;
    this.#token = null;
    this.#expiresAtMs = 0;
    this.#cancelTimer();
    if (!this.#disposed) this.#onLost(reason);
  }

  #scheduleRefresh(): void {
    this.#cancelTimer();
    if (this.#disposed || this.#lost) return;
    const delay = Math.max(this.#expiresAtMs - this.#skewMs - this.#now(), MIN_REFRESH_DELAY_MS);
    this.#timer = this.#setTimer(() => {
      this.#timer = null;
      // Forced: a non-forced fetch inside the skew window may hand back the same
      // near-expiry token, which would reschedule and achieve nothing.
      void this.#refresh(true);
    }, delay);
  }

  #cancelTimer(): void {
    if (this.#timer !== null) {
      this.#clearTimer(this.#timer);
      this.#timer = null;
    }
  }
}

/**
 * Reads `exp` out of a JWT without verifying it.
 *
 * Verification is the gateway's job — it has Google's public keys and fails closed
 * without them (`CachingKeySource`). All we need is a schedule, and the worst a
 * forged `exp` can do to a schedule is waste a refresh. An unreadable token is
 * treated as short-lived rather than long-lived, so the failure direction is "refresh
 * too often", not "carry an expired token".
 */
export function expiryOf(token: string, nowMs: number): number {
  const parts = token.split('.');
  const payload = parts.length === 3 ? parts[1] : undefined;
  if (payload === undefined) return nowMs + UNKNOWN_EXPIRY_TTL_MS;
  try {
    const json: unknown = JSON.parse(base64UrlDecode(payload));
    if (typeof json === 'object' && json !== null) {
      const exp = (json as Record<string, unknown>)['exp'];
      // `exp` is seconds since the epoch (RFC 7519), not milliseconds.
      if (typeof exp === 'number' && Number.isFinite(exp) && exp > 0) return exp * 1000;
    }
  } catch {
    // Not base64, not JSON, or a payload we cannot read. Fall through.
  }
  return nowMs + UNKNOWN_EXPIRY_TTL_MS;
}

function base64UrlDecode(value: string): string {
  const padded = value.replace(/-/g, '+').replace(/_/g, '/');
  // `atob` exists in every browser the portal supports and in Node 18+, so no
  // polyfill and no Buffer import (which would not resolve in the browser bundle).
  return atob(padded + '='.repeat((4 - (padded.length % 4)) % 4));
}

/**
 * Operator-facing text for a failed refresh. Never includes the provider's raw
 * message: it can name the account, and this string is rendered.
 */
function describeRefreshFailure(cause: unknown): string {
  const code = typeof cause === 'object' && cause !== null ? (cause as { code?: unknown }).code : undefined;
  if (code === 'auth/user-disabled') return 'This account has been disabled.';
  if (code === 'auth/user-token-expired' || code === 'auth/invalid-user-token') {
    return 'Your session was ended elsewhere.';
  }
  if (code === 'auth/network-request-failed') return 'Could not reach the identity provider to renew your session.';
  return 'Your session could not be renewed.';
}
