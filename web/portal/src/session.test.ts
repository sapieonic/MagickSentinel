import { describe, expect, it } from 'vitest';
import { ApiError, MISSING_CREDENTIALS } from '@sentinel/shared';
import { sessionErrorMessage } from './session.js';

const BASE_URL = 'https://api.sentinel.magickvoice.com';

describe('sessionErrorMessage', () => {
  it('names the gateway and the two things that could be wrong when it cannot be reached', () => {
    // `POST /v1/sessions` is the portal's first authenticated request, so it is where
    // a wrong VITE_API_BASE_URL and a gateway whose SENTINEL_ALLOWED_ORIGINS does not
    // list this portal both show up first — as the same indistinguishable browser
    // error. Naming both is the most honest thing available.
    const message = sessionErrorMessage(
      new ApiError(0, { code: 'network_error', message: 'Could not reach the gateway' }),
      BASE_URL,
    );
    expect(message).toContain(BASE_URL);
    expect(message).toContain('VITE_API_BASE_URL');
    expect(message).toContain('origins');
  });

  it('does not tell the user to sign in again for a problem signing in cannot fix', () => {
    // The behaviour this replaces: one sentence, "Could not start a session. Sign in
    // again.", shown for a CORS misconfiguration that no amount of signing in touches.
    const message = sessionErrorMessage(
      new ApiError(0, { code: 'network_error', message: 'Could not reach the gateway' }),
      BASE_URL,
    );
    expect(message.toLowerCase()).not.toContain('sign in again');
  });

  it('says the account is not permitted when the gateway refuses a valid token', () => {
    const message = sessionErrorMessage(
      new ApiError(403, { code: 'forbidden', message: 'role not permitted' }),
      BASE_URL,
    );
    expect(message).toContain('not permitted');
    expect(message.toLowerCase()).not.toContain('sign in again');
  });

  it('surfaces the request id so support can find the log line', () => {
    const message = sessionErrorMessage(
      new ApiError(500, { code: 'internal', message: 'boom', request_id: 'req-42' }),
      BASE_URL,
    );
    expect(message).toContain('req-42');
    // Never the gateway's own message: an upstream can put anything in there,
    // including data the viewer should not see.
    expect(message).not.toContain('boom');
  });

  it('still says something useful for a failure that is not an ApiError at all', () => {
    expect(sessionErrorMessage(new TypeError('undefined is not a function'), BASE_URL)).toBe(
      'Could not start a session.',
    );
  });

  it('treats a missing credential as the auth layer’s business, not a session error', () => {
    // The provider hands this case to `onCredentialsLost` rather than rendering it, so
    // that "you are signed out" is one screen with one button instead of a session
    // error competing with the sign-in screen. This asserts the classification the
    // provider branches on.
    const error = new ApiError(0, { code: MISSING_CREDENTIALS, message: 'No auth token available' });
    expect(error.isMissingCredentials).toBe(true);
    expect(error.isTransport).toBe(false);
  });
});
