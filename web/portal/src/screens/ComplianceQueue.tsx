/**
 * Screen 2 — Compliance queue (spec 13.3): flags by severity, assign / resolve /
 * annotate, reviewer audit trail, evidence pack export.
 */
import { useState } from 'react';
import { Link } from 'react-router-dom';
import { SEVERITIES, SEVERITY_RANK, SeverityBadge, formatDuration } from '@sentinel/shared';
import type { Flag, FlagStatus, Severity } from '@sentinel/shared';
import { ApiError } from '@sentinel/shared';
import { EmptyState, ErrorState, LoadState, Panel } from '../components/Async.js';
import { useAsync } from '../components/useAsync.js';
import { useSession } from '../session.js';

const STATUSES: readonly FlagStatus[] = ['open', 'assigned', 'upheld', 'dismissed'];

export function ComplianceQueue() {
  const { api, session, can } = useSession();
  const [severity, setSeverity] = useState<Severity | ''>('');
  const [status, setStatus] = useState<FlagStatus | ''>('open');
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [note, setNote] = useState('');
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  const flags = useAsync(
    async (signal) =>
      api.listFlags({ severity: severity || undefined, status: status || undefined, limit: 100 }, signal),
    [api, severity, status],
  );

  const readOnly = !can('resolve_flags');

  // Critical first: a queue sorted by arrival buries the flag that matters under
  // fifty low-severity ones.
  const items = [...(flags.data?.items ?? [])].sort((a, b) => SEVERITY_RANK[b.severity] - SEVERITY_RANK[a.severity]);

  const act = async (flag: Flag, patch: { status?: FlagStatus; assignToMe?: boolean }) => {
    setBusy(true);
    setMessage(null);
    try {
      await api.updateFlag(flag.id, {
        ...(patch.status ? { status: patch.status } : {}),
        ...(patch.assignToMe && session ? { reviewer_uid: session.user.firebase_uid } : {}),
        ...(note.trim() ? { note: note.trim() } : {}),
      });
      setNote('');
      flags.reload();
    } catch (error) {
      setMessage(error instanceof ApiError ? `Could not update the flag (${error.code}).` : 'Could not update the flag.');
    } finally {
      setBusy(false);
    }
  };

  const exportEvidence = async (includeAudio: boolean) => {
    if (selected.size === 0) return;
    setBusy(true);
    setMessage(null);
    try {
      const job = await api.createEvidenceExport({ flag_ids: [...selected], include_audio: includeAudio });
      // The job is asynchronous; the contract returns 202 with a status, so there is
      // nothing to download yet and promising one would be a lie.
      setMessage(`Evidence pack queued (${job.status}). It will appear in exports when ready.`);
      setSelected(new Set());
    } catch (error) {
      setMessage(error instanceof ApiError ? `Export failed (${error.code}).` : 'Export failed.');
    } finally {
      setBusy(false);
    }
  };

  const toggle = (id: string) =>
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  return (
    <Panel
      title="Compliance queue"
      actions={
        <div className="pt-actions">
          <button disabled={busy || selected.size === 0} onClick={() => void exportEvidence(false)}>
            Export evidence ({selected.size})
          </button>
          <button disabled={busy || selected.size === 0} onClick={() => void exportEvidence(true)}>
            Export with audio
          </button>
        </div>
      }
    >
      <div className="pt-filters">
        <label>
          <span>Severity</span>
          <select value={severity} onChange={(event) => setSeverity(event.target.value as Severity | '')}>
            <option value="">Any</option>
            {SEVERITIES.map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </select>
        </label>
        <label>
          <span>Status</span>
          <select value={status} onChange={(event) => setStatus(event.target.value as FlagStatus | '')}>
            <option value="">Any</option>
            {STATUSES.map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </select>
        </label>
        <label className="pt-filters__grow">
          <span>Note (applied with the next action)</span>
          <input value={note} onChange={(event) => setNote(event.target.value)} maxLength={2000} disabled={readOnly} />
        </label>
      </div>

      {readOnly ? <p className="pt-notice">Your role can read the queue but not resolve flags.</p> : null}
      {message ? <p className="pt-notice">{message}</p> : null}

      {flags.loading ? (
        <LoadState />
      ) : flags.error ? (
        <ErrorState error={flags.error} onRetry={flags.reload} />
      ) : items.length === 0 ? (
        <EmptyState label="Nothing in the queue for these filters." />
      ) : (
        <table className="pt-table">
          <thead>
            <tr>
              <th />
              <th>Severity</th>
              <th>Rule</th>
              <th>Source</th>
              <th>Span</th>
              <th>Evidence</th>
              <th>Status</th>
              <th>Reviewer</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody>
            {items.map((flag) => (
              <tr key={flag.id}>
                <td>
                  <input
                    type="checkbox"
                    aria-label={`Select flag ${flag.rule_id}`}
                    checked={selected.has(flag.id)}
                    onChange={() => toggle(flag.id)}
                  />
                </td>
                <td>
                  <SeverityBadge severity={flag.severity} />
                </td>
                <td className="sx-mono">{flag.rule_id}</td>
                <td>{flag.tier === 1 ? 'rule' : 'judge'}</td>
                <td className="sx-nums">
                  {flag.span_start_ms !== null && flag.span_start_ms !== undefined
                    ? formatDuration(flag.span_start_ms)
                    : '—'}
                </td>
                <td className="pt-evidence">
                  {flag.evidence_text ?? flag.judge_rationale ?? '—'}
                  <Link className="pt-evidence__link" to={`/calls/${encodeURIComponent(flag.call_id)}`}>
                    open call
                  </Link>
                </td>
                <td>{flag.status}</td>
                <td className="sx-mono">{flag.reviewer_uid ?? '—'}</td>
                <td>
                  <div className="pt-actions">
                    <button disabled={readOnly || busy} onClick={() => void act(flag, { status: 'assigned', assignToMe: true })}>
                      Assign me
                    </button>
                    <button disabled={readOnly || busy} onClick={() => void act(flag, { status: 'upheld' })}>
                      Uphold
                    </button>
                    <button disabled={readOnly || busy} onClick={() => void act(flag, { status: 'dismissed' })}>
                      Dismiss
                    </button>
                  </div>
                  {flag.agent_response ? (
                    <p className="pt-agentresp sx-muted">Agent: {flag.agent_response}</p>
                  ) : null}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}
