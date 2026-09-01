/**
 * Deliberate stubs.
 *
 * Spec 13.3 numbers its screens in build order and marks 5–7 as later phases; the
 * milestones in section 16 put the live floor view in phase 5. A half-built
 * scorecard or a floor view that silently shows nothing is worse than an empty
 * screen that says what is missing, because QA cannot tell the difference between
 * "not built" and "no data" — so these say so, and each names the contract
 * operation it will be built on.
 */
import { Panel } from '../components/Async.js';

function Stub({ title, phase, endpoint, why }: { title: string; phase: string; endpoint: string; why: string }) {
  return (
    <Panel title={title}>
      <p className="pt-notice">
        <strong>Not built yet — {phase}.</strong> {why}
      </p>
      <p className="sx-muted">
        Will be built on <code className="sx-mono">{endpoint}</code>.
      </p>
    </Panel>
  );
}

export function TeamScorecards() {
  return (
    <Stub
      title="Team scorecards"
      phase="spec 13.3 screen 5"
      endpoint="GET /v1/teams/{id}/scorecards"
      why="Supervisor view with trend over time. The endpoint returns a point-in-time median plus per-agent rows; trend needs either repeated windowed calls or a contract change, which is a decision this build does not make on its own."
    />
  );
}

export function LiveFloor() {
  return (
    <Stub
      title="Live floor view"
      phase="spec 13.3 screen 6, milestone phase 5"
      endpoint="GET /v1/teams/{id}/live (SSE)"
      why="Active calls with live sentiment and escalation alerts. Blocked on how the stream is authenticated: EventSource cannot send an Authorization header and the contract does not define an alternative, so subscribing here would mean inventing one."
    />
  );
}

export function BankClientView() {
  return (
    <Stub
      title="Bank client view"
      phase="spec 13.3 screen 7"
      endpoint="GET /v1/compliance/flags"
      why="Read-only aggregate compliance posture, drilling into flagged calls only. The flag queue endpoint exists, but the role matrix restricts this role to flagged calls and there is no flag-scoped call detail endpoint to drill into."
    />
  );
}

export function RuleEditor() {
  return (
    <Stub
      title="Rule editor"
      phase="spec 13.3 screen 4, deferred"
      endpoint="GET/PUT /v1/admin/rules"
      why="Publishing a rule set is versioned and immutable, and a mis-published version changes what the whole floor is flagged on. An editor worth shipping needs a diff against the active version and a preview of what would newly flag; neither is buildable without a dry-run operation the contract does not define."
    />
  );
}
