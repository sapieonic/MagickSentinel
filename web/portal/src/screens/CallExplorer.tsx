/**
 * Screen 1 — Call explorer (spec 13.3). The workhorse: QA lives in it.
 *
 * Scope note. The contract exposes exactly two call listings: `/v1/me/calls` and
 * `/v1/teams/{id}/calls`. The role matrix grants qa/compliance/admin "all tenant
 * calls", but no endpoint serves that, so this screen lists the caller's team and
 * says so plainly rather than pretending to be tenant-wide. Same for detail: only
 * `/v1/me/calls/{id}` returns a CallDetail, so a reviewer opening another agent's
 * call gets an honest notice instead of a fabricated request.
 */
import { useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import {
  CaptureTierBadge,
  DISPOSITIONS,
  DISPOSITION_LABEL,
  DispositionChip,
  PtpChip,
  SentimentDeltaChip,
  SeverityBadge,
  formatDateTime,
  formatDuration,
} from '@sentinel/shared';
import type { CallSummary, Disposition } from '@sentinel/shared';
import { EmptyState, ErrorState, LoadState, Panel } from '../components/Async.js';
import { CallDetailView } from '../components/CallDetailView.js';
import { useAsync, useDebounced } from '../components/useAsync.js';
import { useSession } from '../session.js';

export function CallExplorer() {
  const { api, session, can, role } = useSession();
  const teamId = session?.user.team_id ?? null;

  const [from, setFrom] = useState('');
  const [to, setTo] = useState('');
  const [disposition, setDisposition] = useState<Disposition | ''>('');
  const [query, setQuery] = useState('');
  const debouncedQuery = useDebounced(query);

  const scope: 'team' | 'own' = can('team_calls') && teamId ? 'team' : 'own';

  const calls = useAsync<CallSummary[]>(
    async (signal) => {
      const params = {
        from: from ? new Date(from).toISOString() : undefined,
        to: to ? new Date(to).toISOString() : undefined,
        limit: 100,
      };
      if (scope === 'team' && teamId) {
        const page = await api.listTeamCalls(teamId, params, signal);
        return page.items;
      }
      // `q` and `disposition` are only offered by /v1/me/calls; on the team listing
      // they are applied client-side below rather than silently ignored.
      const page = await api.listMyCalls(
        { ...params, disposition: disposition || undefined, q: debouncedQuery || undefined },
        signal,
      );
      return page.items;
    },
    [api, scope, teamId, from, to, scope === 'own' ? disposition : '', scope === 'own' ? debouncedQuery : ''],
  );

  const items = (calls.data ?? []).filter((call) => {
    if (scope === 'own') return true;
    if (disposition && call.disposition !== disposition) return false;
    if (debouncedQuery) {
      const haystack = `${call.account_ref ?? ''} ${call.summary ?? ''}`.toLowerCase();
      if (!haystack.includes(debouncedQuery.toLowerCase())) return false;
    }
    return true;
  });

  return (
    <div className="pt-explorer">
      <Panel
        title="Call explorer"
        actions={
          <span className="sx-muted">
            {scope === 'team' ? 'Your team' : 'Your calls'} · {items.length} shown
          </span>
        }
      >
        {scope === 'team' && can('all_tenant_calls') ? (
          <p className="pt-notice">
            Showing your team only. The contract has no tenant-wide call listing yet, so tenant scope is not
            available in this build.
          </p>
        ) : null}

        <div className="pt-filters">
          <label>
            <span>From</span>
            <input type="date" value={from} onChange={(event) => setFrom(event.target.value)} />
          </label>
          <label>
            <span>To</span>
            <input type="date" value={to} onChange={(event) => setTo(event.target.value)} />
          </label>
          <label>
            <span>Disposition</span>
            <select value={disposition} onChange={(event) => setDisposition(event.target.value as Disposition | '')}>
              <option value="">Any</option>
              {DISPOSITIONS.map((value) => (
                <option key={value} value={value}>
                  {DISPOSITION_LABEL[value]}
                </option>
              ))}
            </select>
          </label>
          <label className="pt-filters__grow">
            <span>Search</span>
            <input
              value={query}
              placeholder={scope === 'own' ? 'Full-text over your transcripts' : 'Account ref or summary'}
              onChange={(event) => setQuery(event.target.value)}
            />
          </label>
        </div>

        {calls.loading ? (
          <LoadState />
        ) : calls.error ? (
          <ErrorState error={calls.error} onRetry={calls.reload} />
        ) : items.length === 0 ? (
          <EmptyState label="No calls match these filters." />
        ) : (
          <table className="pt-table">
            <thead>
              <tr>
                <th>Started</th>
                <th>Account</th>
                <th>Duration</th>
                <th>Tier</th>
                <th>Disposition</th>
                <th>PTP</th>
                <th>Sentiment</th>
                <th>Flags</th>
              </tr>
            </thead>
            <tbody>
              {items.map((call) => (
                <tr key={call.id}>
                  <td>
                    <Link to={`/calls/${encodeURIComponent(call.id)}`}>{formatDateTime(call.started_at)}</Link>
                  </td>
                  <td>{call.account_ref ?? '—'}</td>
                  <td className="sx-nums">{formatDuration(call.duration_ms)}</td>
                  <td>
                    <CaptureTierBadge tier={call.capture_tier} />
                  </td>
                  <td>
                    <DispositionChip disposition={call.disposition} />
                  </td>
                  <td>
                    <PtpChip ptp={call.ptp} />
                  </td>
                  <td>
                    <SentimentDeltaChip delta={call.sentiment_delta} />
                  </td>
                  <td>
                    {call.flag_count ? <SeverityBadge severity={call.max_severity} count={call.flag_count} /> : '—'}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      {role === 'client' ? (
        <p className="pt-notice">Bank client accounts see flagged calls only; this screen is not available.</p>
      ) : null}
    </div>
  );
}

export function CallDetailScreen() {
  const { callId = '' } = useParams();
  const { api, session, canPlayAudio } = useSession();

  const detail = useAsync(async (signal) => api.getMyCall(callId, signal), [api, callId]);

  return (
    <Panel title="Call" actions={<Link to="/calls">Back to explorer</Link>}>
      {detail.loading ? (
        <LoadState />
      ) : detail.error ? (
        <>
          <ErrorState error={detail.error} onRetry={detail.reload} />
          {session && session.user.role !== 'agent' ? (
            <p className="pt-notice">
              Only <code>/v1/me/calls/&#123;id&#125;</code> returns call detail. Reviewing another agent&apos;s call
              needs a team- or tenant-scoped detail endpoint, which the contract does not define yet.
            </p>
          ) : null}
        </>
      ) : detail.data ? (
        <CallDetailView call={detail.data} audioAllowed={canPlayAudio} />
      ) : null}
    </Panel>
  );
}
