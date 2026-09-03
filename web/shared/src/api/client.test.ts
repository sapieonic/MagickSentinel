import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiClient, ApiError, MISSING_CREDENTIALS } from './client.js';

function jsonResponse(status: number, body: unknown, init: ResponseInit = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
    ...init,
  });
}

function client(fetchImpl: typeof globalThis.fetch, token: string | null = 'tok') {
  return new ApiClient({ baseUrl: 'https://api.example/', getToken: () => token, fetch: fetchImpl });
}

describe('ApiClient transport', () => {
  let calls: Array<{ url: string; init: RequestInit }>;
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    calls = [];
    fetchMock = vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
      calls.push({ url: String(url), init: init ?? {} });
      return jsonResponse(200, { items: [] });
    });
  });

  it('strips the trailing slash from the base URL so paths do not double up', async () => {
    await client(fetchMock as unknown as typeof fetch).listMyCalls();
    expect(calls[0]!.url).toBe('https://api.example/v1/me/calls');
  });

  it('sends the bearer token from the provider on every request', async () => {
    let issued = 0;
    const rotating = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => `tok-${++issued}`,
      fetch: fetchMock as unknown as typeof fetch,
    });
    await rotating.listMyCalls();
    await rotating.listMyCalls();
    const headers = calls.map((c) => (c.init.headers as Record<string, string>)['Authorization']);
    // Re-read per request: the native layer rotates the injected token mid-shift and
    // a client that cached it would 401 the moment it expired.
    expect(headers).toEqual(['Bearer tok-1', 'Bearer tok-2']);
  });

  it('drops empty, null and undefined query parameters', async () => {
    await client(fetchMock as unknown as typeof fetch).listMyCalls({
      from: '2026-01-01T00:00:00Z',
      to: undefined,
      q: '',
      limit: 20,
    });
    expect(calls[0]!.url).toBe('https://api.example/v1/me/calls?from=2026-01-01T00%3A00%3A00Z&limit=20');
  });

  it('does not attach a token to the unauthenticated operations', async () => {
    const anon = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => {
        throw new Error('token provider must not be called for anonymous routes');
      },
      fetch: fetchMock as unknown as typeof fetch,
    });
    await anon.health();
    expect((calls[0]!.init.headers as Record<string, string>)['Authorization']).toBeUndefined();
  });

  it('fails before the round trip when no token is available', async () => {
    const noToken = client(fetchMock as unknown as typeof fetch, null);
    await expect(noToken.listMyCalls()).rejects.toMatchObject({ code: 'no_credentials', status: 0 });
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('returns undefined for 204 rather than trying to parse a body', async () => {
    const noContent = vi.fn(async () => new Response(null, { status: 204 }));
    await expect(client(noContent as unknown as typeof fetch).endSession()).resolves.toBeUndefined();
  });
});

describe('ApiClient error handling', () => {
  it('surfaces the contract error shape including request_id', async () => {
    const failing = vi.fn(async () =>
      jsonResponse(403, { code: 'forbidden', message: 'role not permitted', request_id: 'req-42' }),
    );
    const error = await client(failing as unknown as typeof fetch)
      .listFlags()
      .catch((e: unknown) => e);

    expect(error).toBeInstanceOf(ApiError);
    const api = error as ApiError;
    expect(api.code).toBe('forbidden');
    expect(api.message).toBe('role not permitted');
    expect(api.requestId).toBe('req-42');
    expect(api.status).toBe(403);
    expect(api.isForbidden).toBe(true);
    expect(api.isTransport).toBe(false);
  });

  it('synthesises an error for documented bodyless failures', async () => {
    // POST /v1/me/calls/{id}/confirm 409 has no schema: the window simply closed.
    const conflict = vi.fn(async () => new Response(null, { status: 409 }));
    const error = (await client(conflict as unknown as typeof fetch)
      .confirmMyCall('call-1', { disposition: 'ptp' })
      .catch((e: unknown) => e)) as ApiError;

    expect(error.status).toBe(409);
    expect(error.code).toBe('conflict');
    expect(error.isConflict).toBe(true);
    expect(error.message).not.toBe('');
    expect(error.requestId).toBeUndefined();
  });

  it('does not mistake a proxy HTML page for the contract error shape', async () => {
    const html = vi.fn(async () => new Response('<html>502 Bad Gateway</html>', { status: 502 }));
    const error = (await client(html as unknown as typeof fetch)
      .getMyStats()
      .catch((e: unknown) => e)) as ApiError;
    expect(error.code).toBe('server_error');
    expect(error.message).not.toContain('<html>');
  });

  it('ignores a JSON body that is not the error shape', async () => {
    const odd = vi.fn(async () => jsonResponse(400, { error: 'nope' }));
    const error = (await client(odd as unknown as typeof fetch)
      .getMyStats()
      .catch((e: unknown) => e)) as ApiError;
    expect(error.code).toBe('bad_request');
  });

  it('classifies a transport failure as status 0, distinct from a 5xx', async () => {
    const offline = vi.fn(async () => {
      throw new TypeError('Failed to fetch');
    });
    const error = (await client(offline as unknown as typeof fetch)
      .getPolicy()
      .catch((e: unknown) => e)) as ApiError;
    expect(error.status).toBe(0);
    expect(error.code).toBe('network_error');
    expect(error.isTransport).toBe(true);
    expect(error.isAuthFailure).toBe(false);
  });

  it('reports an aborted request as a timeout, not a network error', async () => {
    const aborting = vi.fn(async () => {
      const err = new Error('aborted');
      err.name = 'TimeoutError';
      throw err;
    });
    const error = (await client(aborting as unknown as typeof fetch)
      .getPolicy()
      .catch((e: unknown) => e)) as ApiError;
    expect(error.code).toBe('timeout');
  });

  it('keeps borrower data out of the thrown message on transport failure', async () => {
    const offline = vi.fn(async () => {
      throw new TypeError('Failed to fetch');
    });
    const error = (await client(offline as unknown as typeof fetch)
      .confirmMyCall('call-1', { disposition: 'ptp', ptp_amount_paise: 150000, note: 'borrower said X' })
      .catch((e: unknown) => e)) as ApiError;
    expect(error.message).not.toContain('borrower');
    expect(error.message).not.toContain('150000');
  });

  it('rejects a 200 whose body is not JSON', async () => {
    const garbage = vi.fn(async () => new Response('not json', { status: 200 }));
    const error = (await client(garbage as unknown as typeof fetch)
      .getMyStats()
      .catch((e: unknown) => e)) as ApiError;
    expect(error.code).toBe('malformed_response');
  });
});

describe('ApiClient request shapes', () => {
  it('sends the confirmation body verbatim, paise untouched', async () => {
    let sent: unknown;
    const spy = vi.fn(async (_url: string | URL | Request, init?: RequestInit) => {
      sent = JSON.parse(String(init?.body));
      return jsonResponse(200, { id: 'call-1', started_at: '', capture_tier: 'A', status: 'complete' });
    });
    await client(spy as unknown as typeof fetch).confirmMyCall('call-1', {
      disposition: 'ptp',
      ptp_present: true,
      ptp_amount_paise: 150050,
      ptp_due_date: '2026-09-15',
    });
    expect(sent).toEqual({
      disposition: 'ptp',
      ptp_present: true,
      ptp_amount_paise: 150050,
      ptp_due_date: '2026-09-15',
    });
  });

  it('percent-encodes ids into the path', async () => {
    const spy = vi.fn(async (url: string | URL | Request) => jsonResponse(200, { id: String(url) }));
    await client(spy as unknown as typeof fetch).getMyCall('a/b?c');
    expect(spy.mock.calls[0]![0]).toBe('https://api.example/v1/me/calls/a%2Fb%3Fc');
  });

  it('builds the live stream URL with the ticket and no token', () => {
    const url = client(vi.fn() as unknown as typeof fetch).teamLiveUrl('team-1', 'tick et/1');
    expect(url).toBe('https://api.example/v1/teams/team-1/live?ticket=tick%20et%2F1');
    // The whole point of the ticket exchange: the bearer token never reaches a
    // query string, where it would outlive the request in logs and caches.
    expect(url).not.toContain('tok');
  });
});

describe('ApiClient scope-derived call listing', () => {
  let seen: string[];
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    seen = [];
    fetchMock = vi.fn(async (url: string | URL | Request) => {
      seen.push(String(url));
      return jsonResponse(200, { items: [] });
    });
  });

  it('sends only the filters that were set', async () => {
    await client(fetchMock as unknown as typeof fetch).listCalls({
      from: '2026-09-01T00:00:00Z',
      team_id: 'team-7',
      limit: 200,
    });
    expect(seen[0]).toBe('https://api.example/v1/calls?from=2026-09-01T00%3A00%3A00Z&team_id=team-7&limit=200');
  });

  it('keeps has_flags=false, which is a filter and not an absent one', async () => {
    // 'no flags' and 'any' are different questions; dropping false would silently
    // turn one into the other.
    await client(fetchMock as unknown as typeof fetch).listCalls({ has_flags: false });
    expect(seen[0]).toBe('https://api.example/v1/calls?has_flags=false');
    await client(fetchMock as unknown as typeof fetch).listCalls({ has_flags: true });
    expect(seen[1]).toBe('https://api.example/v1/calls?has_flags=true');
  });

  it('reports a call outside the caller\u2019s scope as not found, never as forbidden', async () => {
    const missing = vi.fn(async () => jsonResponse(404, { code: 'not_found', message: 'no such call' }));
    const error = (await client(missing as unknown as typeof fetch)
      .getCall('c-1')
      .catch((e: unknown) => e)) as ApiError;
    expect(error.isNotFound).toBe(true);
    expect(error.isForbidden).toBe(false);
  });

  it('percent-encodes a call id into the detail path', async () => {
    await client(fetchMock as unknown as typeof fetch).getCall('a/b');
    expect(seen[0]).toBe('https://api.example/v1/calls/a%2Fb');
  });

  it('surfaces a failed team listing rather than pretending there are no teams', async () => {
    const failing = vi.fn(async () => jsonResponse(500, { code: 'internal', message: 'boom' }));
    const error = (await client(failing as unknown as typeof fetch)
      .listTeams()
      .catch((e: unknown) => e)) as ApiError;
    expect(error).toBeInstanceOf(ApiError);
    expect(error.status).toBe(500);
    expect(error.code).toBe('internal');
  });
});

describe('ApiClient live ticket', () => {
  it('POSTs to the team ticket route and returns the ticket', async () => {
    let method = '';
    const minting = vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
      method = String(init?.method);
      expect(String(url)).toBe('https://api.example/v1/teams/team-1/live/ticket');
      return jsonResponse(201, { ticket: 'abc', expires_at: '2026-09-01T00:01:00Z' });
    });
    const ticket = await client(minting as unknown as typeof fetch).createLiveTicket('team-1');
    expect(method).toBe('POST');
    expect(ticket).toEqual({ ticket: 'abc', expires_at: '2026-09-01T00:01:00Z' });
  });

  it('reports a refused mint as forbidden so the caller can stop retrying', async () => {
    const refused = vi.fn(async () => jsonResponse(403, { code: 'forbidden', message: 'role not permitted' }));
    const error = (await client(refused as unknown as typeof fetch)
      .createLiveTicket('team-1')
      .catch((e: unknown) => e)) as ApiError;
    expect(error.isForbidden).toBe(true);
  });

  it('reports an unreachable gateway as transport, which is worth retrying', async () => {
    const offline = vi.fn(async () => {
      throw new TypeError('Failed to fetch');
    });
    const error = (await client(offline as unknown as typeof fetch)
      .createLiveTicket('team-1')
      .catch((e: unknown) => e)) as ApiError;
    expect(error.isTransport).toBe(true);
    expect(error.isForbidden).toBe(false);
  });
});

describe('ApiClient credential refresh on 401', () => {
  /**
   * Builds a fetch that answers 401 until it is handed `goodToken`, then 200. This is
   * the shape of the real failure: the token in hand was minted an hour ago, the
   * gateway is right to refuse it, and the only thing wrong is our copy.
   */
  function gatewayExpecting(goodToken: string) {
    const seen: Array<string | undefined> = [];
    const fetchMock = vi.fn(async (_url: string | URL | Request, init?: RequestInit) => {
      const auth = (init?.headers as Record<string, string> | undefined)?.['Authorization'];
      seen.push(auth);
      return auth === `Bearer ${goodToken}`
        ? jsonResponse(200, { items: [] })
        : jsonResponse(401, { code: 'unauthorized', message: 'token expired' });
    });
    return { seen, fetchMock };
  }

  it('forces a refresh and retries once when the gateway rejects a stale token', async () => {
    const { seen, fetchMock } = gatewayExpecting('fresh');
    const refresh = vi.fn(async () => 'fresh');
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      refreshToken: refresh,
      fetch: fetchMock as unknown as typeof fetch,
    });

    await expect(api.listMyCalls()).resolves.toEqual({ items: [] });
    expect(seen).toEqual(['Bearer stale', 'Bearer fresh']);
    expect(refresh).toHaveBeenCalledTimes(1);
  });

  it('does not retry when no refresher was supplied', async () => {
    const { fetchMock } = gatewayExpecting('fresh');
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      fetch: fetchMock as unknown as typeof fetch,
    });
    const error = (await api.listMyCalls().catch((e: unknown) => e)) as ApiError;
    expect(error.status).toBe(401);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('reports a failed refresh as no_credentials, never as another 401', async () => {
    // The whole point: the caller must be able to tell "sign in again" from
    // "retry", because retrying a dead session is the 401 loop.
    const { fetchMock } = gatewayExpecting('never-issued');
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      refreshToken: async () => null,
      fetch: fetchMock as unknown as typeof fetch,
    });
    const error = (await api.getMyStats().catch((e: unknown) => e)) as ApiError;
    expect(error.code).toBe(MISSING_CREDENTIALS);
    expect(error.isMissingCredentials).toBe(true);
    expect(error.isAuthFailure).toBe(false);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('treats a throwing refresher as a failed refresh rather than propagating it', async () => {
    const { fetchMock } = gatewayExpecting('never-issued');
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      refreshToken: () => {
        throw new Error('identity platform unreachable');
      },
      fetch: fetchMock as unknown as typeof fetch,
    });
    const error = (await api.getMyStats().catch((e: unknown) => e)) as ApiError;
    expect(error).toBeInstanceOf(ApiError);
    expect(error.code).toBe(MISSING_CREDENTIALS);
  });

  it('folds an empty-string refresh into a failed one', async () => {
    const { fetchMock } = gatewayExpecting('never-issued');
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      refreshToken: async () => '',
      fetch: fetchMock as unknown as typeof fetch,
    });
    const error = (await api.getMyStats().catch((e: unknown) => e)) as ApiError;
    expect(error.code).toBe(MISSING_CREDENTIALS);
  });

  it('gives up rather than retrying when the refresher hands back the rejected token', async () => {
    const { fetchMock } = gatewayExpecting('never-issued');
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      refreshToken: async () => 'stale',
      fetch: fetchMock as unknown as typeof fetch,
    });
    const error = (await api.getMyStats().catch((e: unknown) => e)) as ApiError;
    // The original 401 survives: a cache-serving refresher is a bug in the
    // refresher, and pretending the session is gone would hide it.
    expect(error.status).toBe(401);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('retries exactly once, so a gateway that refuses the user does not loop', async () => {
    // Every token is refused. This is a suspended user row or a tenant mismatch,
    // not an expiry, and it must surface as a 401 after one retry.
    const fetchMock = vi.fn(async () => jsonResponse(401, { code: 'unauthorized', message: 'suspended' }));
    let issued = 0;
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      refreshToken: async () => `fresh-${++issued}`,
      fetch: fetchMock as unknown as typeof fetch,
    });
    const error = (await api.listMyFlags().catch((e: unknown) => e)) as ApiError;
    expect(error.status).toBe(401);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(issued).toBe(1);
  });

  it('coalesces concurrent 401s into a single refresh', async () => {
    const { fetchMock } = gatewayExpecting('fresh');
    let refreshes = 0;
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      refreshToken: async () => {
        refreshes += 1;
        // Yield twice so all three requests are certain to be waiting on this one.
        await Promise.resolve();
        await Promise.resolve();
        return 'fresh';
      },
      fetch: fetchMock as unknown as typeof fetch,
    });

    await Promise.all([api.listMyCalls(), api.listMyFlags(), api.getMyStats()]);
    expect(refreshes).toBe(1);
  });

  it('refreshes again on a later expiry rather than latching after the first', async () => {
    const { fetchMock } = gatewayExpecting('fresh');
    let refreshes = 0;
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      refreshToken: async () => {
        refreshes += 1;
        return 'fresh';
      },
      fetch: fetchMock as unknown as typeof fetch,
    });
    await api.listMyCalls();
    await api.listMyCalls();
    // A shift is eight hours long; the in-flight coalescing must not become a
    // once-per-client latch or the second expiry would go unhandled.
    expect(refreshes).toBe(2);
  });

  it('never refreshes for an unauthenticated operation', async () => {
    const unauthorised = vi.fn(async () => jsonResponse(401, { code: 'unauthorized', message: 'bad token' }));
    const refresh = vi.fn(async () => 'fresh');
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      refreshToken: refresh,
      fetch: unauthorised as unknown as typeof fetch,
    });
    // POST /v1/devices/enroll answers 401 for a bad enrollment token, which no ID
    // token refresh can fix.
    const error = (await api
      .enrollDevice({
        enrollment_token: 'nope',
        csr_pem: '',
        machine_guid: 'guid',
        hw_fingerprint: 'fp',
        os_build: '10.0.22631',
        capture_tier: 'A',
        agent_version: '0.1.0',
      })
      .catch((e: unknown) => e)) as ApiError;
    expect(error.status).toBe(401);
    expect(refresh).not.toHaveBeenCalled();
  });

  it('does not refresh on 403, which a new token cannot fix', async () => {
    const forbidden = vi.fn(async () => jsonResponse(403, { code: 'forbidden', message: 'role not permitted' }));
    const refresh = vi.fn(async () => 'fresh');
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      refreshToken: refresh,
      fetch: forbidden as unknown as typeof fetch,
    });
    await expect(api.listFlags()).rejects.toMatchObject({ status: 403 });
    expect(refresh).not.toHaveBeenCalled();
  });

  it('replays the request body on the retry', async () => {
    const bodies: unknown[] = [];
    const fetchMock = vi.fn(async (_url: string | URL | Request, init?: RequestInit) => {
      const auth = (init?.headers as Record<string, string> | undefined)?.['Authorization'];
      bodies.push(JSON.parse(String(init?.body)));
      return auth === 'Bearer fresh'
        ? jsonResponse(200, { id: 'call-1', started_at: '', capture_tier: 'A', status: 'complete' })
        : jsonResponse(401, { code: 'unauthorized', message: 'token expired' });
    });
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => 'stale',
      refreshToken: async () => 'fresh',
      fetch: fetchMock as unknown as typeof fetch,
    });
    await api.confirmMyCall('call-1', { disposition: 'ptp', ptp_amount_paise: 150050 });
    // A dropped body on the retry would silently confirm the call with no
    // disposition — worse than failing, because it looks like it worked.
    expect(bodies).toEqual([
      { disposition: 'ptp', ptp_amount_paise: 150050 },
      { disposition: 'ptp', ptp_amount_paise: 150050 },
    ]);
  });
});

describe('ApiClient CORS and origin handling', () => {
  it('sends no cookies, so the gateway never needs the credentialed CORS mode', async () => {
    let init: RequestInit | undefined;
    const spy = vi.fn(async (_url: string | URL | Request, requestInit?: RequestInit) => {
      init = requestInit;
      return jsonResponse(200, { items: [] });
    });
    await client(spy as unknown as typeof fetch).listMyCalls();
    expect(init?.credentials).toBe('omit');
    expect(init?.mode).toBe('cors');
  });

  it('names the configured gateway in a transport failure so a CORS block is diagnosable', async () => {
    // A browser cannot tell us "blocked by CORS" — it reports a bare TypeError. The
    // only useful thing we can add is which origin we were trying to reach.
    const blocked = vi.fn(async () => {
      throw new TypeError('Failed to fetch');
    });
    const api = new ApiClient({
      baseUrl: 'https://api.sentinel.example/',
      getToken: () => 'tok',
      fetch: blocked as unknown as typeof fetch,
    });
    const error = (await api.listTeams().catch((e: unknown) => e)) as ApiError;
    expect(error.isTransport).toBe(true);
    expect(error.message).toContain('https://api.sentinel.example');
    expect(error.message).toContain('origin');
  });

  it('exposes the normalised base URL for error copy', () => {
    const api = new ApiClient({ baseUrl: 'https://api.example//', getToken: () => 'tok' });
    expect(api.baseUrl).toBe('https://api.example');
  });

  it('keeps a missing credential out of the transport bucket', async () => {
    const never = vi.fn(async () => jsonResponse(200, {}));
    const api = new ApiClient({
      baseUrl: 'https://api.example',
      getToken: () => null,
      fetch: never as unknown as typeof fetch,
    });
    const error = (await api.listMyCalls().catch((e: unknown) => e)) as ApiError;
    expect(error.isMissingCredentials).toBe(true);
    // "Sign in" and "check your connection" are different instructions; a signed-out
    // portal reported as an outage sends the user to the wrong person for help.
    expect(error.isTransport).toBe(false);
  });
});
