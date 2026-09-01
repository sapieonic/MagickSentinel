/**
 * Typed REST client for the Sentinel gateway.
 *
 * Every method below corresponds to exactly one operation in
 * `contracts/openapi.yaml`. Nothing here may invent a route: if a screen needs data
 * the contract does not expose, the contract changes first.
 *
 * Both surfaces share this client but not their credentials — the widget is handed a
 * token by the native layer, the portal mints one from its own Identity Platform
 * session — so the token arrives through a provider function rather than being
 * stored here. The provider is called per request so a refreshed token is picked up
 * without rebuilding the client.
 */
import type {
  AgentStats,
  ApiErrorBody,
  AuditPage,
  CallConfirmation,
  CallDetail,
  CallPage,
  DevicePage,
  DeviceStatusFilter,
  Disposition,
  EnrollRequest,
  EnrollResponse,
  EnrollmentToken,
  EvidenceExportJob,
  EvidenceExportRequest,
  Flag,
  FlagPage,
  FlagStatus,
  FlagUpdate,
  HealthResponse,
  Heartbeat,
  HeartbeatResponse,
  LiveTicket,
  Policy,
  RuleSet,
  RuleSetDefinition,
  SessionResponse,
  Severity,
  Team,
  User,
  UserUpdate,
} from './types.js';

export type TokenProvider = () => string | null | undefined | Promise<string | null | undefined>;

/**
 * Rejected value for every failed call. Carries the contract's `{code, message,
 * request_id}` when the gateway sent one, and a synthesised code when it did not
 * (several documented responses — 401 on enroll, 403 on session, 409 on confirm —
 * have no body at all, and transport failures never reach the gateway).
 */
export class ApiError extends Error {
  readonly code: string;
  readonly status: number;
  readonly requestId: string | undefined;

  constructor(status: number, body: ApiErrorBody) {
    super(body.message);
    this.name = 'ApiError';
    this.code = body.code;
    this.status = status;
    this.requestId = body.request_id;
  }

  /** True for the two statuses a caller should react to by re-authenticating. */
  get isAuthFailure(): boolean {
    return this.status === 401;
  }

  get isForbidden(): boolean {
    return this.status === 403;
  }

  get isNotFound(): boolean {
    return this.status === 404;
  }

  /** Correction window closed / concurrent edit. */
  get isConflict(): boolean {
    return this.status === 409;
  }

  /** No response reached us: offline, DNS, TLS, aborted. Retrying may work. */
  get isTransport(): boolean {
    return this.status === 0;
  }
}

export interface ApiClientOptions {
  /** Gateway origin, e.g. `https://api.sentinel.magickvoice.com`. No trailing slash needed. */
  baseUrl: string;
  getToken: TokenProvider;
  /** Injectable for tests and for the widget's WebView fetch. */
  fetch?: typeof globalThis.fetch;
  /** Applied per request; the caller's own signal still wins if it aborts first. */
  timeoutMs?: number;
}

type QueryValue = string | number | boolean | null | undefined;

interface RequestOptions {
  query?: Record<string, QueryValue>;
  body?: unknown;
  signal?: AbortSignal;
  /** Enrollment and health are the only unauthenticated operations in the contract. */
  anonymous?: boolean;
}

const DEFAULT_TIMEOUT_MS = 20_000;

export class ApiClient {
  readonly #baseUrl: string;
  readonly #getToken: TokenProvider;
  readonly #fetch: typeof globalThis.fetch;
  readonly #timeoutMs: number;

  constructor(options: ApiClientOptions) {
    this.#baseUrl = options.baseUrl.replace(/\/+$/, '');
    this.#getToken = options.getToken;
    // Bound to globalThis: an unbound `fetch` reference throws "Illegal invocation"
    // in the WebView2 host.
    this.#fetch = options.fetch ?? globalThis.fetch.bind(globalThis);
    this.#timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  }

  /* ------------------------------------------------------------- transport */

  async #request<T>(method: string, path: string, options: RequestOptions = {}): Promise<T> {
    const url = this.#baseUrl + path + encodeQuery(options.query);

    const headers: Record<string, string> = { Accept: 'application/json' };
    if (options.body !== undefined) headers['Content-Type'] = 'application/json';
    if (!options.anonymous) {
      const token = await this.#getToken();
      if (!token) {
        // Fail before the round trip: an unauthenticated request would come back
        // 401 and be indistinguishable from an expired token, which sends the
        // widget into a sign-out loop.
        throw new ApiError(0, { code: 'no_credentials', message: 'No auth token available' });
      }
      headers['Authorization'] = `Bearer ${token}`;
    }

    const timeout = AbortSignal.timeout(this.#timeoutMs);
    const signal = options.signal ? anySignal([options.signal, timeout]) : timeout;

    let response: Response;
    try {
      response = await this.#fetch(url, {
        method,
        headers,
        signal,
        ...(options.body !== undefined ? { body: JSON.stringify(options.body) } : {}),
      });
    } catch (cause) {
      // Status 0 marks "never reached the gateway" so callers can distinguish an
      // offline desktop from a rejection. The URL is included; request bodies and
      // response bodies are not, because they can carry borrower data.
      throw new ApiError(0, {
        code: isAbort(cause) ? 'timeout' : 'network_error',
        message: isAbort(cause) ? `Request timed out: ${method} ${path}` : `Network error: ${method} ${path}`,
      });
    }

    if (!response.ok) throw await toApiError(response);

    if (response.status === 204) return undefined as T;
    const text = await response.text();
    if (text === '') return undefined as T;
    try {
      return JSON.parse(text) as T;
    } catch {
      throw new ApiError(response.status, {
        code: 'malformed_response',
        message: `Expected JSON from ${method} ${path}`,
      });
    }
  }

  /* ---------------------------------------------------------------- health */

  health(signal?: AbortSignal): Promise<HealthResponse> {
    return this.#request('GET', '/healthz', { anonymous: true, ...sig(signal) });
  }

  /* ---------------------------------------------------------------- device */

  enrollDevice(body: EnrollRequest, signal?: AbortSignal): Promise<EnrollResponse> {
    return this.#request('POST', '/v1/devices/enroll', { body, anonymous: true, ...sig(signal) });
  }

  getPolicy(signal?: AbortSignal): Promise<Policy> {
    return this.#request('GET', '/v1/policy', sig(signal));
  }

  heartbeat(body: Heartbeat, signal?: AbortSignal): Promise<HeartbeatResponse> {
    return this.#request('POST', '/v1/heartbeat', { body, ...sig(signal) });
  }

  /* --------------------------------------------------------------- session */

  createSession(signal?: AbortSignal): Promise<SessionResponse> {
    return this.#request('POST', '/v1/sessions', sig(signal));
  }

  /** The client MUST flush its spool before calling this. */
  endSession(signal?: AbortSignal): Promise<void> {
    return this.#request('DELETE', '/v1/sessions/current', sig(signal));
  }

  /* ----------------------------------------------------------------- calls */

  /**
   * The listing every role shares. What comes back is decided by row-level
   * security from the verified token; the parameters only narrow it, so passing an
   * id outside the caller's scope yields an empty page rather than someone else's
   * calls.
   */
  listCalls(params: CallQuery = {}, signal?: AbortSignal): Promise<CallPage> {
    return this.#request('GET', '/v1/calls', { query: params, ...sig(signal) });
  }

  /**
   * One call at the caller's scope. Rejects with 404 — never 403 — when the call is
   * outside it, so callers must not word a failure as "this call does not exist".
   */
  getCall(id: string, signal?: AbortSignal): Promise<CallDetail> {
    return this.#request('GET', `/v1/calls/${encodeURIComponent(id)}`, sig(signal));
  }

  listTeams(signal?: AbortSignal): Promise<Team[]> {
    return this.#request('GET', '/v1/teams', sig(signal));
  }

  /* -------------------------------------------------------------------- me */

  listMyCalls(
    params: {
      from?: string | undefined;
      to?: string | undefined;
      disposition?: Disposition | undefined;
      q?: string | undefined;
      limit?: number | undefined;
      cursor?: string | undefined;
    } = {},
    signal?: AbortSignal,
  ): Promise<CallPage> {
    return this.#request('GET', '/v1/me/calls', { query: params, ...sig(signal) });
  }

  getMyCall(id: string, signal?: AbortSignal): Promise<CallDetail> {
    return this.#request('GET', `/v1/me/calls/${encodeURIComponent(id)}`, sig(signal));
  }

  confirmMyCall(id: string, body: CallConfirmation, signal?: AbortSignal): Promise<CallDetail> {
    return this.#request('POST', `/v1/me/calls/${encodeURIComponent(id)}/confirm`, { body, ...sig(signal) });
  }

  getMyStats(params: { from?: string | undefined; to?: string | undefined } = {}, signal?: AbortSignal): Promise<AgentStats> {
    return this.#request('GET', '/v1/me/stats', { query: params, ...sig(signal) });
  }

  listMyFlags(signal?: AbortSignal): Promise<Flag[]> {
    return this.#request('GET', '/v1/me/flags', sig(signal));
  }

  respondToMyFlag(id: string, response: string, signal?: AbortSignal): Promise<Flag> {
    return this.#request('POST', `/v1/me/flags/${encodeURIComponent(id)}/respond`, {
      body: { response },
      ...sig(signal),
    });
  }

  /* ------------------------------------------------------------------ team */

  listTeamCalls(
    teamId: string,
    params: { from?: string | undefined; to?: string | undefined; user_uid?: string | undefined; limit?: number | undefined; cursor?: string | undefined } = {},
    signal?: AbortSignal,
  ): Promise<CallPage> {
    return this.#request('GET', `/v1/teams/${encodeURIComponent(teamId)}/calls`, {
      query: params,
      ...sig(signal),
    });
  }

  getTeamScorecards(
    teamId: string,
    params: { from?: string | undefined; to?: string | undefined } = {},
    signal?: AbortSignal,
  ): Promise<{ median: AgentStats; agents: AgentStats[] }> {
    return this.#request('GET', `/v1/teams/${encodeURIComponent(teamId)}/scorecards`, {
      query: params,
      ...sig(signal),
    });
  }

  /**
   * Exchanges the bearer token for a ticket the SSE stream will accept. The ticket
   * is single-use and short-lived, so mint one per connection attempt and never
   * cache it.
   */
  createLiveTicket(teamId: string, signal?: AbortSignal): Promise<LiveTicket> {
    return this.#request('POST', `/v1/teams/${encodeURIComponent(teamId)}/live/ticket`, sig(signal));
  }

  /**
   * URL of the live-floor SSE stream. Built rather than subscribed because
   * `EventSource` belongs to the caller's lifecycle, not the client's. The ticket
   * travels in the query string precisely so the bearer token does not: a ticket in
   * an access log is worthless a minute later, a token is not.
   */
  teamLiveUrl(teamId: string, ticket: string): string {
    return (
      `${this.#baseUrl}/v1/teams/${encodeURIComponent(teamId)}/live` +
      `?ticket=${encodeURIComponent(ticket)}`
    );
  }

  /* ------------------------------------------------------------ compliance */

  listFlags(
    params: {
      severity?: Severity | undefined;
      status?: FlagStatus | undefined;
      rule_id?: string | undefined;
      from?: string | undefined;
      to?: string | undefined;
      limit?: number | undefined;
      cursor?: string | undefined;
    } = {},
    signal?: AbortSignal,
  ): Promise<FlagPage> {
    return this.#request('GET', '/v1/compliance/flags', { query: params, ...sig(signal) });
  }

  updateFlag(id: string, body: FlagUpdate, signal?: AbortSignal): Promise<Flag> {
    return this.#request('PATCH', `/v1/compliance/flags/${encodeURIComponent(id)}`, { body, ...sig(signal) });
  }

  createEvidenceExport(body: EvidenceExportRequest, signal?: AbortSignal): Promise<EvidenceExportJob> {
    return this.#request('POST', '/v1/compliance/exports', { body, ...sig(signal) });
  }

  /* ----------------------------------------------------------------- admin */

  listDevices(
    params: { status?: DeviceStatusFilter | undefined; limit?: number | undefined; cursor?: string | undefined } = {},
    signal?: AbortSignal,
  ): Promise<DevicePage> {
    return this.#request('GET', '/v1/admin/devices', { query: params, ...sig(signal) });
  }

  revokeDevice(id: string, reason?: string, signal?: AbortSignal): Promise<void> {
    return this.#request('POST', `/v1/admin/devices/${encodeURIComponent(id)}/revoke`, {
      body: reason === undefined ? {} : { reason },
      ...sig(signal),
    });
  }

  createEnrollmentToken(signal?: AbortSignal): Promise<EnrollmentToken> {
    return this.#request('POST', '/v1/admin/enrollment-tokens', sig(signal));
  }

  listUsers(signal?: AbortSignal): Promise<User[]> {
    return this.#request('GET', '/v1/admin/users', sig(signal));
  }

  updateUser(uid: string, body: UserUpdate, signal?: AbortSignal): Promise<User> {
    return this.#request('PATCH', `/v1/admin/users/${encodeURIComponent(uid)}`, { body, ...sig(signal) });
  }

  getRules(signal?: AbortSignal): Promise<RuleSet> {
    return this.#request('GET', '/v1/admin/rules', sig(signal));
  }

  /** Publishes a new version; never mutates an existing one. */
  putRules(body: RuleSetDefinition, signal?: AbortSignal): Promise<RuleSet> {
    return this.#request('PUT', '/v1/admin/rules', { body, ...sig(signal) });
  }

  getAuditLog(
    params: { actor_uid?: string | undefined; entity?: string | undefined; from?: string | undefined; to?: string | undefined; limit?: number | undefined; cursor?: string | undefined } = {},
    signal?: AbortSignal,
  ): Promise<AuditPage> {
    return this.#request('GET', '/v1/admin/audit', { query: params, ...sig(signal) });
  }
}

/** Query surface of GET /v1/calls. Every field narrows; none can widen. */
export interface CallQuery {
  from?: string | undefined;
  to?: string | undefined;
  user_uid?: string | undefined;
  team_id?: string | undefined;
  disposition?: Disposition | undefined;
  has_flags?: boolean | undefined;
  q?: string | undefined;
  limit?: number | undefined;
  cursor?: string | undefined;
}

/* --------------------------------------------------------------- internals */

function sig(signal: AbortSignal | undefined): RequestOptions {
  return signal ? { signal } : {};
}

function isAbort(cause: unknown): boolean {
  return cause instanceof Error && (cause.name === 'AbortError' || cause.name === 'TimeoutError');
}

/** Skips null/undefined so optional filters do not become `?from=undefined`. */
function encodeQuery(query: Record<string, QueryValue> | undefined): string {
  if (!query) return '';
  const parts = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value === undefined || value === null || value === '') continue;
    parts.set(key, String(value));
  }
  const encoded = parts.toString();
  return encoded ? `?${encoded}` : '';
}

async function toApiError(response: Response): Promise<ApiError> {
  let text = '';
  try {
    text = await response.text();
  } catch {
    // Body already consumed or the stream broke; the status alone still classifies.
  }
  if (text !== '') {
    try {
      const parsed: unknown = JSON.parse(text);
      if (isErrorBody(parsed)) return new ApiError(response.status, parsed);
    } catch {
      // Not JSON — an intermediary (proxy, load balancer) answered. Fall through.
    }
  }
  return new ApiError(response.status, {
    code: fallbackCode(response.status),
    message: fallbackMessage(response.status),
  });
}

function isErrorBody(value: unknown): value is ApiErrorBody {
  if (typeof value !== 'object' || value === null) return false;
  const candidate = value as Record<string, unknown>;
  return typeof candidate['code'] === 'string' && typeof candidate['message'] === 'string';
}

function fallbackCode(status: number): string {
  switch (status) {
    case 400:
      return 'bad_request';
    case 401:
      return 'unauthorized';
    case 403:
      return 'forbidden';
    case 404:
      return 'not_found';
    case 409:
      return 'conflict';
    default:
      return status >= 500 ? 'server_error' : 'request_failed';
  }
}

function fallbackMessage(status: number): string {
  switch (status) {
    case 401:
      return 'Your session has expired. Sign in again.';
    case 403:
      return 'You do not have access to this.';
    case 404:
      return 'Not found, or not visible at your access level.';
    case 409:
      return 'This conflicts with the current state of the record.';
    default:
      return status >= 500 ? 'The server could not complete this request.' : 'The request was rejected.';
  }
}

/**
 * AbortSignal.any is only in Node 20+/Chromium 121+. WebView2 on an evergreen
 * runtime has it, but a pinned fixed-version runtime may not, so fall back rather
 * than losing the caller's cancellation.
 */
function anySignal(signals: AbortSignal[]): AbortSignal {
  const anyFn = (AbortSignal as { any?: (s: AbortSignal[]) => AbortSignal }).any;
  if (typeof anyFn === 'function') return anyFn(signals);
  const controller = new AbortController();
  for (const signal of signals) {
    if (signal.aborted) {
      controller.abort(signal.reason);
      break;
    }
    signal.addEventListener('abort', () => controller.abort(signal.reason), { once: true });
  }
  return controller.signal;
}
