/**
 * The identity surface the portal programs against, and the state machine that drives
 * what it renders.
 *
 * `IdentityBackend` is a deliberately small interface over Identity Platform rather
 * than the Firebase SDK itself. Two reasons, and neither is abstraction for its own
 * sake:
 *
 *  - Everything interesting about portal sign-in is sequencing — a redirect result
 *    consumed at boot, a user arriving after the first render, a token refresh failing
 *    mid-session, a sign-out that has to clear state before the next user sits down.
 *    None of that is testable against the real SDK in a node test runner, and all of
 *    it is testable against a fake that implements six methods.
 *  - OPEN-2 is undecided (`docs/open-decisions.md`). Federated SAML/OIDC and
 *    email/password enter through the same two methods here, so the answer changes
 *    configuration and not structure.
 *
 * `reduceAuth` is a pure reducer over the events the backend produces. Keeping it
 * pure is what lets the signed-out and sign-in-failure paths be asserted directly,
 * instead of only being reachable by driving a browser.
 */
import type { IdTokenFetcher } from './tokens.js';

/** The signed-in user, as far as the portal is concerned. */
export interface IdentityUser {
  uid: string;
  email: string | null;
  displayName: string | null;
  /**
   * The Identity Platform tenant the credential was actually issued under. Checked
   * against the configured tenant before the user is accepted; see `tenantMismatch`.
   */
  tenantId: string | null;
  getIdToken: IdTokenFetcher;
}

/** The same user without the token machinery, safe to keep in React state. */
export interface IdentityUserSummary {
  uid: string;
  email: string | null;
  displayName: string | null;
  tenantId: string | null;
}

export function summarise(user: IdentityUser): IdentityUserSummary {
  return { uid: user.uid, email: user.email, displayName: user.displayName, tenantId: user.tenantId };
}

/**
 * What `start` learned from the page load.
 *
 * `signInError` exists because the redirect flow reports a failed federated sign-in
 * on the *next* page load rather than to the caller that started it. Without a way to
 * carry that back, an Entra ID rejection would look identical to arriving at the
 * portal signed out, and the operator would keep clicking the button.
 */
export interface StartResult {
  signInError: string | null;
}

export interface IdentityBackend {
  /**
   * Completes anything the provider left pending in the page load — for the redirect
   * flow, consuming the credential Identity Platform put in the URL. Resolves once
   * the initial user is known, so the portal can tell "still deciding" from "signed
   * out" and not flash the sign-in screen at someone who is already signed in.
   */
  start(): Promise<StartResult>;
  /** Fires on sign-in, sign-out and every token rotation. Returns an unsubscribe. */
  onUserChanged(callback: (user: IdentityUser | null) => void): () => void;
  signInWithPassword(email: string, password: string): Promise<void>;
  /** SAML or OIDC, whichever `VITE_IDENTITY_FEDERATED_PROVIDER_ID` names. */
  signInWithFederatedProvider(): Promise<void>;
  signOut(): Promise<void>;
}

/* ------------------------------------------------------------------ phases */

export type AuthPhase =
  /** Before the provider has told us whether anyone is signed in. */
  | { kind: 'starting' }
  /**
   * The build is missing configuration it cannot invent — most importantly the
   * customer's Identity Platform tenant. A separate phase rather than an error
   * message on the sign-in screen, because there is no point offering a button that
   * cannot work.
   */
  | { kind: 'misconfigured'; problems: readonly string[] }
  | { kind: 'signed_out'; error: string | null; busy: boolean }
  | { kind: 'signed_in'; user: IdentityUserSummary };

export type AuthEvent =
  | { type: 'user'; user: IdentityUserSummary | null }
  | { type: 'sign_in_started' }
  | { type: 'sign_in_failed'; message: string }
  /** A refresh failed, so the session is over even though nobody clicked sign out. */
  | { type: 'credentials_lost'; message: string }
  | { type: 'sign_out' };

export const SIGNED_OUT: AuthPhase = { kind: 'signed_out', error: null, busy: false };

/**
 * Every transition the portal's auth state can make.
 *
 * The one non-obvious rule is at the bottom: a `user: null` event arriving after
 * `credentials_lost` must not clear the reason. Identity Platform reports a revoked
 * refresh token as a failed refresh first and a null user a moment later, and the
 * useful sentence — "this account has been disabled" — is in the first event. Letting
 * the second overwrite it leaves the operator staring at a bare sign-in form
 * wondering what happened, and calling support to find out.
 */
export function reduceAuth(phase: AuthPhase, event: AuthEvent): AuthPhase {
  // Nothing recovers a misconfigured build at runtime, so it absorbs every event
  // rather than being talked out of it by a stray callback.
  if (phase.kind === 'misconfigured') return phase;

  switch (event.type) {
    case 'sign_in_started':
      return { kind: 'signed_out', error: null, busy: true };

    case 'sign_in_failed':
      return { kind: 'signed_out', error: event.message, busy: false };

    case 'credentials_lost':
      return { kind: 'signed_out', error: event.message, busy: false };

    case 'sign_out':
      // A deliberate sign-out carries no error: the user knows why they are here.
      return SIGNED_OUT;

    case 'user':
      if (event.user !== null) return { kind: 'signed_in', user: event.user };
      if (phase.kind === 'signed_out' && phase.error !== null) {
        // Keep the explanation, drop the spinner.
        return { kind: 'signed_out', error: phase.error, busy: false };
      }
      return SIGNED_OUT;
  }
}

/* ------------------------------------------------------- tenant enforcement */

/**
 * Refuses a credential issued under a tenant other than the configured one.
 *
 * The gateway checks this too — it reads `firebase.tenant` off the verified token —
 * and the gateway's check is the one that counts. This one exists because failing at
 * the portal produces a sentence an operator can act on, whereas failing at the
 * gateway produces `401 token_invalid` on every request with no explanation. It is
 * also cheap insurance against a build where `auth.tenantId` was never applied: a
 * project-level sign-in would otherwise look completely normal here and be refused by
 * every request.
 */
export function tenantMismatch(user: IdentityUserSummary, expectedTenantId: string): string | null {
  if (user.tenantId === expectedTenantId) return null;
  // The configured tenant is named; the one the token carries is not. It is another
  // customer's identifier and does not belong on this screen.
  return `This account is not part of the configured Sentinel tenant (${expectedTenantId}). Sign in again.`;
}

/* ---------------------------------------------------- credential handover */

/**
 * What the currently held credential is, from the point of view of the decision below.
 */
export interface HeldCredential {
  /** Whose credential is held, or null when none is. */
  uid: string | null;
  /** False when there is no credential, or when its refresh has already failed. */
  live: boolean;
}

export type CredentialAction =
  /** Nobody is signed in: drop what is held and render the sign-in screen. */
  | { kind: 'clear' }
  /** Wrong tenant: drop the credential, say why, and sign out of the provider. */
  | { kind: 'reject'; message: string }
  /** Same user, credential still good: do nothing at all. */
  | { kind: 'keep' }
  /** A user we are not already holding a credential for: build one. */
  | { kind: 'adopt'; user: IdentityUserSummary };

/**
 * What to do when the identity provider reports a user.
 *
 * The subtlety this exists to pin down is that the provider notifies on *token*
 * changes, not user changes, so it fires every time the ID token rotates — including
 * for the rotations the portal's own token cache asked for. Treating one of those as a
 * new sign-in and rebuilding the cache is worse than wasteful: the replacement starts
 * with no token and no scheduled refresh, so proactive refreshing would quietly stop
 * after the first hour and every later expiry would be paid for by a failed request.
 * Hence `keep`, and hence this being a function with a test rather than three
 * conditions inside an effect.
 *
 * `live: false` deliberately does not mean `keep`: a cache whose refresh has already
 * failed is not a credential, so a provider that still reports the same user gets a
 * fresh cache and one more chance.
 */
export function credentialAction(
  user: IdentityUserSummary | null,
  expectedTenantId: string,
  held: HeldCredential,
): CredentialAction {
  if (user === null) return { kind: 'clear' };

  const mismatch = tenantMismatch(user, expectedTenantId);
  if (mismatch !== null) return { kind: 'reject', message: mismatch };

  if (held.uid === user.uid && held.live) return { kind: 'keep' };
  return { kind: 'adopt', user };
}

/* --------------------------------------------------------------- error copy */

/**
 * Operator-facing text for a failed sign-in.
 *
 * Wrong-password and no-such-user are folded into one message on purpose: answering
 * them separately turns the sign-in form into a directory of who works at the BPO,
 * which is a finding in a bank's security review. Identity Platform's own
 * `auth/invalid-credential` does the same folding for newer projects; this makes the
 * behaviour uniform regardless of which code comes back.
 */
export function signInErrorMessage(cause: unknown): string {
  const code = errorCodeOf(cause);
  switch (code) {
    case 'auth/invalid-email':
    case 'auth/invalid-credential':
    case 'auth/wrong-password':
    case 'auth/user-not-found':
      return 'That email address and password do not match an account.';
    case 'auth/missing-password':
      return 'Enter your password.';
    case 'auth/user-disabled':
      return 'This account has been disabled. Contact your administrator.';
    case 'auth/too-many-requests':
      return 'Too many attempts. Wait a few minutes and try again.';
    case 'auth/network-request-failed':
      return 'Could not reach the identity provider. Check your connection and try again.';
    case 'auth/popup-blocked':
      return 'The sign-in window was blocked. Allow pop-ups for this site, or ask for the redirect flow to be enabled.';
    case 'auth/popup-closed-by-user':
    case 'auth/cancelled-popup-request':
      return 'Sign-in was cancelled.';
    case 'auth/operation-not-allowed':
      return 'This sign-in method is not enabled for this tenant. Contact your administrator.';
    case 'auth/unauthorized-domain':
      return 'This portal address is not authorised for sign-in. Contact your administrator.';
    case 'auth/tenant-id-mismatch':
      return 'That account belongs to a different Sentinel tenant.';
    default:
      // Never render the provider's raw message: it can carry an email address, and
      // for federated failures it can carry the IdP's own response.
      return 'Sign-in failed. Try again, and contact your administrator if it keeps failing.';
  }
}

function errorCodeOf(cause: unknown): string | null {
  if (typeof cause !== 'object' || cause === null) return null;
  const code = (cause as { code?: unknown }).code;
  return typeof code === 'string' ? code : null;
}
