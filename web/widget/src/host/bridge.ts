/**
 * Resolves the host object and normalises what it returns.
 *
 * Two things make this more than a property lookup:
 *
 *  1. WebView2 injects `hostObjects` asynchronously during navigation, so the object
 *     can be missing for the first few frames of a cold start. Polling briefly
 *     before falling back to the mock stops a real widget from booting into
 *     developer mode on a slow machine.
 *  2. A host-object proxy marshals every member, including ones we did not ask for.
 *     Values coming back are validated here so a native regression surfaces as an
 *     error state rather than as a widget rendering `undefined`.
 */
import type { CaptureState, CaptureTier } from '@sentinel/shared';
import { MockSentinelHost } from './mock.js';
import { HOST_ERROR_CAUSES } from './types.js';
import type {
  HostAuthState,
  HostError,
  HostErrorCause,
  HostPendingCall,
  HostState,
  SentinelHost,
  WebView2Window,
} from './types.js';

const HOST_WAIT_MS = 1500;
const HOST_POLL_MS = 50;

export interface ResolvedHost {
  host: SentinelHost;
  /** True when the native object was found; false means the mock is in use. */
  native: boolean;
}

function nativeHost(): SentinelHost | undefined {
  const w = globalThis as unknown as WebView2Window;
  return w.chrome?.webview?.hostObjects?.sentinel;
}

export async function resolveHost(waitMs = HOST_WAIT_MS): Promise<ResolvedHost> {
  const deadline = Date.now() + waitMs;
  for (;;) {
    const host = nativeHost();
    if (host) return { host, native: true };
    if (Date.now() >= deadline) break;
    await new Promise((resolve) => setTimeout(resolve, HOST_POLL_MS));
  }
  const mock = new MockSentinelHost();
  // Exposed only in a dev build so a developer can drive the states from the
  // console (`__sentinelMock.simulateCall()`); the production bundle has no handle
  // on the host object at all.
  if (import.meta.env?.DEV) {
    (globalThis as unknown as { __sentinelMock?: MockSentinelHost }).__sentinelMock = mock;
  }
  return { host: mock, native: false };
}

/**
 * Every call across the host-object boundary is bounded by this.
 *
 * WebView2 marshals host-object members over an IPC channel to the agent process. If
 * that process is wedged — mid-restart, blocked on a COM call into an audio device,
 * or gone while the WebView survives — the returned promise never settles. It does not
 * reject; it simply never resolves, and `await` on it is indefinite. That is the
 * failure mode behind every "the widget just says Starting Sentinel" report, and no
 * amount of try/catch catches it, because nothing is thrown.
 *
 * So the rule is that no host call is awaited without a deadline. Timing out is
 * treated as the honest answer to the question asked ("no token", "no configured URL",
 * "capture state unknown") rather than as an error to surface, because an agent on a
 * call must never be looking at a spinner or a stack trace.
 */
export async function withHostTimeout<T>(
  work: Promise<T> | T,
  timeoutMs: number,
  onTimeout: T,
): Promise<T> {
  if (!(work instanceof Promise)) return work;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      work,
      new Promise<T>((resolve) => {
        timer = setTimeout(() => resolve(onTimeout), timeoutMs);
      }),
    ]);
  } finally {
    // The losing promise is abandoned, not cancelled — there is no way to cancel a
    // marshalled call — but the timer must not keep the WebView awake for a request
    // whose answer arrived first.
    if (timer !== undefined) clearTimeout(timer);
  }
}

/** One host-object round trip. Long enough for a busy agent process, short enough that
 *  a wedged one costs the agent a second and not a shift. */
const TOKEN_CALL_TIMEOUT_MS = 1500;

/**
 * How long a forced refresh waits for the native layer to publish a *different* token
 * before giving up. Best effort by design: see `refreshToken` below.
 */
const TOKEN_REFRESH_WINDOW_MS = 3000;
const TOKEN_REFRESH_POLL_MS = 300;

export interface HostTokenProvider {
  /** Read per request. Null means "no credential right now" — a state, not a failure. */
  getToken: () => Promise<string | null>;
  /** Forced re-read, for `ApiClient`'s single retry after a 401. */
  refreshToken: () => Promise<string | null>;
}

export interface HostTokenProviderOptions {
  callTimeoutMs?: number;
  refreshWindowMs?: number;
  refreshPollMs?: number;
  /** Injectable so the refresh window can be tested without waiting three seconds. */
  sleep?: (ms: number) => Promise<void>;
}

/**
 * Token provider for the API client.
 *
 * Spec 6.7 says the native layer injects a token and does not say how, so both shapes
 * are accepted: `sentinel.getToken()` on the host object, or `__SENTINEL_TOKEN__` on
 * the window. Nothing is cached on this side — the agent process rotates the ID token
 * roughly hourly and a cached copy would 401 mid-shift — so the interesting behaviour
 * is what happens when there is no token, which is most of the situations the widget
 * actually boots into:
 *
 *  - **Absent at startup.** The widget is launched by the service before the agent has
 *    signed anyone in, so the first read legitimately returns nothing. Because the
 *    provider is called per request rather than once, a token that arrives ten minutes
 *    later is picked up with no reload and no re-initialisation.
 *  - **Never present.** The agent is signed out. `null` becomes a `no_credentials`
 *    failure inside `ApiClient`, which the history tab renders as "sign in", and the
 *    widget's main view is already `SignedOut` because the native state says so. What
 *    must not happen is an exception or an indefinite wait.
 *  - **Expired mid-shift.** The gateway answers 401, `ApiClient` calls `refreshToken`,
 *    and we go back to the native layer for a newer one.
 *  - **The host is not answering.** Bounded (`withHostTimeout`) and reported as no
 *    credential, so the widget degrades to the signed-out view instead of hanging.
 *
 * The mock host returns null from `getToken` deliberately, so browser development gets
 * the same honest "no credentials" path rather than a silently broken history tab.
 */
export function resolveTokenProvider(
  host: SentinelHost,
  options: HostTokenProviderOptions = {},
): HostTokenProvider {
  const callTimeoutMs = options.callTimeoutMs ?? TOKEN_CALL_TIMEOUT_MS;
  const refreshWindowMs = options.refreshWindowMs ?? TOKEN_REFRESH_WINDOW_MS;
  const refreshPollMs = options.refreshPollMs ?? TOKEN_REFRESH_POLL_MS;
  const sleep = options.sleep ?? ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));

  /**
   * The last token handed to the API client, so a forced refresh can tell a genuinely
   * new token from the stale one the gateway just rejected. Deliberately not a cache:
   * it is never returned from `getToken`, only compared against.
   */
  let lastIssued: string | null = null;

  const read = async (): Promise<string | null> => {
    if (typeof host.getToken === 'function') {
      let fromHost: string | null;
      try {
        fromHost = await withHostTimeout(host.getToken(), callTimeoutMs, null);
      } catch {
        // A marshalling failure is not a credential; fall through to the window.
        fromHost = null;
      }
      if (fromHost !== null) return normaliseToken(fromHost);
    }
    return normaliseToken((globalThis as unknown as WebView2Window).__SENTINEL_TOKEN__);
  };

  return {
    async getToken(): Promise<string | null> {
      const token = await read();
      lastIssued = token;
      return token;
    },

    /**
     * Asks the native layer again, and keeps asking for a short window until it offers
     * something other than the token that was just refused.
     *
     * This is best effort on purpose. The agent process owns the credential and
     * refreshes it on its own schedule; the widget cannot make that happen sooner. So
     * the window is a few seconds — long enough to catch a rotation already in flight,
     * short enough that an agent between calls is not left waiting — and when it
     * closes we return null rather than the stale token. Null is what produces a clean
     * `no_credentials` and the signed-out view; handing back the same string would
     * produce the original 401 again, which is the shape of a loop.
     */
    async refreshToken(): Promise<string | null> {
      const rejected = lastIssued;
      const deadline = Date.now() + refreshWindowMs;
      for (;;) {
        const token = await read();
        if (token !== null && token !== rejected) {
          lastIssued = token;
          return token;
        }
        if (Date.now() >= deadline) break;
        await sleep(refreshPollMs);
      }
      // Signed out as far as the API is concerned. The native layer will push a new
      // auth state when it has one, and that is what brings the widget back.
      lastIssued = null;
      return null;
    },
  };
}

function normaliseToken(value: unknown): string | null {
  // A host object that has no token yet can marshal `undefined` across as an empty
  // string. `Authorization: Bearer ` reaches the gateway as a malformed header and
  // comes back 400, which never routes to the sign-in path a 401 would.
  return typeof value === 'string' && value !== '' ? value : null;
}

export async function resolveApiBaseUrl(host: SentinelHost, callTimeoutMs = TOKEN_CALL_TIMEOUT_MS): Promise<string> {
  if (typeof host.getApiBaseUrl === 'function') {
    try {
      // Bounded like every host call: this one runs during boot, and an unbounded
      // wait here means the API client is never constructed, which the history tab
      // renders as a permanent "Loading…".
      const url = await withHostTimeout<string | null>(host.getApiBaseUrl(), callTimeoutMs, null);
      if (typeof url === 'string' && url !== '') return url;
    } catch {
      // Fall through to build-time configuration.
    }
  }
  const injected = (globalThis as unknown as WebView2Window).__SENTINEL_API_BASE_URL__;
  if (injected) return injected;
  const fromEnv = import.meta.env?.['VITE_API_BASE_URL'];
  // The local-development server from contracts/openapi.yaml. No production host is
  // baked into the bundle; the agent supplies it.
  return typeof fromEnv === 'string' && fromEnv !== '' ? fromEnv : 'http://localhost:8080';
}

/* ------------------------------------------------------------ validation */

const AUTH_STATES: readonly HostAuthState[] = ['signed_out', 'signing_in', 'signed_in'];
const CAPTURE_STATES: readonly CaptureState[] = ['IDLE', 'ARMED', 'IN_CALL', 'WRAP', 'FINALIZE', 'BLOCKED'];
const TIERS: readonly CaptureTier[] = ['A', 'B'];

/**
 * Coerces whatever came across the host-object boundary into a HostState.
 *
 * Unknown enum values fail closed: an unrecognised capture state becomes BLOCKED
 * with an `unknown` cause, because the alternative — treating it as IDLE — would
 * hide the recording indicator while capture may still be running.
 */
export function normaliseHostState(raw: unknown): HostState {
  const source = (typeof raw === 'object' && raw !== null ? raw : {}) as Record<string, unknown>;

  const authState = AUTH_STATES.includes(source['authState'] as HostAuthState)
    ? (source['authState'] as HostAuthState)
    : 'signed_out';

  const knownCapture = CAPTURE_STATES.includes(source['captureState'] as CaptureState);
  const captureState = knownCapture ? (source['captureState'] as CaptureState) : 'BLOCKED';

  const error = normaliseError(source['error']) ?? (knownCapture ? null : { cause: 'unknown' as HostErrorCause });

  const coverageRaw = source['coverage'];
  const coverage = typeof coverageRaw === 'number' && Number.isFinite(coverageRaw) ? coverageRaw : null;

  const callIdRaw = source['callId'];
  const callId = typeof callIdRaw === 'string' && callIdRaw !== '' ? callIdRaw : null;

  const displayNameRaw = source['displayName'];

  return {
    authState,
    captureState,
    tier: TIERS.includes(source['tier'] as CaptureTier) ? (source['tier'] as CaptureTier) : 'B',
    coverage,
    callId,
    error,
    pendingCall: normalisePendingCall(source['pendingCall']),
    displayName: typeof displayNameRaw === 'string' ? displayNameRaw : null,
  };
}

function normaliseError(raw: unknown): HostError | null {
  if (typeof raw !== 'object' || raw === null) return null;
  const source = raw as Record<string, unknown>;
  const cause = HOST_ERROR_CAUSES.includes(source['cause'] as HostErrorCause)
    ? (source['cause'] as HostErrorCause)
    : 'unknown';
  const detail = source['detail'];
  return typeof detail === 'string' ? { cause, detail } : { cause };
}

function normalisePendingCall(raw: unknown): HostPendingCall | null {
  if (typeof raw !== 'object' || raw === null) return null;
  const source = raw as Record<string, unknown>;
  const callId = source['callId'];
  if (typeof callId !== 'string' || callId === '') return null;
  const endedAt = typeof source['endedAt'] === 'string' ? source['endedAt'] : new Date().toISOString();
  const epoch = source['endedAtEpochMs'];
  return typeof epoch === 'number'
    ? { callId, endedAt, endedAtEpochMs: epoch }
    : { callId, endedAt };
}
