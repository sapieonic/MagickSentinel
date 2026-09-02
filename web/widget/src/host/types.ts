/**
 * The native host object contract, from spec section 6.7.
 *
 * WebView2 exposes native objects at `window.chrome.webview.hostObjects.<name>`.
 * Every member of a host object is marshalled asynchronously, so all five documented
 * methods are modelled as returning promises even where the native signature is
 * void — awaiting them is the only way to know the call reached the agent process.
 *
 * Spec 6.7 documents exactly this surface:
 *
 *   sentinel.getState()        -> { authState, captureState, tier, coverage, callId }
 *   sentinel.signIn()          -> starts the PKCE flow
 *   sentinel.signOut()
 *   sentinel.onStateChange(cb)
 *   sentinel.confirmCall(id, payload)
 *   sentinel.openPortal(path)
 *
 * Anything below marked EXTENSION is not in 6.7 but is required to build the states
 * 13.1 asks for. Each one degrades to a documented fallback when the native layer
 * does not provide it, so an older agent build still renders something correct.
 */
import type { CallConfirmation, CaptureState, CaptureTier } from '@sentinel/shared';

export type HostAuthState = 'signed_out' | 'signing_in' | 'signed_in';

/** The three causes spec 13.1 names, plus a catch-all so an unknown block is not silently idle. */
export type HostErrorCause = 'headset_missing' | 'offline_past_grace' | 'device_revoked' | 'unknown';

export const HOST_ERROR_CAUSES: readonly HostErrorCause[] = [
  'headset_missing',
  'offline_past_grace',
  'device_revoked',
  'unknown',
];

export interface HostError {
  cause: HostErrorCause;
  /** Short operator-facing detail. Must never carry borrower data. */
  detail?: string;
}

/** A call that has ended and is waiting on the agent's confirmation. */
export interface HostPendingCall {
  callId: string;
  endedAt: string;
  /** Wall-clock ms at which the native layer observed hangup, for the 5 s budget. */
  endedAtEpochMs?: number;
}

export interface HostState {
  authState: HostAuthState;
  captureState: CaptureState;
  tier: CaptureTier;
  /** Today's coverage as a 0..1 fraction, or null before the first reconciliation. */
  coverage: number | null;
  /** Id of the call currently being captured, null when not in one. */
  callId: string | null;

  /** EXTENSION. Absent means "no error"; a BLOCKED capture state without one is treated as `unknown`. */
  error?: HostError | null;
  /** EXTENSION. Absent means the post-call card is derived from `callId` outliving the call. */
  pendingCall?: HostPendingCall | null;
  /** EXTENSION. Display name for the pill; the widget shows nothing rather than guessing. */
  displayName?: string | null;
}

export interface SentinelHost {
  getState(): Promise<HostState>;
  signIn(): Promise<void>;
  signOut(): Promise<void>;
  /** Returns an unsubscribe function where the native layer supports it. */
  onStateChange(callback: (state: HostState) => void): Promise<(() => void) | void> | (() => void) | void;
  confirmCall(id: string, payload: CallConfirmation): Promise<void>;
  openPortal(path: string): Promise<void>;

  /**
   * EXTENSION. Spec 6.7 says history and summaries call the API "using a token the
   * native layer injects" but does not say how the token arrives. This is the
   * assumed shape; `resolveTokenProvider` also accepts a token injected onto the
   * window, so either native implementation works.
   */
  getToken?(): Promise<string | null>;
  /** EXTENSION. Gateway origin, so the bundle is not built per environment. */
  getApiBaseUrl?(): Promise<string>;
}

/** Shape WebView2 injects. Everything is optional: in a browser none of it exists. */
export interface WebView2Window {
  chrome?: {
    webview?: {
      hostObjects?: {
        sentinel?: SentinelHost;
      };
    };
  };
  /** EXTENSION fallback for token injection (see SentinelHost.getToken). */
  __SENTINEL_TOKEN__?: string;
  __SENTINEL_API_BASE_URL__?: string;
}
