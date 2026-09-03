import { describe, expect, it } from 'vitest';
import { ApiError, MISSING_CREDENTIALS } from '@sentinel/shared';
import { historyErrorMessage } from './HistoryTab.js';

describe('historyErrorMessage', () => {
  it('tells a signed-out agent to sign in, not to wait for the network', () => {
    // The distinction that matters on a collections desktop. Being offline for a
    // minute is routine and waiting is right; being signed out means the token the
    // native layer holds is gone, and waiting is how a shift gets spent not knowing
    // the widget needed a sign-in.
    const error = new ApiError(0, { code: MISSING_CREDENTIALS, message: 'No auth token available' });
    expect(historyErrorMessage(error)).toBe('Sign in to see your history.');
  });

  it('still says "offline" for a real transport failure', () => {
    const error = new ApiError(0, { code: 'network_error', message: 'Could not reach the gateway' });
    expect(historyErrorMessage(error)).toBe('History is unavailable while offline.');
  });

  it('falls back to a plain failure for anything the gateway rejected', () => {
    const error = new ApiError(500, { code: 'internal', message: 'boom' });
    expect(historyErrorMessage(error)).toBe('Could not load your history.');
    // Never the gateway's own message: it can carry borrower data.
    expect(historyErrorMessage(error)).not.toContain('boom');
  });

  it('handles a rejection that is not an ApiError', () => {
    expect(historyErrorMessage(new TypeError('nope'))).toBe('Could not load your history.');
  });
});
