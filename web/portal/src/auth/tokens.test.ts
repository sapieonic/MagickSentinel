import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { IdTokenCache, expiryOf } from './tokens.js';

const HOUR_MS = 60 * 60 * 1000;
const SKEW_MS = 5 * 60 * 1000;
const START_MS = Date.UTC(2026, 8, 3, 9, 0, 0);

/** A token whose `exp` is `ttlMs` from now. Signature is irrelevant: nothing here verifies. */
function token(label: string, ttlMs: number): string {
  const payload = { exp: Math.floor((Date.now() + ttlMs) / 1000), sub: label };
  const encoded = btoa(JSON.stringify(payload)).replace(/=+$/, '').replace(/\+/g, '-').replace(/\//g, '_');
  return `header.${encoded}.${label}`;
}

describe('expiryOf', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(START_MS);
  });
  afterEach(() => vi.useRealTimers());

  it('reads exp and converts seconds to milliseconds', () => {
    // RFC 7519 says seconds. Treating them as milliseconds would put every expiry in
    // 1970 and refresh on a permanent loop.
    expect(expiryOf(token('a', HOUR_MS), Date.now())).toBe(Math.floor((START_MS + HOUR_MS) / 1000) * 1000);
  });

  it('treats a token it cannot parse as short-lived rather than long-lived', () => {
    // Failing towards "refresh sooner" wastes a request; failing towards "trust it for
    // an hour" carries an expired token into a shift.
    for (const bad of ['', 'not-a-jwt', 'a.b', 'a.!!!.c', `header.${btoa('[]')}.sig`]) {
      const assumed = expiryOf(bad, Date.now());
      expect(assumed).toBeGreaterThan(Date.now());
      expect(assumed).toBeLessThan(Date.now() + HOUR_MS);
    }
  });

  it('ignores a non-numeric or negative exp', () => {
    const weird = btoa(JSON.stringify({ exp: 'soon' })).replace(/=+$/, '');
    expect(expiryOf(`h.${weird}.s`, Date.now())).toBeLessThan(Date.now() + HOUR_MS);
  });
});

describe('IdTokenCache', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(START_MS);
  });
  afterEach(() => vi.useRealTimers());

  it('serves the same token until it nears expiry', async () => {
    const fetcher = vi.fn(async () => token('first', HOUR_MS));
    const cache = new IdTokenCache(fetcher);

    expect(await cache.get()).toContain('first');
    expect(await cache.get()).toContain('first');
    // One mint per hour, not one per request: this provider is called on every API
    // call and every screen fires several.
    expect(fetcher).toHaveBeenCalledTimes(1);
    cache.dispose();
  });

  it('re-mints once the token is inside the skew window', async () => {
    let issued = 0;
    const fetcher = vi.fn(async () => token(`t${++issued}`, HOUR_MS));
    const cache = new IdTokenCache(fetcher);
    expect(await cache.get()).toContain('t1');

    // Just inside the window. The token is still technically valid, but the gateway
    // allows for clock skew in the other direction and a request in flight has to
    // outlive it, so it is treated as spent.
    vi.setSystemTime(START_MS + HOUR_MS - SKEW_MS + 1000);
    expect(await cache.get()).toContain('t2');
    cache.dispose();
  });

  it('refreshes proactively before anything has 401d', async () => {
    let issued = 0;
    const fetcher = vi.fn(async () => token(`t${++issued}`, HOUR_MS));
    const cache = new IdTokenCache(fetcher);
    await cache.get();

    // Nothing due yet.
    await vi.advanceTimersByTimeAsync(HOUR_MS - SKEW_MS - 60_000);
    expect(fetcher).toHaveBeenCalledTimes(1);

    // The whole point of proactive refresh: the token is replaced without a request
    // having to fail first, so the supervisor who returns from a stand-up at 10:05
    // does not pay for it with a screen of errors.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(await cache.get()).toContain('t2');
    cache.dispose();
  });

  it('forces a refresh on demand even when the cached token still looks fresh', async () => {
    let issued = 0;
    const fetcher = vi.fn(async (force: boolean) => `${force ? 'forced' : 'cached'}-${++issued}`);
    const cache = new IdTokenCache(fetcher);
    await cache.get();
    expect(fetcher).toHaveBeenLastCalledWith(false);

    const refreshed = await cache.refresh();
    // The gateway has just refused the cached token; our own opinion of its freshness
    // is worth nothing against Google's signing keys.
    expect(fetcher).toHaveBeenLastCalledWith(true);
    expect(refreshed).toBe('forced-2');
    cache.dispose();
  });

  it('coalesces concurrent reads into one mint', async () => {
    let calls = 0;
    const fetcher = async () => {
      calls += 1;
      await Promise.resolve();
      return token('t', HOUR_MS);
    };
    const cache = new IdTokenCache(fetcher);
    await Promise.all([cache.get(), cache.get(), cache.get()]);
    expect(calls).toBe(1);
    cache.dispose();
  });

  it('never satisfies a forced refresh from an in-flight cached read', async () => {
    const seen: boolean[] = [];
    // A holder rather than a bare `let`: TypeScript narrows a variable assigned only
    // inside a callback back to its initialiser type at the call site.
    const gate = { release: () => undefined as void };
    const fetcher = async (force: boolean) => {
      seen.push(force);
      if (!force) {
        await new Promise<void>((resolve) => {
          gate.release = resolve;
        });
      }
      return force ? 'forced' : 'cached';
    };
    const cache = new IdTokenCache(fetcher);

    const cached = cache.get();
    const forced = cache.refresh();
    gate.release();

    await expect(cached).resolves.toBe('cached');
    // The in-flight non-forced call is allowed to return the very token the gateway
    // refused, so joining it would defeat the retry entirely.
    await expect(forced).resolves.toBe('forced');
    expect(seen).toEqual([false, true]);
    cache.dispose();
  });

  it('returns to signed-out when a refresh fails, and stays there', async () => {
    const lost: string[] = [];
    let attempts = 0;
    const fetcher = async () => {
      attempts += 1;
      throw Object.assign(new Error('nope'), { code: 'auth/user-disabled' });
    };
    const cache = new IdTokenCache(fetcher, { onLost: (reason) => lost.push(reason) });

    expect(await cache.get()).toBeNull();
    expect(cache.lost).toBe(true);
    expect(lost).toEqual(['This account has been disabled.']);

    // Every later read answers null immediately. This is the 401 loop's off switch:
    // ApiClient turns null into a clean no_credentials, and the portal renders the
    // sign-in screen instead of retrying a credential that is gone.
    expect(await cache.get()).toBeNull();
    expect(await cache.refresh()).toBeNull();
    expect(attempts).toBe(1);
    expect(lost).toHaveLength(1);
    cache.dispose();
  });

  it('keeps the provider’s account details out of the reason it renders', async () => {
    const lost: string[] = [];
    const fetcher = async () => {
      throw new Error('FIREBASE: no user record for supervisor@bpo.example');
    };
    const cache = new IdTokenCache(fetcher, { onLost: (reason) => lost.push(reason) });
    await cache.get();
    expect(lost[0]).toBe('Your session could not be renewed.');
    expect(lost[0]).not.toContain('@');
    cache.dispose();
  });

  it('treats an empty token as a failed refresh, not as a credential', async () => {
    const lost: string[] = [];
    const cache = new IdTokenCache(async () => '', { onLost: (reason) => lost.push(reason) });
    // An empty bearer header is a 400 from the gateway, and a 400 never routes to the
    // sign-in path that a 401 does — so it has to be caught here.
    expect(await cache.get()).toBeNull();
    expect(lost).toHaveLength(1);
    cache.dispose();
  });

  it('stops refreshing after a failure rather than hammering the provider', async () => {
    let calls = 0;
    const cache = new IdTokenCache(async () => {
      calls += 1;
      throw Object.assign(new Error('offline'), { code: 'auth/network-request-failed' });
    });
    await cache.get();
    await vi.advanceTimersByTimeAsync(4 * HOUR_MS);
    expect(calls).toBe(1);
    cache.dispose();
  });

  it('does not spin when the provider hands back an already-stale token', async () => {
    // A desktop with a badly wrong clock reads every token as inside the skew window.
    // Without a floor on the timer this schedules a 0 ms refresh and turns a clock
    // problem into a request storm against Identity Platform.
    let calls = 0;
    const cache = new IdTokenCache(async () => {
      calls += 1;
      return token(`t${calls}`, 60_000);
    });
    await cache.get();
    expect(calls).toBe(1);
    await vi.advanceTimersByTimeAsync(29_000);
    expect(calls).toBe(1);
    await vi.advanceTimersByTimeAsync(2000);
    expect(calls).toBe(2);
    cache.dispose();
  });

  it('cancels the refresh timer on dispose', async () => {
    let calls = 0;
    const cache = new IdTokenCache(async () => {
      calls += 1;
      return token('t', HOUR_MS);
    });
    await cache.get();
    cache.dispose();

    await vi.advanceTimersByTimeAsync(4 * HOUR_MS);
    // A timer surviving dispose means the portal keeps minting tokens for the user who
    // just signed out, on a workstation the next supervisor is about to sit down at.
    expect(calls).toBe(1);
    expect(await cache.get()).toBeNull();
  });

  it('answers null after dispose without calling the provider again', async () => {
    const fetcher = vi.fn(async () => token('t', HOUR_MS));
    const cache = new IdTokenCache(fetcher);
    cache.dispose();
    expect(await cache.get()).toBeNull();
    expect(await cache.refresh()).toBeNull();
    expect(fetcher).not.toHaveBeenCalled();
  });

  it('does not report a loss for a fetch that lands after dispose', async () => {
    const lost: string[] = [];
    const gate = { release: () => undefined as void };
    const cache = new IdTokenCache(
      async () => {
        await new Promise<void>((resolve) => {
          gate.release = resolve;
        });
        throw new Error('too late');
      },
      { onLost: (reason) => lost.push(reason) },
    );
    const pending = cache.get();
    cache.dispose();
    gate.release();
    expect(await pending).toBeNull();
    // Signing out and then being told the session was lost is a confusing sequence to
    // render, and the reducer would keep the message on the sign-in screen.
    expect(lost).toEqual([]);
  });
});
