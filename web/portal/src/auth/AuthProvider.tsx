/**
 * Portal sign-in, wired up.
 *
 * Owns three things and nothing else: the auth phase (`reduceAuth`), the ID token
 * cache for the currently signed-in user (`IdTokenCache`), and the handlers the
 * sign-in screen calls. The API client and the gateway session live one layer down in
 * `session.tsx`, which is only mounted once there is a credential to give it — so no
 * screen in the portal can ever issue a request while signed out.
 *
 * Two invariants worth stating, because breaking either is silent:
 *
 *  - **`getToken` and `refreshToken` keep a stable identity for the life of the
 *    provider.** `session.tsx` builds its `ApiClient` in a `useMemo` keyed on them; a
 *    new function each render would rebuild the client, which re-runs
 *    `POST /v1/sessions` and re-renders, which builds another one. They read the
 *    current token cache out of a ref instead of closing over it.
 *  - **The token cache is disposed whenever the user changes.** It holds a refresh
 *    timer bound to one Firebase user. A cache left running after a sign-out keeps
 *    minting tokens for whoever just left the desk, which on a shared supervisor
 *    workstation is exactly the thing that must not happen.
 */
import { createContext, useCallback, useContext, useEffect, useMemo, useReducer, useRef } from 'react';
import type { ReactNode } from 'react';
import { signInMethods } from './config.js';
import type { ConfigResult, PortalAuthConfig, SignInMethod } from './config.js';
import { credentialAction, reduceAuth, signInErrorMessage, summarise } from './identity.js';
import type { AuthPhase, IdentityBackend } from './identity.js';
import { IdTokenCache } from './tokens.js';

export interface PortalAuth {
  phase: AuthPhase;
  /** Null only when the build is misconfigured. */
  config: PortalAuthConfig | null;
  /** Which sign-in routes this deployment offers; empty when misconfigured. */
  methods: readonly SignInMethod[];
  /** Stable across renders. Null while signed out, which becomes `no_credentials`. */
  getToken: () => Promise<string | null>;
  /** Stable across renders. Forced refresh for the API client's one 401 retry. */
  refreshToken: () => Promise<string | null>;
  /** Never rejects: failures land in `phase` as text the screen can render. */
  signInWithPassword: (email: string, password: string) => Promise<void>;
  signInWithFederatedProvider: () => Promise<void>;
  signOut: () => Promise<void>;
}

const AuthContext = createContext<PortalAuth | null>(null);

export function AuthProvider({
  configResult,
  createBackend,
  children,
}: {
  configResult: ConfigResult;
  createBackend: (config: PortalAuthConfig) => IdentityBackend;
  children: ReactNode;
}) {
  const config = configResult.ok ? configResult.config : null;

  const [phase, dispatch] = useReducer(
    reduceAuth,
    configResult,
    (result): AuthPhase =>
      result.ok ? { kind: 'starting' } : { kind: 'misconfigured', problems: result.problems },
  );

  const backend = useMemo(
    () => (config === null ? null : createBackend(config)),
    // `createBackend` is deliberately not a dependency: it is a constructor passed
    // from the module scope of main.tsx, and treating it as reactive would rebuild
    // the Firebase app — and drop the signed-in session — on any re-render that
    // happened to pass a fresh closure.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [config],
  );

  const cacheRef = useRef<IdTokenCache | null>(null);
  /** Whose credential `cacheRef` holds, so a token rotation is not mistaken for a new user. */
  const uidRef = useRef<string | null>(null);

  useEffect(() => {
    if (backend === null || config === null) return;
    let cancelled = false;

    const swapCache = (uid: string | null, next: IdTokenCache | null) => {
      cacheRef.current?.dispose();
      cacheRef.current = next;
      uidRef.current = uid;
    };

    // Subscribed before `start()` so a session restored during startup cannot land
    // between the two calls and be missed.
    const unsubscribe = backend.onUserChanged((user) => {
      if (cancelled) return;

      const held = cacheRef.current;
      const action = credentialAction(user === null ? null : summarise(user), config.tenantId, {
        uid: uidRef.current,
        live: held !== null && !held.lost,
      });

      switch (action.kind) {
        case 'keep':
          // A token rotation for the user we already hold. See `credentialAction`.
          return;

        case 'clear':
          swapCache(null, null);
          dispatch({ type: 'user', user: null });
          return;

        case 'reject':
          // Defence in depth against a build where the tenant was not applied. Drop
          // the credential rather than keeping a session the gateway will refuse on
          // every request with no explanation the operator can read.
          swapCache(null, null);
          dispatch({ type: 'credentials_lost', message: action.message });
          void backend.signOut();
          return;

        case 'adopt': {
          // Unreachable with a null user — `credentialAction` only adopts a user it
          // was given — but narrowing it here beats a non-null assertion.
          if (user === null) return;
          swapCache(
            action.user.uid,
            new IdTokenCache(user.getIdToken, {
              onLost: (reason) => {
                if (cancelled) return;
                // A failed refresh ends the session. Signing out of the provider too
                // keeps its state and ours from disagreeing about who is signed in.
                dispatch({ type: 'credentials_lost', message: reason });
                void backend.signOut();
              },
            }),
          );
          dispatch({ type: 'user', user: action.user });
          return;
        }
      }
    });

    void backend
      .start()
      .then(({ signInError }) => {
        if (cancelled || signInError === null) return;
        // A redirect-flow rejection: reported on the page load *after* the attempt,
        // so it arrives here rather than from the click that caused it.
        dispatch({ type: 'sign_in_failed', message: signInError });
      })
      .catch(() => {
        if (cancelled) return;
        // `start` failing means the SDK could not initialise or reach the provider.
        // Signed-out with an explanation is the honest rendering; "starting" forever
        // is a spinner nobody can get out of.
        dispatch({
          type: 'sign_in_failed',
          message: 'Could not reach the identity provider. Check your connection and reload.',
        });
      });

    return () => {
      cancelled = true;
      unsubscribe();
      swapCache(null, null);
    };
  }, [backend, config]);

  // Both read the ref at call time, so their identity never changes. See the header.
  const getToken = useCallback(() => cacheRef.current?.get() ?? Promise.resolve(null), []);
  const refreshToken = useCallback(() => cacheRef.current?.refresh() ?? Promise.resolve(null), []);

  const signInWithPassword = useCallback(
    async (email: string, password: string) => {
      if (backend === null) return;
      dispatch({ type: 'sign_in_started' });
      try {
        await backend.signInWithPassword(email, password);
        // Success is not dispatched here. The signed-in phase comes from
        // `onUserChanged`, which is also the path a restored session takes, so there
        // is one way into `signed_in` rather than two that can disagree.
      } catch (cause) {
        dispatch({ type: 'sign_in_failed', message: signInErrorMessage(cause) });
      }
    },
    [backend],
  );

  const signInWithFederatedProvider = useCallback(async () => {
    if (backend === null) return;
    dispatch({ type: 'sign_in_started' });
    try {
      await backend.signInWithFederatedProvider();
    } catch (cause) {
      dispatch({ type: 'sign_in_failed', message: signInErrorMessage(cause) });
    }
  }, [backend]);

  const signOut = useCallback(async () => {
    // Local state first, and unconditionally. If the provider's sign-out fails —
    // offline, for instance — the portal must still end up signed out: an operator
    // who clicked "sign out" and walked away must not leave a usable session behind.
    cacheRef.current?.dispose();
    cacheRef.current = null;
    uidRef.current = null;
    dispatch({ type: 'sign_out' });
    try {
      await backend?.signOut();
    } catch {
      // Nothing to tell the user: as far as this portal is concerned they are out.
    }
  }, [backend]);

  const value = useMemo<PortalAuth>(
    () => ({
      phase,
      config,
      methods: config === null ? [] : signInMethods(config),
      getToken,
      refreshToken,
      signInWithPassword,
      signInWithFederatedProvider,
      signOut,
    }),
    [phase, config, getToken, refreshToken, signInWithPassword, signInWithFederatedProvider, signOut],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function usePortalAuth(): PortalAuth {
  const value = useContext(AuthContext);
  if (!value) throw new Error('usePortalAuth used outside AuthProvider');
  return value;
}
