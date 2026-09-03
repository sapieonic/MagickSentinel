/**
 * Portal session: who is signed in, what the tenant policy allows, the teams the
 * caller can name, and the one ApiClient every screen uses.
 *
 * The portal has its own Identity Platform session (the widget's token comes from the
 * native layer instead), so the credential arrives through two functions rather than
 * being stored here: `getToken`, called per request, and `refreshToken`, called by
 * `ApiClient` after a 401. Both are seams — `auth/AuthProvider.tsx` supplies the real
 * implementations and a test can supply its own — which is why this file has no
 * knowledge of Firebase at all.
 *
 * This layer is mounted only when the auth layer says someone is signed in (see
 * `PortalRoot`), so `getToken` returning null here means the session ended *during*
 * use: a refresh that failed, or an administrator disabling the account mid-shift.
 * That surfaces as a `no_credentials` failure rather than a 401, and the response is
 * to hand control back to the auth layer through `onCredentialsLost` — never to retry.
 * Retrying is the 401 loop this comment has warned about since before there was
 * anything behind the seam.
 */
import { createContext, useContext, useEffect, useMemo, useState } from 'react';
import type { ReactNode } from 'react';
import { ApiClient, ApiError, canPlayAudio } from '@sentinel/shared';
import type { Capability, Policy, Role, Team, User } from '@sentinel/shared';
import { can } from '@sentinel/shared';

export interface Session {
  user: User;
  policy: Policy;
}

interface SessionContextValue {
  api: ApiClient;
  session: Session | null;
  loading: boolean;
  error: string | null;
  role: Role | null;
  can: (capability: Capability) => boolean;
  /** Applies the two policy-gated cells of spec 13.4's matrix. */
  canPlayAudio: boolean;
  /** Empty until the listing resolves, and for roles that never name a team. */
  teams: readonly Team[];
  /** False only while the listing is in flight, so "none" is never shown too early. */
  teamsResolved: boolean;
  /** A team's name, or the raw id when it is not in the listing. */
  teamName: (id: string | null | undefined) => string;
}

const SessionContext = createContext<SessionContextValue | null>(null);

/**
 * The token seam. Kept null-safe so a signed-out portal produces a clean
 * `no_credentials` error rather than a 401 loop.
 */
export type PortalTokenProvider = () => string | null | Promise<string | null>;

/**
 * Mints a new token, bypassing any cache. Optional: without it a mid-session expiry
 * surfaces as a 401 the user has to resolve by reloading, which is survivable but
 * needless. See `ApiClient`'s retry rule.
 */
export type PortalTokenRefresher = () => string | null | Promise<string | null>;

export function SessionProvider({
  children,
  baseUrl,
  getToken,
  refreshToken,
  onCredentialsLost,
}: {
  children: ReactNode;
  baseUrl: string;
  getToken: PortalTokenProvider;
  refreshToken?: PortalTokenRefresher;
  /** Called when the credential is gone rather than merely rejected. */
  onCredentialsLost?: () => void;
}) {
  const api = useMemo(
    () =>
      new ApiClient({
        baseUrl,
        getToken,
        ...(refreshToken ? { refreshToken } : {}),
      }),
    [baseUrl, getToken, refreshToken],
  );
  const [session, setSession] = useState<Session | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [teams, setTeams] = useState<readonly Team[]>([]);
  const [teamsResolved, setTeamsResolved] = useState(false);

  useEffect(() => {
    const controller = new AbortController();
    void api
      .createSession(controller.signal)
      .then((response) => setSession({ user: response.user, policy: response.policy }))
      .catch((cause: unknown) => {
        if (controller.signal.aborted) return;
        if (cause instanceof ApiError && cause.isMissingCredentials) {
          // The credential went away between mounting and this request — ApiClient
          // already tried a forced refresh. There is nothing to show here; the auth
          // layer owns the signed-out screen, so hand control back rather than
          // rendering a second, competing "sign in again" message.
          onCredentialsLost?.();
          return;
        }
        setError(sessionErrorMessage(cause, baseUrl));
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
    // `onCredentialsLost` is intentionally not a dependency: it is a stable callback
    // from the auth provider, and including it would re-run POST /v1/sessions if it
    // were ever passed as an inline closure.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [api, baseUrl]);

  const role = session?.user.role ?? null;
  // Only roles that work across teams ask for the listing. An agent has no use for
  // the tenant's team roster and asking for it would put a list of every team on
  // the floor into a screen that never shows one.
  const wantsTeams = can(role, 'team_calls') || can(role, 'manage_devices_users');

  useEffect(() => {
    if (!wantsTeams) {
      setTeams([]);
      setTeamsResolved(true);
      return;
    }
    const controller = new AbortController();
    void api
      .listTeams(controller.signal)
      .then((found) => setTeams(found))
      // A failed team listing degrades a label to a raw id; it must not take the
      // screen down, so nothing is surfaced here.
      .catch(() => undefined)
      .finally(() => {
        if (!controller.signal.aborted) setTeamsResolved(true);
      });
    return () => controller.abort();
  }, [api, wantsTeams]);

  const value = useMemo<SessionContextValue>(() => {
    const byId = new Map(teams.map((team) => [team.id, team.name]));
    return {
      api,
      session,
      loading,
      error,
      role,
      can: (capability: Capability) => can(role, capability),
      canPlayAudio: canPlayAudio(role, session?.policy.allow_agent_audio_playback ?? false),
      teams,
      teamsResolved,
      teamName: (id) => (id ? (byId.get(id) ?? id) : '—'),
    };
  }, [api, session, loading, error, role, teams, teamsResolved]);

  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

export function useSession(): SessionContextValue {
  const value = useContext(SessionContext);
  if (!value) throw new Error('useSession used outside SessionProvider');
  return value;
}

/**
 * Why `POST /v1/sessions` failed, in words that point at the right person.
 *
 * `POST /v1/sessions` is the portal's first authenticated request, so it is where
 * every deployment mistake between the browser and the gateway shows up first: a
 * wrong `VITE_API_BASE_URL`, a gateway that is not running, TLS, and — the one that
 * costs the most time — an origin missing from the gateway's `SENTINEL_ALLOWED_ORIGINS`.
 * The browser reports that last case as an indistinguishable network failure by
 * design, so this names both possibilities and the origin involved instead of
 * guessing between them. The alternative, which this replaces, was one sentence
 * telling the user to sign in again for a problem no amount of signing in fixes.
 *
 * Exported so it can be asserted directly, without a DOM.
 */
export function sessionErrorMessage(cause: unknown, baseUrl: string): string {
  if (!(cause instanceof ApiError)) return 'Could not start a session.';
  if (cause.isTransport) {
    return (
      `Could not reach the Sentinel gateway at ${baseUrl}. It may be unreachable, or it may not ` +
      'be configured to accept requests from this address. Check VITE_API_BASE_URL and the ' +
      'gateway’s allowed origins.'
    );
  }
  if (cause.isForbidden) {
    // The token verified but the gateway will not open a session: a suspended user
    // row, or a role the portal does not serve. Signing in again changes nothing.
    return 'Your account is not permitted to use this portal. Contact your administrator.';
  }
  if (cause.isAuthFailure) return 'Your session has expired. Sign in again.';
  return cause.requestId
    ? `Could not start a session (ref ${cause.requestId}).`
    : 'Could not start a session.';
}
