import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { MockSentinelHost } from './mock.js';
import { resolveApiBaseUrl, resolveHost, resolveTokenProvider, withHostTimeout } from './bridge.js';
import type { SentinelHost } from './types.js';

/**
 * A host object with only the members a test needs. The real one is a WebView2 proxy
 * that marshals every member asynchronously, which is why partial hosts are a case
 * worth covering: an older agent build genuinely does not implement `getToken`.
 */
function host(partial: Partial<SentinelHost>): SentinelHost {
  return {
    getState: async () => {
      throw new Error('not used in this test');
    },
    signIn: async () => undefined,
    signOut: async () => undefined,
    onStateChange: () => undefined,
    confirmCall: async () => undefined,
    openPortal: async () => undefined,
    ...partial,
  };
}

/** A promise that never settles: what a wedged agent process returns. */
function neverSettles<T>(): Promise<T> {
  return new Promise<T>(() => undefined);
}

type Injected = {
  __SENTINEL_TOKEN__?: string;
  __SENTINEL_API_BASE_URL__?: string;
};

function injected(): Injected {
  return globalThis as unknown as Injected;
}

afterEach(() => {
  delete injected().__SENTINEL_TOKEN__;
  delete injected().__SENTINEL_API_BASE_URL__;
  delete (globalThis as { chrome?: unknown }).chrome;
});

describe('withHostTimeout', () => {
  it('returns the host’s answer when it arrives', async () => {
    await expect(withHostTimeout(Promise.resolve('tok'), 1000, null)).resolves.toBe('tok');
  });

  it('returns the fallback when the host never answers', async () => {
    // The failure this exists for: a marshalled call to a wedged agent process does
    // not reject, it simply never settles, so try/catch never fires and the widget
    // sits on "Starting Sentinel" until someone restarts it.
    await expect(withHostTimeout(neverSettles<string>(), 5, null)).resolves.toBeNull();
  });

  it('passes a synchronous value straight through', async () => {
    // `onStateChange` is documented as possibly returning a plain function rather than
    // a promise, so not everything crossing this boundary is thenable.
    await expect(withHostTimeout('already-here', 5, null)).resolves.toBe('already-here');
  });

  it('still propagates a rejection, which callers handle as no answer', async () => {
    await expect(withHostTimeout(Promise.reject(new Error('marshalling failed')), 1000, null)).rejects.toThrow(
      'marshalling failed',
    );
  });
});

describe('resolveTokenProvider', () => {
  it('prefers the host object’s token', async () => {
    const tokens = resolveTokenProvider(host({ getToken: async () => 'from-host' }));
    expect(await tokens.getToken()).toBe('from-host');
  });

  it('answers null when nobody is signed in, rather than throwing', async () => {
    // The widget starts before the agent signs anyone in, and keeps running after
    // sign-out. Null becomes a clean `no_credentials` inside ApiClient, which is what
    // renders as "sign in" rather than as a broken panel.
    const tokens = resolveTokenProvider(host({ getToken: async () => null }));
    expect(await tokens.getToken()).toBeNull();
  });

  it('accepts a token injected on the window instead of a host method', async () => {
    // Spec 6.7 says the native layer injects a token and does not say how, so both
    // shapes have to work; an older agent build implements only this one.
    injected().__SENTINEL_TOKEN__ = 'from-window';
    const tokens = resolveTokenProvider(host({}));
    expect(await tokens.getToken()).toBe('from-window');
  });

  it('falls back to the window when the host method throws', async () => {
    injected().__SENTINEL_TOKEN__ = 'from-window';
    const tokens = resolveTokenProvider(
      host({
        getToken: async () => {
          throw new Error('host object disconnected');
        },
      }),
    );
    expect(await tokens.getToken()).toBe('from-window');
  });

  it('falls back to the window when the host method never answers', async () => {
    injected().__SENTINEL_TOKEN__ = 'from-window';
    const tokens = resolveTokenProvider(host({ getToken: () => neverSettles<string | null>() }), {
      callTimeoutMs: 5,
    });
    // An agent on a call must never wait on the widget. One bounded attempt, then the
    // next best source.
    expect(await tokens.getToken()).toBe('from-window');
  });

  it('degrades to no credential when the host is wedged and nothing is injected', async () => {
    const tokens = resolveTokenProvider(host({ getToken: () => neverSettles<string | null>() }), {
      callTimeoutMs: 5,
    });
    expect(await tokens.getToken()).toBeNull();
  });

  it('treats an empty token as no token', async () => {
    // A host object with no token yet can marshal `undefined` across as ''. Sending
    // `Authorization: Bearer ` produces a 400, and a 400 never routes to the sign-in
    // path a 401 would.
    const tokens = resolveTokenProvider(host({ getToken: async () => '' }));
    expect(await tokens.getToken()).toBeNull();
  });

  it('picks up a token that only arrives later in the shift', async () => {
    // The widget is launched by the service before anyone signs in, so the first reads
    // legitimately come back empty. Re-reading per request is what makes a sign-in ten
    // minutes later work with no reload.
    let current: string | null = null;
    const tokens = resolveTokenProvider(host({ getToken: async () => current }));
    expect(await tokens.getToken()).toBeNull();
    current = 'signed-in-now';
    expect(await tokens.getToken()).toBe('signed-in-now');
  });

  it('does not cache, so a rotated token is used on the next request', async () => {
    let issued = 0;
    const tokens = resolveTokenProvider(host({ getToken: async () => `tok-${++issued}` }));
    expect(await tokens.getToken()).toBe('tok-1');
    expect(await tokens.getToken()).toBe('tok-2');
  });

  it('keeps the mock host’s honest no-credentials behaviour', async () => {
    // Browser development must get the same "no credentials" path as a signed-out
    // agent, not a silently broken history tab.
    const tokens = resolveTokenProvider(new MockSentinelHost());
    expect(await tokens.getToken()).toBeNull();
  });
});

describe('resolveTokenProvider refresh', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(Date.UTC(2026, 8, 3, 14, 0, 0));
  });
  afterEach(() => vi.useRealTimers());

  /** Advances the clock instead of waiting, so the poll window is deterministic. */
  const sleep = async (ms: number) => {
    await vi.advanceTimersByTimeAsync(ms);
  };

  it('returns the rotated token when the native layer already has a newer one', async () => {
    let current = 'stale';
    const tokens = resolveTokenProvider(host({ getToken: async () => current }), { sleep });
    expect(await tokens.getToken()).toBe('stale');
    current = 'rotated';
    expect(await tokens.refreshToken()).toBe('rotated');
  });

  it('waits briefly for a rotation that is already in flight', async () => {
    // The agent process owns the credential and rotates it on its own schedule. A
    // short window catches a rotation that is milliseconds away; it does not try to
    // make one happen.
    let reads = 0;
    const tokens = resolveTokenProvider(
      host({
        getToken: async () => {
          reads += 1;
          return reads < 4 ? 'stale' : 'rotated';
        },
      }),
      { sleep, refreshWindowMs: 3000, refreshPollMs: 300 },
    );
    await tokens.getToken();
    expect(await tokens.refreshToken()).toBe('rotated');
  });

  it('gives up rather than handing back the token the gateway just refused', async () => {
    let reads = 0;
    const tokens = resolveTokenProvider(
      host({
        getToken: async () => {
          reads += 1;
          return 'stale';
        },
      }),
      { sleep, refreshWindowMs: 900, refreshPollMs: 300 },
    );
    await tokens.getToken();
    // Null, not 'stale'. Returning the stale token would produce the same 401 on the
    // retry, which is the shape of a loop; null produces a clean no_credentials and
    // the signed-out view.
    expect(await tokens.refreshToken()).toBeNull();
    expect(reads).toBeGreaterThan(1);
  });

  it('reports no credential immediately when the agent is signed out', async () => {
    const tokens = resolveTokenProvider(host({ getToken: async () => null }), {
      sleep,
      refreshWindowMs: 900,
      refreshPollMs: 300,
    });
    await tokens.getToken();
    expect(await tokens.refreshToken()).toBeNull();
  });

  it('accepts any token when the first request 401d before one was ever issued', async () => {
    // Enrollment-time ordering: a request can be attempted before the widget has read
    // a token at all, so the refresher has nothing to compare against and must not
    // reject a perfectly good token for being "unchanged" from null.
    const tokens = resolveTokenProvider(host({ getToken: async () => 'first' }), { sleep });
    expect(await tokens.refreshToken()).toBe('first');
  });
});

describe('resolveTokenProvider refresh against an unresponsive host', () => {
  // Real timers here on purpose: the point of this case is that the *internal*
  // deadline in `withHostTimeout` fires, and a faked clock that only moves when the
  // injected sleep runs would deadlock before ever reaching the sleep.
  it('reports no credential instead of hanging the retry', async () => {
    const tokens = resolveTokenProvider(host({ getToken: () => neverSettles<string | null>() }), {
      callTimeoutMs: 5,
      refreshWindowMs: 20,
      refreshPollMs: 5,
    });
    await expect(tokens.refreshToken()).resolves.toBeNull();
  });
});

describe('resolveApiBaseUrl', () => {
  it('prefers the gateway the agent names', async () => {
    // The bundle ships in an MSI and is not rebuilt per environment, so the native
    // layer is the authority on which gateway this desktop talks to.
    await expect(resolveApiBaseUrl(host({ getApiBaseUrl: async () => 'https://gw.example' }))).resolves.toBe(
      'https://gw.example',
    );
  });

  it('accepts an injected base URL from an older agent build', async () => {
    injected().__SENTINEL_API_BASE_URL__ = 'https://injected.example';
    await expect(resolveApiBaseUrl(host({}))).resolves.toBe('https://injected.example');
  });

  it('ignores an empty answer from the host', async () => {
    injected().__SENTINEL_API_BASE_URL__ = 'https://injected.example';
    await expect(resolveApiBaseUrl(host({ getApiBaseUrl: async () => '' }))).resolves.toBe('https://injected.example');
  });

  it('falls back to the development gateway rather than never constructing a client', async () => {
    // This runs during boot. An unbounded wait here means the API client is never
    // built, which the history tab renders as a permanent "Loading…".
    await expect(resolveApiBaseUrl(host({ getApiBaseUrl: () => neverSettles<string>() }), 5)).resolves.toBe(
      'http://localhost:8080',
    );
  });

  it('falls back when the host method throws', async () => {
    await expect(
      resolveApiBaseUrl(
        host({
          getApiBaseUrl: async () => {
            throw new Error('host object disconnected');
          },
        }),
      ),
    ).resolves.toBe('http://localhost:8080');
  });
});

describe('resolveHost', () => {
  it('uses the mock when no native host object appears', async () => {
    const resolved = await resolveHost(0);
    expect(resolved.native).toBe(false);
    // The dev banner is driven off this flag, so a real widget must never boot into
    // developer mode silently.
    expect(resolved.host).toBeInstanceOf(MockSentinelHost);
  });

  it('uses the native host object as soon as WebView2 has injected it', async () => {
    const native = host({ getToken: async () => 'from-host' });
    (globalThis as { chrome?: unknown }).chrome = { webview: { hostObjects: { sentinel: native } } };
    const resolved = await resolveHost(0);
    expect(resolved).toEqual({ host: native, native: true });
  });
});
