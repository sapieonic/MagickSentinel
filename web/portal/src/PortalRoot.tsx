/**
 * What the portal renders, decided by the auth phase.
 *
 * The important structural property is that `SessionProvider` — and therefore the
 * `ApiClient` and everything under it — is mounted only in the signed-in phase. A
 * portal that mounted its screens first and let them discover they had no credential
 * would fire a `POST /v1/sessions` and a handful of listings on every page load by a
 * signed-out visitor, each failing, each rendering its own error. Gating here means
 * "signed out" is one screen with one button rather than an error per panel.
 *
 * Sign-out and credential loss come back to the same place: the signed-out phase,
 * which renders the sign-in screen carrying whatever explanation the auth layer had.
 */
import { App } from './App.js';
import { usePortalAuth } from './auth/AuthProvider.js';
import { ConfigProblem, SignIn } from './screens/SignIn.js';
import { SessionProvider } from './session.js';

export function PortalRoot({ baseUrl }: { baseUrl: string }) {
  const { phase, getToken, refreshToken, signOut } = usePortalAuth();

  switch (phase.kind) {
    case 'misconfigured':
      return <ConfigProblem problems={phase.problems} />;

    case 'starting':
      // Deliberately not the sign-in screen. Identity Platform restores a persisted
      // session asynchronously, and flashing a password form at someone who is
      // already signed in trains them to start typing before the page settles.
      return <p className="pt-boot sx-muted">Starting Sentinel…</p>;

    case 'signed_out':
      return <SignIn />;

    case 'signed_in':
      return (
        <SessionProvider
          baseUrl={baseUrl}
          getToken={getToken}
          refreshToken={refreshToken}
          // The gateway rejecting the credential outranks anything the identity
          // provider believes: if a refreshed token still cannot start a session, the
          // portal returns to signed-out rather than sitting on an error screen with
          // a session the server will not honour.
          onCredentialsLost={signOut}
        >
          <App />
        </SessionProvider>
      );
  }
}
