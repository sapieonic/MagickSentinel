import { describe, expect, it } from 'vitest';
import { readApiBaseUrl, readAuthConfig, signInMethods } from './config.js';
import type { Env } from './config.js';

/** The minimum a working deployment sets. */
const COMPLETE: Env = {
  VITE_IDENTITY_API_KEY: 'AIza-example',
  VITE_IDENTITY_AUTH_DOMAIN: 'sentinel-prod.firebaseapp.com',
  VITE_IDENTITY_PROJECT_ID: 'sentinel-prod',
  VITE_IDENTITY_TENANT_ID: 'bpo-alpha',
};

function config(env: Env) {
  const result = readAuthConfig(env);
  if (!result.ok) throw new Error(`expected a valid config, got: ${result.problems.join(' ')}`);
  return result.config;
}

function problems(env: Env): readonly string[] {
  const result = readAuthConfig(env);
  return result.ok ? [] : result.problems;
}

describe('readAuthConfig', () => {
  it('accepts a complete environment and defaults the optional values', () => {
    const parsed = config(COMPLETE);
    expect(parsed.tenantId).toBe('bpo-alpha');
    expect(parsed.federatedProviderId).toBeNull();
    // Password sign-in on by default and SSO off: a deployment that has said nothing
    // about OPEN-2 still has a way in, and has not had an SSO integration invented
    // for it.
    expect(parsed.passwordSignIn).toBe(true);
    expect(parsed.federatedFlow).toBe('popup');
    // Session persistence by default: a supervisor's desk on a collections floor is
    // frequently not one person's machine.
    expect(parsed.persistence).toBe('session');
  });

  it('refuses to run without a tenant, and says why', () => {
    const found = problems({ ...COMPLETE, VITE_IDENTITY_TENANT_ID: undefined });
    expect(found).toHaveLength(1);
    expect(found[0]).toContain('VITE_IDENTITY_TENANT_ID');
    // The reason matters more than the variable name: falling back to the
    // project-level user pool would sign users in outside the isolation boundary the
    // gateway enforces with the firebase.tenant claim.
    expect(found[0]).toContain('firebase.tenant');
  });

  it('never invents a tenant from the project id', () => {
    // The single most tempting fallback in this file, and the one that would silently
    // cross a customer boundary.
    const result = readAuthConfig({ ...COMPLETE, VITE_IDENTITY_TENANT_ID: '   ' });
    expect(result.ok).toBe(false);
  });

  it('reports every missing variable at once', () => {
    // Whoever is reading this is holding a deploy pipeline. Three missing variables
    // must not cost three deploys to discover.
    const found = problems({});
    expect(found.length).toBeGreaterThanOrEqual(4);
    for (const key of [
      'VITE_IDENTITY_API_KEY',
      'VITE_IDENTITY_AUTH_DOMAIN',
      'VITE_IDENTITY_PROJECT_ID',
      'VITE_IDENTITY_TENANT_ID',
    ]) {
      expect(found.some((problem) => problem.includes(key))).toBe(true);
    }
  });

  it('treats an empty string as unset rather than as a value', () => {
    expect(readAuthConfig({ ...COMPLETE, VITE_IDENTITY_API_KEY: '' }).ok).toBe(false);
  });

  it('accepts either federation protocol, because OPEN-2 is undecided', () => {
    // A SAML federation to Entra ID and a generic OIDC provider must be the same
    // amount of work from here; anything else amounts to guessing the answer.
    expect(config({ ...COMPLETE, VITE_IDENTITY_FEDERATED_PROVIDER_ID: 'saml.entra-alpha' }).federatedProviderId).toBe(
      'saml.entra-alpha',
    );
    expect(config({ ...COMPLETE, VITE_IDENTITY_FEDERATED_PROVIDER_ID: 'oidc.keycloak' }).federatedProviderId).toBe(
      'oidc.keycloak',
    );
  });

  it('rejects a provider id without Identity Platform’s protocol prefix', () => {
    const found = problems({ ...COMPLETE, VITE_IDENTITY_FEDERATED_PROVIDER_ID: 'entra' });
    expect(found[0]).toContain('saml.');
  });

  it('rejects a build with no way to sign in', () => {
    const found = problems({ ...COMPLETE, VITE_IDENTITY_PASSWORD_SIGN_IN: 'false' });
    expect(found.some((problem) => problem.includes('No sign-in method'))).toBe(true);
  });

  it('allows SSO-only when a federated provider is configured', () => {
    const parsed = config({
      ...COMPLETE,
      VITE_IDENTITY_PASSWORD_SIGN_IN: 'false',
      VITE_IDENTITY_FEDERATED_PROVIDER_ID: 'saml.entra-alpha',
    });
    expect(signInMethods(parsed)).toEqual(['federated']);
  });

  it('offers federated first when both routes are open', () => {
    // Mid-migration state. On a floor that has SSO the password form is the
    // break-glass path and must not be the obvious one.
    const parsed = config({ ...COMPLETE, VITE_IDENTITY_FEDERATED_PROVIDER_ID: 'saml.entra-alpha' });
    expect(signInMethods(parsed)).toEqual(['federated', 'password']);
  });

  it('reports a mistyped boolean instead of quietly picking a side', () => {
    expect(problems({ ...COMPLETE, VITE_IDENTITY_PASSWORD_SIGN_IN: 'yes' })[0]).toContain(
      'VITE_IDENTITY_PASSWORD_SIGN_IN',
    );
  });

  it('reports a mistyped persistence value rather than defaulting it', () => {
    // A typo that silently defaulted would be an invisible change to how long a
    // signed-in portal survives on a shared workstation.
    expect(problems({ ...COMPLETE, VITE_IDENTITY_PERSISTENCE: 'forever' })[0]).toContain('VITE_IDENTITY_PERSISTENCE');
  });

  it('reports a mistyped federated flow', () => {
    expect(problems({ ...COMPLETE, VITE_IDENTITY_FEDERATED_FLOW: 'iframe' })[0]).toContain(
      'VITE_IDENTITY_FEDERATED_FLOW',
    );
  });

  it('carries the customer label through for the sign-in screen', () => {
    const parsed = config({ ...COMPLETE, VITE_IDENTITY_TENANT_LABEL: 'Alpha Recoveries' });
    expect(parsed.tenantLabel).toBe('Alpha Recoveries');
  });

  it('labels the SSO button without exposing the provider id', () => {
    const parsed = config({ ...COMPLETE, VITE_IDENTITY_FEDERATED_PROVIDER_ID: 'saml.entra-alpha' });
    expect(parsed.federatedLabel).not.toContain('saml.');
    expect(config({ ...COMPLETE, VITE_IDENTITY_FEDERATED_LABEL: 'Sign in with Alpha SSO' }).federatedLabel).toBe(
      'Sign in with Alpha SSO',
    );
  });
});

describe('readApiBaseUrl', () => {
  it('falls back to the local gateway in a development build only', () => {
    expect(readApiBaseUrl({}, true)).toEqual({ ok: true, baseUrl: 'http://localhost:8080' });
  });

  it('requires the gateway to be named in a production build', () => {
    // The failure this prevents is a production bundle quietly talking to
    // localhost:8080: a blank screen, a console full of network errors, and an
    // afternoon to work out that nothing was misconfigured on the server at all.
    const result = readApiBaseUrl({}, false);
    expect(result).toEqual({
      ok: false,
      problem: 'VITE_API_BASE_URL is not set. A production build must name the gateway explicitly.',
    });
  });

  it('normalises to the origin and drops a trailing slash', () => {
    expect(readApiBaseUrl({ VITE_API_BASE_URL: 'https://api.sentinel.magickvoice.com/' }, false)).toEqual({
      ok: true,
      baseUrl: 'https://api.sentinel.magickvoice.com',
    });
  });

  it('rejects a value with a path, which would prefix every contract route', () => {
    const result = readApiBaseUrl({ VITE_API_BASE_URL: 'https://api.example/gateway' }, false);
    expect(result.ok).toBe(false);
  });

  it('rejects a value that is not an absolute URL', () => {
    for (const bad of ['api.example', '/v1', 'localhost:8080']) {
      expect(readApiBaseUrl({ VITE_API_BASE_URL: bad }, true).ok).toBe(false);
    }
  });

  it('rejects a scheme the browser will not fetch from', () => {
    expect(readApiBaseUrl({ VITE_API_BASE_URL: 'ws://api.example' }, false).ok).toBe(false);
  });

  it('keeps a non-default port, which local and staging gateways use', () => {
    expect(readApiBaseUrl({ VITE_API_BASE_URL: 'https://gw.internal:8443' }, false)).toEqual({
      ok: true,
      baseUrl: 'https://gw.internal:8443',
    });
  });
});
