/**
 * Deliberate stubs.
 *
 * Both screens here are blocked on a product decision, not on plumbing: each names
 * what would settle it and the operation it would be built on. A half-built
 * scorecard is worse than an empty screen that says what is missing, because a
 * reviewer cannot tell "not built" from "no data".
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
