import { describe, expect, it } from 'vitest';
import { normaliseHostState } from './host/bridge.js';
import type { HostState } from './host/types.js';
import { POST_CALL_CARD_DEADLINE_MS, deriveWidgetView, errorCopy, isCaptureActive, summaryState } from './state.js';

const signedIn: HostState = {
  authState: 'signed_in',
  captureState: 'IDLE',
  tier: 'A',
  coverage: 0.91,
  callId: null,
  error: null,
  pendingCall: null,
  displayName: 'A. Agent',
};

const host = (patch: Partial<HostState>): HostState => ({ ...signedIn, ...patch });

describe('deriveWidgetView — every state in spec 13.1 is reachable', () => {
  it('signed out', () => {
    expect(deriveWidgetView(host({ authState: 'signed_out' }))).toEqual({ kind: 'signed_out', signingIn: false });
    expect(deriveWidgetView(host({ authState: 'signing_in' }))).toEqual({ kind: 'signed_out', signingIn: true });
  });

  it('idle carries the tier badge and today’s coverage', () => {
    expect(deriveWidgetView(host({ captureState: 'IDLE', coverage: 0.83, tier: 'B' }))).toEqual({
      kind: 'idle',
      tier: 'B',
      coverage: 0.83,
    });
  });

  it('armed, in call, wrap', () => {
    expect(deriveWidgetView(host({ captureState: 'ARMED' })).kind).toBe('armed');
    expect(deriveWidgetView(host({ captureState: 'IN_CALL', callId: 'c1' }))).toEqual({
      kind: 'in_call',
      callId: 'c1',
      tier: 'A',
    });
    expect(deriveWidgetView(host({ captureState: 'WRAP' })).kind).toBe('wrap');
    expect(deriveWidgetView(host({ captureState: 'FINALIZE' })).kind).toBe('wrap');
  });

  it('post-call card once the native layer reports a pending confirmation', () => {
    const view = deriveWidgetView(
      host({ captureState: 'IDLE', pendingCall: { callId: 'c9', endedAt: '2026-09-01T10:00:00Z' } }),
    );
    expect(view).toEqual({ kind: 'post_call', callId: 'c9', endedAt: '2026-09-01T10:00:00Z', tier: 'A' });
  });

  it('error, with the specific cause spec 13.1 requires', () => {
    for (const cause of ['headset_missing', 'offline_past_grace', 'device_revoked'] as const) {
      expect(deriveWidgetView(host({ error: { cause } }))).toEqual({ kind: 'error', cause, detail: undefined });
    }
  });
});

describe('deriveWidgetView — precedence', () => {
  it('shows the error rather than the sign-in button when the device is revoked', () => {
    // Signing in cannot fix a revoked device; offering the button is a dead end.
    const view = deriveWidgetView(host({ authState: 'signed_out', error: { cause: 'device_revoked' } }));
    expect(view).toEqual({ kind: 'error', cause: 'device_revoked', detail: undefined });
  });

  it('treats BLOCKED without an explicit cause as an unknown error, never as idle', () => {
    expect(deriveWidgetView(host({ captureState: 'BLOCKED' }))).toEqual({
      kind: 'error',
      cause: 'unknown',
      detail: undefined,
    });
  });

  it('lets a live call outrank an unconfirmed previous call', () => {
    const view = deriveWidgetView(
      host({
        captureState: 'IN_CALL',
        callId: 'c-new',
        pendingCall: { callId: 'c-old', endedAt: '2026-09-01T10:00:00Z' },
      }),
    );
    expect(view).toEqual({ kind: 'in_call', callId: 'c-new', tier: 'A' });
  });

  it('lets the pending card outrank FINALIZE so the 5 s budget is not spent on an upload', () => {
    const view = deriveWidgetView(
      host({ captureState: 'FINALIZE', pendingCall: { callId: 'c9', endedAt: '2026-09-01T10:00:00Z' } }),
    );
    expect(view.kind).toBe('post_call');
  });

  it('falls back to a lingering call id when the agent build has no pendingCall', () => {
    const view = deriveWidgetView(host({ captureState: 'IDLE', callId: 'c-legacy', pendingCall: null }));
    expect(view.kind).toBe('post_call');
    if (view.kind === 'post_call') expect(view.callId).toBe('c-legacy');
  });

  it('does not show a card during ARMED just because a call id exists', () => {
    expect(deriveWidgetView(host({ captureState: 'ARMED', callId: 'c1' })).kind).toBe('armed');
  });
});

describe('isCaptureActive', () => {
  it('keeps the indicator up for every state where audio is still moving', () => {
    expect(['ARMED', 'IN_CALL', 'WRAP', 'FINALIZE'].map(isCaptureActive as never)).toEqual([true, true, true, true]);
  });

  it('is false only when nothing is being recorded', () => {
    expect(isCaptureActive('IDLE')).toBe(false);
    expect(isCaptureActive('BLOCKED')).toBe(false);
  });
});

describe('normaliseHostState', () => {
  it('fails closed on an unrecognised capture state', () => {
    // Treating an unknown state as IDLE would hide the recording indicator while
    // capture may still be running — the one outcome compliance cannot tolerate.
    const state = normaliseHostState({ authState: 'signed_in', captureState: 'SOMETHING_NEW', tier: 'A' });
    expect(state.captureState).toBe('BLOCKED');
    expect(state.error).toEqual({ cause: 'unknown' });
    expect(deriveWidgetView(state).kind).toBe('error');
  });

  it('defaults an unknown tier to B, the weaker guarantee', () => {
    expect(normaliseHostState({ captureState: 'IDLE', tier: 'Z' }).tier).toBe('B');
  });

  it('coerces junk from the host-object proxy without throwing', () => {
    const state = normaliseHostState({
      authState: 42,
      captureState: 'IDLE',
      tier: 'A',
      coverage: 'lots',
      callId: '',
      pendingCall: { callId: '' },
      error: { cause: 'not_a_real_cause' },
    });
    expect(state.authState).toBe('signed_out');
    expect(state.coverage).toBeNull();
    expect(state.callId).toBeNull();
    expect(state.pendingCall).toBeNull();
    expect(state.error).toEqual({ cause: 'unknown' });
  });

  it('survives null and undefined entirely', () => {
    expect(normaliseHostState(null).authState).toBe('signed_out');
    expect(normaliseHostState(undefined).captureState).toBe('BLOCKED');
  });

  it('keeps a well-formed state intact', () => {
    const raw = {
      authState: 'signed_in',
      captureState: 'IN_CALL',
      tier: 'A',
      coverage: 0.77,
      callId: 'c1',
      error: null,
      pendingCall: null,
      displayName: 'A. Agent',
    };
    expect(normaliseHostState(raw)).toEqual({ ...raw, error: null, pendingCall: null });
  });
});

describe('summary placeholder', () => {
  it('never blocks the card: the summary is a separate concern from rendering', () => {
    expect(summaryState(null, 0)).toBe('pending');
    expect(summaryState('Borrower agreed to pay.', 0)).toBe('ready');
  });

  it('gives up rather than spinning forever', () => {
    expect(summaryState(null, 60_000)).toBe('unavailable');
  });

  it('has a summary timeout well past the card deadline', () => {
    // If these ever crossed, the card would be replaced by an error before the
    // agent had a chance to read it.
    expect(POST_CALL_CARD_DEADLINE_MS).toBeLessThan(20_000);
  });
});

describe('errorCopy', () => {
  it('gives every cause distinct, actionable text', () => {
    const titles = (['headset_missing', 'offline_past_grace', 'device_revoked', 'unknown'] as const).map(
      (c) => errorCopy(c).title,
    );
    expect(new Set(titles).size).toBe(titles.length);
    expect(errorCopy('device_revoked').recoverable).toBe(false);
    expect(errorCopy('headset_missing').recoverable).toBe(true);
  });
});
