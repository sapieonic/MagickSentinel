import { describe, expect, it } from 'vitest';
import { SIGNED_OUT, credentialAction, reduceAuth, signInErrorMessage, tenantMismatch } from './identity.js';
import type { AuthPhase, IdentityUserSummary } from './identity.js';

const USER: IdentityUserSummary = {
  uid: 'uid-1',
  email: 'supervisor@bpo.example',
  displayName: 'S. Rao',
  tenantId: 'bpo-alpha',
};

describe('reduceAuth', () => {
  it('lands on the sign-in screen when startup finds nobody signed in', () => {
    const phase = reduceAuth({ kind: 'starting' }, { type: 'user', user: null });
    expect(phase).toEqual({ kind: 'signed_out', error: null, busy: false });
  });

  it('goes straight to signed_in when a persisted session is restored', () => {
    // No sign-in screen in between: flashing a password form at someone who is
    // already signed in trains them to start typing before the page settles.
    expect(reduceAuth({ kind: 'starting' }, { type: 'user', user: USER })).toEqual({
      kind: 'signed_in',
      user: USER,
    });
  });

  it('marks the sign-in screen busy while an attempt is in flight', () => {
    const phase = reduceAuth(SIGNED_OUT, { type: 'sign_in_started' });
    expect(phase).toEqual({ kind: 'signed_out', error: null, busy: true });
  });

  it('clears a previous error when a new attempt starts', () => {
    const failed = reduceAuth(SIGNED_OUT, { type: 'sign_in_failed', message: 'Wrong password.' });
    const retrying = reduceAuth(failed, { type: 'sign_in_started' });
    expect(retrying).toEqual({ kind: 'signed_out', error: null, busy: true });
  });

  it('drops the spinner when an attempt fails', () => {
    const busy = reduceAuth(SIGNED_OUT, { type: 'sign_in_started' });
    const failed = reduceAuth(busy, { type: 'sign_in_failed', message: 'Too many attempts.' });
    expect(failed).toEqual({ kind: 'signed_out', error: 'Too many attempts.', busy: false });
  });

  it('signs out with no error, because the user knows why they are there', () => {
    const signedIn: AuthPhase = { kind: 'signed_in', user: USER };
    expect(reduceAuth(signedIn, { type: 'sign_out' })).toEqual(SIGNED_OUT);
  });

  it('replaces a live session with the sign-in screen when the credential is lost', () => {
    const signedIn: AuthPhase = { kind: 'signed_in', user: USER };
    const lost = reduceAuth(signedIn, { type: 'credentials_lost', message: 'Your session was ended elsewhere.' });
    expect(lost).toEqual({
      kind: 'signed_out',
      error: 'Your session was ended elsewhere.',
      busy: false,
    });
  });

  it('keeps the reason when the provider follows a lost credential with a null user', () => {
    // Identity Platform reports a revoked refresh token as a failed refresh first and
    // a null user a moment later. The useful sentence is in the first event; letting
    // the second erase it leaves an operator at a bare form with no idea what
    // happened, and a call to support to find out.
    const lost = reduceAuth(
      { kind: 'signed_in', user: USER },
      { type: 'credentials_lost', message: 'This account has been disabled.' },
    );
    const settled = reduceAuth(lost, { type: 'user', user: null });
    expect(settled).toEqual({
      kind: 'signed_out',
      error: 'This account has been disabled.',
      busy: false,
    });
  });

  it('drops the spinner on a null user so a cancelled sign-in is not stuck busy', () => {
    const busy = reduceAuth(SIGNED_OUT, { type: 'sign_in_started' });
    expect(reduceAuth(busy, { type: 'user', user: null })).toEqual(SIGNED_OUT);
  });

  it('signs a second user in over the first without passing through signed_out', () => {
    // A shared workstation: one supervisor signs out and the next signs in. There must
    // be no frame in which the second user's session is rendered under the first
    // user's identity.
    const first: AuthPhase = { kind: 'signed_in', user: USER };
    const second = { ...USER, uid: 'uid-2', displayName: 'A. Khan' };
    expect(reduceAuth(first, { type: 'user', user: second })).toEqual({ kind: 'signed_in', user: second });
  });

  it('absorbs every event once the build is known to be misconfigured', () => {
    const broken: AuthPhase = { kind: 'misconfigured', problems: ['VITE_IDENTITY_TENANT_ID is not set.'] };
    // Nothing at runtime fixes a missing tenant, and offering a sign-in form that
    // cannot work is worse than saying so.
    for (const event of [
      { type: 'user' as const, user: USER },
      { type: 'sign_in_started' as const },
      { type: 'sign_out' as const },
    ]) {
      expect(reduceAuth(broken, event)).toBe(broken);
    }
  });
});

describe('tenantMismatch', () => {
  it('accepts a credential issued under the configured tenant', () => {
    expect(tenantMismatch(USER, 'bpo-alpha')).toBeNull();
  });

  it('refuses a credential from another tenant', () => {
    // The gateway refuses this too, by checking firebase.tenant. Catching it here is
    // what turns "every request 401s with no explanation" into one sentence.
    const message = tenantMismatch({ ...USER, tenantId: 'bpo-beta' }, 'bpo-alpha');
    expect(message).toContain('bpo-alpha');
    // The other customer's tenant identifier does not belong on this screen.
    expect(message).not.toContain('bpo-beta');
  });

  it('refuses a project-level credential that carries no tenant at all', () => {
    // This is what a build that forgot to set `auth.tenantId` produces: a sign-in that
    // looks completely successful and a token the gateway cannot place.
    expect(tenantMismatch({ ...USER, tenantId: null }, 'bpo-alpha')).not.toBeNull();
  });
});

describe('signInErrorMessage', () => {
  it('gives one answer for a wrong password and an unknown account', () => {
    // Answering them separately turns the form into a directory of who works at the
    // BPO, which is a finding in a bank's security review.
    const wrong = signInErrorMessage({ code: 'auth/wrong-password' });
    const missing = signInErrorMessage({ code: 'auth/user-not-found' });
    const invalid = signInErrorMessage({ code: 'auth/invalid-credential' });
    expect(new Set([wrong, missing, invalid]).size).toBe(1);
  });

  it('names the causes an operator can act on', () => {
    expect(signInErrorMessage({ code: 'auth/user-disabled' })).toContain('disabled');
    expect(signInErrorMessage({ code: 'auth/too-many-requests' })).toContain('Wait');
    expect(signInErrorMessage({ code: 'auth/network-request-failed' })).toContain('connection');
    expect(signInErrorMessage({ code: 'auth/popup-blocked' })).toContain('pop-ups');
    expect(signInErrorMessage({ code: 'auth/operation-not-allowed' })).toContain('not enabled');
  });

  it('never renders the provider’s raw message', () => {
    // Firebase error text can carry the email address that was tried, and for a
    // federated failure it can carry the IdP's own response verbatim.
    const message = signInErrorMessage(
      Object.assign(new Error('SAML response rejected for supervisor@bpo.example'), { code: 'auth/internal-error' }),
    );
    expect(message).not.toContain('@');
    expect(message).not.toContain('SAML');
    expect(message).toContain('Sign-in failed');
  });

  it('handles a thrown value that is not an error object at all', () => {
    expect(signInErrorMessage(undefined)).toContain('Sign-in failed');
    expect(signInErrorMessage('boom')).toContain('Sign-in failed');
  });
});

describe('credentialAction', () => {
  const NONE = { uid: null, live: false };
  const HOLDING = { uid: 'uid-1', live: true };

  it('adopts a user it is not already holding a credential for', () => {
    expect(credentialAction(USER, 'bpo-alpha', NONE)).toEqual({ kind: 'adopt', user: USER });
  });

  it('does nothing when the same user’s token merely rotated', () => {
    // The provider notifies on token changes, not user changes, so this fires for
    // every rotation — including the ones the portal's own cache asked for. Rebuilding
    // the cache here would leave the replacement with no scheduled refresh, so
    // proactive refreshing would silently stop after the first hour and every later
    // expiry would be paid for with a failed request.
    expect(credentialAction(USER, 'bpo-alpha', HOLDING)).toEqual({ kind: 'keep' });
  });

  it('adopts again when the held credential has already failed to refresh', () => {
    // A dead cache is not a credential. If the provider still reports the user, they
    // get one more chance rather than being stuck holding something that returns null.
    expect(credentialAction(USER, 'bpo-alpha', { uid: 'uid-1', live: false })).toEqual({
      kind: 'adopt',
      user: USER,
    });
  });

  it('adopts the new user when someone else signs in at the same desk', () => {
    const next = { ...USER, uid: 'uid-2', displayName: 'A. Khan' };
    expect(credentialAction(next, 'bpo-alpha', HOLDING)).toEqual({ kind: 'adopt', user: next });
  });

  it('clears everything when the provider reports nobody', () => {
    expect(credentialAction(null, 'bpo-alpha', HOLDING)).toEqual({ kind: 'clear' });
  });

  it('rejects a credential from another tenant even while holding a good one', () => {
    const other = { ...USER, uid: 'uid-9', tenantId: 'bpo-beta' };
    const action = credentialAction(other, 'bpo-alpha', HOLDING);
    expect(action.kind).toBe('reject');
  });

  it('checks the tenant before deciding a rotation can be kept', () => {
    // Ordering matters: a same-uid event whose tenant no longer matches must not be
    // waved through as a rotation.
    const drifted = { ...USER, tenantId: null };
    expect(credentialAction(drifted, 'bpo-alpha', HOLDING).kind).toBe('reject');
  });
});
