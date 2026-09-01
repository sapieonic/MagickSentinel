import { NavLink, Navigate, Route, Routes } from 'react-router-dom';
import type { ReactElement } from 'react';
import type { Capability } from '@sentinel/shared';
import { CallDetailScreen, CallExplorer } from './screens/CallExplorer.js';
import { ComplianceQueue } from './screens/ComplianceQueue.js';
import { AgentSelfView } from './screens/AgentSelfView.js';
import { FleetAdmin } from './screens/FleetAdmin.js';
import { BankClientView, LiveFloor, RuleEditor, TeamScorecards } from './screens/Stubs.js';
import { defaultRoute, navFor } from './navigation.js';
import { useSession } from './session.js';

export function App() {
  const { session, loading, error, role } = useSession();

  if (loading) return <p className="pt-boot sx-muted">Signing in…</p>;
  if (error || !session) return <p className="pt-boot sx-error">{error ?? 'Not signed in.'}</p>;


  return (
    <div className="pt-app">
      <header className="pt-header">
        <span className="pt-brand">Sentinel</span>
        {/* Navigation is filtered, not merely guarded. An agent who can see a
            "Compliance" link and gets a 403 has learned that other people's flags
            exist and that the portal will refuse them; spec 13.4 asks for the link
            not to be there at all. */}
        <nav className="pt-nav">
          {navFor(role).map((entry) => (
            <NavLink key={entry.to} to={entry.to} className={({ isActive }) => (isActive ? 'pt-nav__on' : undefined)}>
              {entry.label}
            </NavLink>
          ))}
        </nav>
        <span className="pt-who sx-muted">
          {session.user.display_name} · {role}
        </span>
      </header>

      <main className="pt-main">
        <Routes>
          <Route path="/" element={<Navigate to={defaultRoute(role)} replace />} />
          <Route path="/me" element={<Guard capability="own_calls">{<AgentSelfView />}</Guard>} />
          <Route path="/calls" element={<Guard capability="team_calls">{<CallExplorer />}</Guard>} />
          {/* Detail is reachable by anyone who can see their own calls: the endpoint
              behind it is /v1/me/calls/{id}, which is self-scoped server-side. */}
          <Route path="/calls/:callId" element={<Guard capability="own_calls">{<CallDetailScreen />}</Guard>} />
          <Route path="/compliance" element={<Guard capability="resolve_flags">{<ComplianceQueue />}</Guard>} />
          <Route path="/fleet" element={<Guard capability="manage_devices_users">{<FleetAdmin />}</Guard>} />
          <Route path="/rules" element={<Guard capability="edit_rules">{<RuleEditor />}</Guard>} />
          <Route path="/scorecards" element={<Guard capability="team_calls">{<TeamScorecards />}</Guard>} />
          <Route path="/live" element={<Guard capability="team_calls">{<LiveFloor />}</Guard>} />
          <Route path="/client" element={<Guard capability="flagged_calls_only">{<BankClientView />}</Guard>} />
          <Route path="*" element={<p className="pt-boot sx-muted">No such screen.</p>} />
        </Routes>
      </main>
    </div>
  );
}

/**
 * Client-side capability gate. This is a usability guard only — the gateway is the
 * security boundary and re-checks every request against the verified token.
 */
function Guard({ capability, children }: { capability: Capability; children: ReactElement }) {
  const { can } = useSession();
  if (!can(capability)) return <p className="pt-boot sx-error">You do not have access to this screen.</p>;
  return children;
}
