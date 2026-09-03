/**
 * The real Identity Platform backend, on the Firebase JS SDK.
 *
 * This file is the only place in the portal that imports `firebase/*`. Everything
 * else programs against `IdentityBackend`, which is why the token lifecycle and the
 * auth state machine have tests and this adapter does not: it contains no decisions,
 * only the SDK calls that satisfy the interface, and a node test of it would be a test
 * of mocks. What it does contain is three sequencing requirements that are easy to get
 * wrong and expensive to get wrong:
 *
 *  1. **`auth.tenantId` is set before any sign-in call.** This is the multi-tenancy
 *     switch. With it unset, `signInWithEmailAndPassword` authenticates against the
 *     project-level user pool instead of the customer's tenant, and the resulting token
 *     carries no `firebase.tenant` claim — which the gateway refuses, but only after
 *     the user believes they signed in. One Identity Platform tenant per BPO customer
 *     is the hard isolation boundary described in `server/gateway/internal/auth`, and
 *     it only exists if this assignment happens first.
 *  2. **Persistence is chosen before sign-in.** Firebase applies persistence to the
 *     session it creates, so setting it afterwards does nothing.
 *  3. **The redirect result is consumed at boot.** In the redirect flow the credential
 *     (or the IdP's error) arrives in the URL of a fresh page load, and it is lost if
 *     nobody asks for it.
 *
 * The `apiKey`, `authDomain` and `projectId` here are not credentials — every Firebase
 * web app ships them — and they come from `VITE_*` configuration rather than being
 * hard-coded, so one bundle can be pointed at development, staging and each customer's
 * production tenant.
 */
import { getApp, getApps, initializeApp } from 'firebase/app';
import {
  OAuthProvider,
  SAMLAuthProvider,
  browserLocalPersistence,
  browserSessionPersistence,
  getAuth,
  getRedirectResult,
  onIdTokenChanged,
  setPersistence,
  signInWithEmailAndPassword,
  signInWithPopup,
  signInWithRedirect,
  signOut as firebaseSignOut,
} from 'firebase/auth';
import type { Auth, AuthProvider, User } from 'firebase/auth';
import type { PortalAuthConfig } from './config.js';
import { signInErrorMessage } from './identity.js';
import type { IdentityBackend, IdentityUser, StartResult } from './identity.js';

export function createFirebaseIdentityBackend(config: PortalAuthConfig): IdentityBackend {
  // Reuse an existing app across a Vite hot reload; `initializeApp` twice with the
  // same name throws and would break the dev server on every save.
  const app = getApps().length > 0 ? getApp() : initializeApp({
    apiKey: config.apiKey,
    authDomain: config.authDomain,
    projectId: config.projectId,
  });

  const auth = getAuth(app);
  // Requirement 1. Assigned here, at construction, so there is no code path that can
  // reach a sign-in call with the project-level pool still selected.
  auth.tenantId = config.tenantId;

  return {
    async start(): Promise<StartResult> {
      await applyPersistence(auth, config);

      let signInError: string | null = null;
      if (config.federatedFlow === 'redirect') {
        try {
          // Requirement 3. The return value is ignored: a successful credential also
          // reaches us through onIdTokenChanged, and this call's real job is to
          // consume the pending state and surface the IdP's rejection.
          await getRedirectResult(auth);
        } catch (cause) {
          signInError = signInErrorMessage(cause);
        }
      }

      // Resolves once Firebase has restored (or failed to restore) a persisted
      // session. Without this the portal would render the sign-in screen for a beat
      // before swapping to the app, and an operator who is already signed in would
      // see a form they must not fill in.
      await auth.authStateReady();
      return { signInError };
    },

    onUserChanged(callback: (user: IdentityUser | null) => void): () => void {
      // onIdTokenChanged rather than onAuthStateChanged: it also fires when the SDK
      // rotates the token, which keeps the portal's idea of the credential and the
      // SDK's from drifting apart over an eight-hour shift.
      return onIdTokenChanged(auth, (user) => callback(user === null ? null : toIdentityUser(user)));
    },

    async signInWithPassword(email: string, password: string): Promise<void> {
      await signInWithEmailAndPassword(auth, email, password);
    },

    async signInWithFederatedProvider(): Promise<void> {
      const providerId = config.federatedProviderId;
      if (providerId === null) {
        // Unreachable through the UI — the button is not rendered without a provider
        // — but throwing beats silently doing nothing if that ever stops being true.
        throw new Error('portal: no federated provider is configured');
      }
      const provider = federatedProvider(providerId);
      if (config.federatedFlow === 'popup') {
        await signInWithPopup(auth, provider);
        return;
      }
      // Navigates away; nothing after this line runs. The result is picked up by
      // `start()` on the way back in.
      await signInWithRedirect(auth, provider);
    },

    async signOut(): Promise<void> {
      await firebaseSignOut(auth);
    },
  };
}

/* --------------------------------------------------------------- internals */

async function applyPersistence(auth: Auth, config: PortalAuthConfig): Promise<void> {
  // Requirement 2. `session` is the default in config.ts: portal users are
  // supervisors, QA, compliance and bank staff, and a supervisor's workstation on a
  // collections floor is frequently not a single person's machine. A session that
  // dies with the tab is the safer default there; a floor that wants convenience opts
  // into `local` explicitly.
  const target = config.persistence === 'local' ? browserLocalPersistence : browserSessionPersistence;
  try {
    await setPersistence(auth, target);
  } catch {
    // Storage can be unavailable outright — a locked-down browser profile, private
    // browsing, a group policy blocking site data. Firebase then falls back to
    // in-memory persistence, which means the session ends with the page rather than
    // with the tab. That is stricter than what was asked for, never looser, so it is
    // safe to continue: refusing to sign in at all would be a worse answer than
    // asking the user to sign in again after a reload.
  }
}

function toIdentityUser(user: User): IdentityUser {
  return {
    uid: user.uid,
    email: user.email,
    displayName: user.displayName,
    // Carried through so `tenantMismatch` can refuse a credential from another
    // tenant before it is ever used against the gateway.
    tenantId: user.tenantId,
    // Bound to this user object rather than to `auth.currentUser`: if the user
    // changes underneath us, the old cache must fail rather than silently start
    // minting tokens for the new one.
    getIdToken: (forceRefresh: boolean) => user.getIdToken(forceRefresh),
  };
}

/**
 * SAML and OIDC, chosen by the provider id's prefix — which is Identity Platform's own
 * namespacing, not a convention invented here. This is the whole of what OPEN-2
 * decides: a SAML federation to Entra ID is `saml.<name>`, a generic OIDC provider is
 * `oidc.<name>`, and both arrive at the same tenant with the same custom claims.
 */
function federatedProvider(providerId: string): AuthProvider {
  if (providerId.startsWith('saml.')) return new SAMLAuthProvider(providerId);
  const provider = new OAuthProvider(providerId);
  // Force the account chooser. On a shared supervisor desktop, silently reusing
  // whoever the browser last signed into the IdP as is how one person's review ends
  // up in another person's audit trail — and every read of call content is audited
  // against the token's subject (`Store.GetCall`).
  provider.setCustomParameters({ prompt: 'select_account' });
  // No extra scopes are requested. What the token carries is decided by the tenant's
  // provider configuration and its claim mapping — `tenant_id`, `role` and `team_id`
  // are custom claims set by provisioning, not OAuth scopes — so asking for more here
  // would only add to a consent screen without adding anything to the token.
  return provider;
}
