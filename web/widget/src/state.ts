/**
 * Maps native state (spec 6.7) onto the rendered widget state (spec 13.1).
 *
 * This is a pure function on purpose. The widget must never ask the network what
 * state it is in — spec 6.7 is explicit that anything the native layer holds is not
 * re-fetched — so the only input here is the host state, and the only output is
 * which view to draw. Keeping it pure also means the precedence rules below are
 * testable without a DOM or a running agent.
 */
import type { CaptureState, CaptureTier } from '@sentinel/shared';
import type { HostErrorCause, HostState } from './host/types.js';

export type WidgetView =
  | { kind: 'signed_out'; signingIn: boolean }
  | { kind: 'idle'; tier: CaptureTier; coverage: number | null }
  | { kind: 'armed'; tier: CaptureTier }
  | { kind: 'in_call'; callId: string | null; tier: CaptureTier }
  | { kind: 'wrap'; tier: CaptureTier }
  | { kind: 'post_call'; callId: string; endedAt: string; tier: CaptureTier }
  | { kind: 'error'; cause: HostErrorCause; detail: string | undefined };

/**
 * Capture is running in every state except IDLE and BLOCKED. ARMED counts: the ring
 * buffer is already recording by then, which is the whole point of the armed state,
 * so the indicator has to be up. FINALIZE counts too — the audio is still being
 * flushed and the agent should not believe recording has stopped.
 */
export function isCaptureActive(captureState: CaptureState): boolean {
  switch (captureState) {
    case 'ARMED':
    case 'IN_CALL':
    case 'WRAP':
    case 'FINALIZE':
      return true;
    case 'IDLE':
    case 'BLOCKED':
      return false;
  }
}

/**
 * Precedence, most specific first. The ordering is the interesting part:
 *
 *  1. Errors outrank being signed out. A revoked device or a missing headset is not
 *     fixed by signing in, and offering the sign-in button would send the agent
 *     down a dead end and generate a support ticket.
 *  2. A live call outranks an unconfirmed previous call. If the agent is already on
 *     the next call, the post-call card must not steal the screen; it comes back
 *     when the line clears.
 *  3. A pending confirmation outranks WRAP/FINALIZE. The card has a hard 5 s budget
 *     from hangup, and finalising the upload is not a reason to keep showing a
 *     spinner. This is what makes the deadline achievable at all.
 */
export function deriveWidgetView(state: HostState): WidgetView {
  const error = state.error ?? (state.captureState === 'BLOCKED' ? { cause: 'unknown' as HostErrorCause } : null);
  if (error) return { kind: 'error', cause: error.cause, detail: error.detail };

  if (state.authState !== 'signed_in') {
    return { kind: 'signed_out', signingIn: state.authState === 'signing_in' };
  }

  if (state.captureState === 'IN_CALL') {
    return { kind: 'in_call', callId: state.callId, tier: state.tier };
  }
  if (state.captureState === 'ARMED') {
    return { kind: 'armed', tier: state.tier };
  }

  const pending = pendingConfirmation(state);
  if (pending) {
    return { kind: 'post_call', callId: pending.callId, endedAt: pending.endedAt, tier: state.tier };
  }

  if (state.captureState === 'WRAP' || state.captureState === 'FINALIZE') {
    return { kind: 'wrap', tier: state.tier };
  }

  return { kind: 'idle', tier: state.tier, coverage: state.coverage };
}

/**
 * Fallback for agent builds that predate `pendingCall`: a call id that outlives the
 * call means the native layer is still holding an unconfirmed call. Without this an
 * older agent would drop straight from WRAP to the idle pill and the agent would
 * never get the disposition dropdown.
 */
function pendingConfirmation(state: HostState): { callId: string; endedAt: string } | null {
  if (state.pendingCall) return { callId: state.pendingCall.callId, endedAt: state.pendingCall.endedAt };
  if (state.captureState === 'IDLE' && state.callId) {
    return { callId: state.callId, endedAt: new Date().toISOString() };
  }
  return null;
}

/**
 * Hard budget from spec 13.1: the post-call card must be on screen within 5 s of
 * hangup. The card never waits on analysis, so this is only used to decide when to
 * stop pretending the summary is on its way and say so instead.
 */
export const POST_CALL_CARD_DEADLINE_MS = 5000;

/**
 * How long to keep showing the "writing summary" placeholder before telling the
 * agent it is not coming. Analysis can legitimately take longer than the card
 * deadline; leaving a spinner up indefinitely trains agents to ignore the card.
 */
export const SUMMARY_PLACEHOLDER_TIMEOUT_MS = 20_000;

export function summaryState(
  summary: string | null | undefined,
  elapsedMs: number,
): 'ready' | 'pending' | 'unavailable' {
  if (summary) return 'ready';
  return elapsedMs < SUMMARY_PLACEHOLDER_TIMEOUT_MS ? 'pending' : 'unavailable';
}

/** Human-readable text for each error cause named in spec 13.1. */
export function errorCopy(cause: HostErrorCause): { title: string; body: string; recoverable: boolean } {
  switch (cause) {
    case 'headset_missing':
      return {
        title: 'Headset not detected',
        body: 'Plug in the assigned headset. Capture will not start from the default audio device.',
        recoverable: true,
      };
    case 'offline_past_grace':
      return {
        title: 'Offline too long',
        body: 'This machine has been offline past the allowed grace period. Reconnect to the network to resume.',
        recoverable: true,
      };
    case 'device_revoked':
      return {
        title: 'Device revoked',
        body: 'This device has been revoked by an administrator. Capture has stopped. Contact your supervisor.',
        recoverable: false,
      };
    case 'unknown':
      return {
        title: 'Capture blocked',
        body: 'Sentinel cannot record on this machine right now. Contact your supervisor if this persists.',
        recoverable: false,
      };
  }
}
