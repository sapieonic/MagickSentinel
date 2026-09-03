import { NavLink, Navigate, Route, Routes } from 'react-router-dom';
import type { ReactElement } from 'react';
import type { Capability } from '@sentinel/shared';
import { CallDetailScreen, CallExplorer } from './screens/CallExplorer.js';
import { ComplianceQueue } from './screens/ComplianceQueue.js';
import { AgentSelfView } from './screens/AgentSelfView.js';
import { FleetAdmin } from './screens/FleetAdmin.js';
import { BankClientView } from './screens/BankClientView.js';
import { LiveFloor } from './screens/LiveFloor.js';
import { RuleEditor, TeamScorecards } from './screens/Stubs.js';
import { defaultRoute, navFor } from './navigation.js';
import { usePortalAuth } from './auth/AuthProvider.js';
import { useSession } from './session.js';

export function App() {
  const { session, loading, error, role } = useSession();
  const { signOut } = usePortalAuth();

  // Reached only in the signed-in auth phase, so this is the gateway session
  // handshake rather than sign-in: the credential is already in hand and
  // POST /v1/sessions is in flight.
  if (loading) return <p className="pt-boot sx-muted">Opening your session…</p>;
  if (error || !session) {
    return (
      <div className="pt-boot">
        <p className="sx-error">{error ?? 'Not signed in.'}</p>
        {/* The only way out of a session the gateway will not open. Without it the
            operator is stuck on this screen with a credential they cannot use and no
            way to try another account. */}
        <button onClick={() => void signOut()}>Sign out</button>
      </div>
    );
  }


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
        <button className="pt-signout" onClick={() => void signOut()}>
          Sign out
        </button>
      </header>

      <main className="pt-main">
        <Routes>
          <Route path="/" element={<Navigate to={defaultRoute(role)} replace />} />
          <Route path="/me" element={<Guard capability="own_calls">{<AgentSelfView />}</Guard>} />
          <Route path="/calls" element={<Guard capability="team_calls">{<CallExplorer />}</Guard>} />
          {/* Detail is reachable by anyone who can see a call at all. Which calls
              those are is the server's decision, and a call outside the caller's
              scope answers 404 — so there is nothing for a stricter gate here to
              protect. */}
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
 * Client-side capability gate.
 *
 * **This is not authorisation.** It is a usability guard, and the only reason it is
 * safe to rely on is that it is not relied upon: the gateway re-checks every request
 * against the role in the verified ID token (`auth.Require`, and row-level security
 * underneath it), so a user who edits their way past this component gains nothing but
 * a screen full of 403s and 404s. Anyone tempted to move a security decision into this
 * file should move it into `server/gateway/internal/auth` instead.
 *
 * The capability comes from `CAPABILITIES`/`can` in `@sentinel/shared`, which mirrors
 * the Go matrix. There is deliberately no second authorisation model in the portal:
 * one mirror can drift and be found by a test, two mirrors drift against each other.
 */
function Guard({ capability, children }: { capability: Capability; children: ReactElement }) {
  const { can } = useSession();
  if (!can(capability)) return <p className="pt-boot sx-error">You do not have access to this screen.</p>;
  return children;
}
