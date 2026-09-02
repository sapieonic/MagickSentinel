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
 * Token provider for the API client.
 *
 * Prefers `sentinel.getToken()`; falls back to a token the native layer set on the
 * window. Never caches: the agent process rotates the ID token roughly hourly and a
 * cached copy would 401 mid-shift.
 */
export function resolveTokenProvider(host: SentinelHost): () => Promise<string | null> {
  return async () => {
    if (typeof host.getToken === 'function') {
      try {
        return (await host.getToken()) ?? null;
      } catch {
        // A marshalling failure is not a credential; fall through to the window.
      }
    }
    const injected = (globalThis as unknown as WebView2Window).__SENTINEL_TOKEN__;
    return injected ?? null;
  };
}

export async function resolveApiBaseUrl(host: SentinelHost): Promise<string> {
  if (typeof host.getApiBaseUrl === 'function') {
    try {
      const url = await host.getApiBaseUrl();
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
