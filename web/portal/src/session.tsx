/**
 * Portal session: who is signed in, what the tenant policy allows, the teams the
 * caller can name, and the one ApiClient every screen uses.
 *
 * The portal has its own Identity Platform session (the widget's token comes from
 * the native layer instead), so the token provider here reads from whatever the
 * auth SDK last handed us. That integration is not part of this workspace; the
 * provider is a single seam so wiring it is a one-file change.
 */
import { createContext, useContext, useEffect, useMemo, useState } from 'react';
import type { ReactNode } from 'react';
import { ApiClient, canPlayAudio } from '@sentinel/shared';
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
 * Replace with the auth SDK's accessor. Kept synchronous-looking and null-safe so a
 * signed-out portal produces a clean `no_credentials` error rather than a 401 loop.
 */
export type PortalTokenProvider = () => string | null | Promise<string | null>;

export function SessionProvider({
  children,
  baseUrl,
  getToken,
}: {
  children: ReactNode;
  baseUrl: string;
  getToken: PortalTokenProvider;
}) {
  const api = useMemo(() => new ApiClient({ baseUrl, getToken }), [baseUrl, getToken]);
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
      .catch(() => {
        if (!controller.signal.aborted) setError('Could not start a session. Sign in again.');
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, [api]);

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
