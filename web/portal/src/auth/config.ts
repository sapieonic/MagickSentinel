/**
 * Portal authentication configuration, read from the Vite environment.
 *
 * Everything here is deliberately data rather than code, for two reasons.
 *
 * **Multi-tenancy is the isolation boundary.** The gateway verifies the
 * `firebase.tenant` claim on every ID token (`server/gateway/internal/auth/auth.go`),
 * and one Identity Platform tenant per BPO customer is what makes cross-customer
 * access impossible at the auth layer rather than merely unlikely at the query layer.
 * A portal build that guessed its tenant — or that fell back to the project-level
 * user pool when the tenant was not configured — would sign users in outside that
 * boundary and hand them tokens the gateway cannot place. So `VITE_IDENTITY_TENANT_ID`
 * is required, has no default, and a missing value is a hard configuration failure
 * that renders an explicit screen. Never add a fallback here.
 *
 * **OPEN-2 is not ours to settle.** `docs/open-decisions.md` records that whether the
 * customer runs Entra ID is undecided, and the gateway's token verification is
 * provider-agnostic precisely so the build does not have to guess. Both routes into
 * the same Identity Platform tenant are therefore supported side by side and chosen
 * by configuration: a SAML/OIDC federated provider (`VITE_IDENTITY_FEDERATED_PROVIDER_ID`)
 * and an email/password credential (`VITE_IDENTITY_PASSWORD_SIGN_IN`). Turning either
 * on or off is an env change, not a code change, and answering OPEN-2 in either
 * direction should require no edit to this file.
 *
 * The Identity Platform web API key, auth domain and project id are not secrets —
 * they identify the project, and every Firebase web app ships them in its bundle. The
 * secret is the ID token, and it never leaves the browser except as a bearer header.
 */

/** The one env var that predates this file; kept for compatibility. */
export const API_BASE_URL_VAR = 'VITE_API_BASE_URL';

export type SignInMethod = 'federated' | 'password';

/** How the federated round trip leaves and re-enters the page. */
export type FederatedFlow = 'popup' | 'redirect';

/** How long a signed-in session outlives the tab. */
export type SessionPersistence = 'session' | 'local';

export interface PortalAuthConfig {
  /** Identity Platform web API key (`apiKey` in the Firebase SDK's config). */
  apiKey: string;
  /** e.g. `sentinel-prod.firebaseapp.com`; also the OAuth redirect host. */
  authDomain: string;
  /**
   * GCP project id. The gateway's expected issuer is
   * `https://securetoken.google.com/<projectId>`, so this and
   * `SENTINEL_FIREBASE_PROJECT` on the gateway must name the same project or every
   * token is refused with `token_invalid`.
   */
  projectId: string;
  /** The BPO customer's Identity Platform tenant. Required; see the file comment. */
  tenantId: string;
  /** Customer name for the sign-in screen, so an operator can see which tenant they are entering. */
  tenantLabel: string | null;
  /** SAML (`saml.*`) or OIDC (`oidc.*`) provider id, or null when not federated. */
  federatedProviderId: string | null;
  /** Button label; the provider id is an internal identifier and must not be shown. */
  federatedLabel: string;
  passwordSignIn: boolean;
  federatedFlow: FederatedFlow;
  persistence: SessionPersistence;
}

export type ConfigResult =
  | { ok: true; config: PortalAuthConfig }
  | { ok: false; problems: readonly string[] };

/** Vite's `import.meta.env` shape, narrowed to what this module reads. */
export type Env = Record<string, string | boolean | undefined>;

const PROVIDER_ID_PATTERN = /^(saml|oidc)\.[A-Za-z0-9][A-Za-z0-9._-]*$/;

/**
 * Validates the whole environment at once and returns every problem it found.
 *
 * All-at-once rather than throwing on the first miss: whoever is deploying this is
 * looking at a screen, and telling them about three missing variables one deploy at a
 * time is three deploys.
 */
export function readAuthConfig(env: Env): ConfigResult {
  const problems: string[] = [];

  const apiKey = str(env, 'VITE_IDENTITY_API_KEY');
  const authDomain = str(env, 'VITE_IDENTITY_AUTH_DOMAIN');
  const projectId = str(env, 'VITE_IDENTITY_PROJECT_ID');
  const tenantId = str(env, 'VITE_IDENTITY_TENANT_ID');

  if (apiKey === null) problems.push('VITE_IDENTITY_API_KEY is not set.');
  if (authDomain === null) problems.push('VITE_IDENTITY_AUTH_DOMAIN is not set.');
  if (projectId === null) problems.push('VITE_IDENTITY_PROJECT_ID is not set.');
  if (tenantId === null) {
    problems.push(
      'VITE_IDENTITY_TENANT_ID is not set. The portal will not sign in without a ' +
        'customer tenant: the gateway checks the firebase.tenant claim, and signing ' +
        'in against the project-level user pool would cross the isolation boundary.',
    );
  }

  const federatedProviderId = str(env, 'VITE_IDENTITY_FEDERATED_PROVIDER_ID');
  if (federatedProviderId !== null && !PROVIDER_ID_PATTERN.test(federatedProviderId)) {
    // Identity Platform namespaces federated providers by protocol. Accepting both
    // prefixes and validating nothing else is what keeps OPEN-2 open: a SAML
    // federation to Entra ID and a generic OIDC provider are the same amount of work
    // from here.
    problems.push(
      `VITE_IDENTITY_FEDERATED_PROVIDER_ID must start with "saml." or "oidc." (got "${federatedProviderId}").`,
    );
  }

  const passwordSignIn = bool(env, 'VITE_IDENTITY_PASSWORD_SIGN_IN', true);
  if (passwordSignIn === null) {
    problems.push('VITE_IDENTITY_PASSWORD_SIGN_IN must be "true" or "false".');
  }

  const federatedFlow = choice<FederatedFlow>(env, 'VITE_IDENTITY_FEDERATED_FLOW', ['popup', 'redirect'], 'popup');
  if (federatedFlow === null) {
    problems.push('VITE_IDENTITY_FEDERATED_FLOW must be "popup" or "redirect".');
  }

  const persistence = choice<SessionPersistence>(env, 'VITE_IDENTITY_PERSISTENCE', ['session', 'local'], 'session');
  if (persistence === null) {
    problems.push('VITE_IDENTITY_PERSISTENCE must be "session" or "local".');
  }

  // A build with neither method enabled renders a sign-in screen with no way to sign
  // in. That is a deployment mistake rather than a state the UI should handle, so it
  // is caught here where it can be named.
  if (federatedProviderId === null && passwordSignIn === false) {
    problems.push(
      'No sign-in method is enabled: set VITE_IDENTITY_FEDERATED_PROVIDER_ID, or leave ' +
        'VITE_IDENTITY_PASSWORD_SIGN_IN at its default of true.',
    );
  }

  if (
    problems.length > 0 ||
    apiKey === null ||
    authDomain === null ||
    projectId === null ||
    tenantId === null ||
    passwordSignIn === null ||
    federatedFlow === null ||
    persistence === null
  ) {
    return { ok: false, problems };
  }

  return {
    ok: true,
    config: {
      apiKey,
      authDomain,
      projectId,
      tenantId,
      tenantLabel: str(env, 'VITE_IDENTITY_TENANT_LABEL'),
      federatedProviderId,
      federatedLabel: str(env, 'VITE_IDENTITY_FEDERATED_LABEL') ?? 'Sign in with your work account',
      passwordSignIn,
      federatedFlow,
      persistence,
    },
  };
}

/** Which buttons the sign-in screen offers, in the order it offers them. */
export function signInMethods(config: PortalAuthConfig): readonly SignInMethod[] {
  const methods: SignInMethod[] = [];
  // Federated first when both are available: on a floor that has SSO, the password
  // form is the break-glass path and should not be the obvious one.
  if (config.federatedProviderId !== null) methods.push('federated');
  if (config.passwordSignIn) methods.push('password');
  return methods;
}

export type BaseUrlResult = { ok: true; baseUrl: string } | { ok: false; problem: string };

/**
 * The gateway origin.
 *
 * The localhost fallback is kept, but only for a development build. In a production
 * bundle an unset `VITE_API_BASE_URL` used to mean "silently talk to localhost:8080",
 * which is the failure that presents as a blank screen with a console full of network
 * errors and takes an afternoon to diagnose. Requiring it in a production build turns
 * that into a named configuration error at boot.
 *
 * The value is also half of a CORS agreement — the other half is
 * `SENTINEL_ALLOWED_ORIGINS` on the gateway, which must list the origin this portal is
 * served from. Nothing here can verify that; the client's transport error names the
 * origin it failed to reach so that the mismatch is at least diagnosable.
 */
export function readApiBaseUrl(env: Env, isDev: boolean): BaseUrlResult {
  const configured = str(env, API_BASE_URL_VAR);
  if (configured === null) {
    if (!isDev) {
      return {
        ok: false,
        problem: `${API_BASE_URL_VAR} is not set. A production build must name the gateway explicitly.`,
      };
    }
    // The local-development server from contracts/openapi.yaml.
    return { ok: true, baseUrl: 'http://localhost:8080' };
  }

  let parsed: URL;
  try {
    parsed = new URL(configured);
  } catch {
    return { ok: false, problem: `${API_BASE_URL_VAR} is not a valid absolute URL: "${configured}".` };
  }
  if (parsed.protocol !== 'https:' && parsed.protocol !== 'http:') {
    return { ok: false, problem: `${API_BASE_URL_VAR} must be http or https (got "${parsed.protocol}").` };
  }
  // Trailing slashes are stripped by ApiClient, but a path component is a different
  // mistake — it would silently prefix every contract route and 404 the lot.
  const path = parsed.pathname.replace(/\/+$/, '');
  if (path !== '') {
    return {
      ok: false,
      problem: `${API_BASE_URL_VAR} must be an origin with no path (got "${configured}").`,
    };
  }
  return { ok: true, baseUrl: parsed.origin };
}

/* --------------------------------------------------------------- internals */

/** Trims, and folds the empty string into "not set" — an empty env var is not a value. */
function str(env: Env, key: string): string | null {
  const raw = env[key];
  if (typeof raw !== 'string') return null;
  const trimmed = raw.trim();
  return trimmed === '' ? null : trimmed;
}

/** null means "set to something that is not a boolean", which is a problem to report. */
function bool(env: Env, key: string, fallback: boolean): boolean | null {
  const raw = str(env, key);
  if (raw === null) return fallback;
  if (raw === 'true') return true;
  if (raw === 'false') return false;
  return null;
}

/**
 * null means "set to an unrecognised value". Reported rather than defaulted: a typo
 * in `VITE_IDENTITY_PERSISTENCE` that quietly picked a default would be a silent
 * change to how long a signed-in portal survives on a shared workstation.
 */
function choice<T extends string>(env: Env, key: string, allowed: readonly T[], fallback: T): T | null {
  const raw = str(env, key);
  if (raw === null) return fallback;
  return allowed.includes(raw as T) ? (raw as T) : null;
}
