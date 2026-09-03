/**
 * The sign-in screen, and the configuration-failure screen that replaces it when the
 * build cannot sign anyone in.
 *
 * Which controls appear is decided entirely by configuration (`signInMethods`), never
 * by anything this file knows about the customer. OPEN-2 — whether the floor
 * authenticates through Entra ID or through an email and password Sentinel owns — is
 * still open, and this screen renders either answer, or both at once during a
 * migration, without a code change.
 *
 * The tenant is named on screen. That is not decoration: one Identity Platform tenant
 * per BPO customer is the isolation boundary, and an operator who has been handed the
 * wrong portal URL should be able to see that before typing a password into it.
 */
import { useState } from 'react';
import type { PortalAuthConfig } from '../auth/config.js';
import { usePortalAuth } from '../auth/AuthProvider.js';

export function SignIn() {
  const { phase, config, methods, signInWithPassword, signInWithFederatedProvider } = usePortalAuth();
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');

  // Rendered only from the signed-out phase; anything else is a wiring mistake
  // upstream and there is nothing sensible to draw.
  if (phase.kind !== 'signed_out' || config === null) return null;
  const busy = phase.busy;

  return (
    <div className="pt-signin">
      <form
        className="pt-signin__card"
        onSubmit={(event) => {
          event.preventDefault();
          if (!busy) void signInWithPassword(email, password);
        }}
      >
        <h1 className="pt-signin__brand">Sentinel</h1>
        <p className="pt-signin__tenant sx-muted">{tenantLine(config)}</p>

        {/* Assertive: a failed sign-in is the one thing on this screen the operator
            must not miss, and a screen reader user tabbing back to the password field
            would otherwise never hear it. */}
        {phase.error !== null ? (
          <p className="sx-error pt-signin__error" role="alert">
            {phase.error}
          </p>
        ) : null}

        {methods.includes('federated') ? (
          <button
            type="button"
            className="sx-btn--primary pt-signin__federated"
            disabled={busy}
            onClick={() => void signInWithFederatedProvider()}
          >
            {busy ? 'Opening your identity provider…' : config.federatedLabel}
          </button>
        ) : null}

        {methods.length > 1 ? <p className="pt-signin__or sx-muted">or</p> : null}

        {methods.includes('password') ? (
          <>
            <label className="pt-signin__field">
              Work email
              <input
                type="email"
                autoComplete="username"
                required
                disabled={busy}
                value={email}
                onChange={(event) => setEmail(event.target.value)}
              />
            </label>
            <label className="pt-signin__field">
              Password
              <input
                type="password"
                autoComplete="current-password"
                required
                disabled={busy}
                value={password}
                onChange={(event) => setPassword(event.target.value)}
              />
            </label>
            <button type="submit" className="sx-btn--primary pt-signin__submit" disabled={busy}>
              {busy ? 'Signing in…' : 'Sign in'}
            </button>
          </>
        ) : null}

        {/* No "forgot password" link. Password resets are the tenant administrator's
            job in Identity Platform, and a self-service reset flow is a decision
            about the customer's directory that OPEN-2 has not made yet. */}
      </form>
    </div>
  );
}

function tenantLine(config: PortalAuthConfig): string {
  return config.tenantLabel !== null
    ? `${config.tenantLabel} · ${config.tenantId}`
    : `Tenant ${config.tenantId}`;
}

/**
 * Shown instead of the sign-in screen when the deployment is missing configuration.
 *
 * It lists variable names rather than paraphrasing them, because the person reading it
 * is holding a deploy pipeline and needs the exact key. None of these values are
 * secrets — the Identity Platform web configuration ships in every Firebase app's
 * bundle — so naming them costs nothing.
 */
export function ConfigProblem({ problems }: { problems: readonly string[] }) {
  return (
    <div className="pt-signin">
      <div className="pt-signin__card">
        <h1 className="pt-signin__brand">Sentinel</h1>
        <p className="sx-error pt-signin__error" role="alert">
          This portal is not configured and cannot sign anyone in.
        </p>
        <ul className="pt-signin__problems">
          {problems.map((problem) => (
            <li key={problem}>{problem}</li>
          ))}
        </ul>
        <p className="sx-muted pt-signin__hint">
          Set the missing values in the portal’s build environment and redeploy. See
          <code> web/portal/.env.example</code>.
        </p>
      </div>
    </div>
  );
}
