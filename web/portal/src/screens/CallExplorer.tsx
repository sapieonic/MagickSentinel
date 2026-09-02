/**
 * Screen 1 — Call explorer (spec 13.3). The workhorse: QA lives in it.
 *
 * One listing serves every role. What comes back is decided by row-level security
 * from the verified token, so this screen does not branch on role to choose an
 * endpoint and does not filter results after the fact — an agent sees their own
 * calls, a supervisor their team's, qa/compliance/admin the tenant. The filters
 * below narrow that set and cannot widen it, so a filter naming something out of
 * scope returns nothing rather than someone else's calls.
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
import type { Disposition } from '@sentinel/shared';
import { EmptyState, ErrorState, LoadState, Panel } from '../components/Async.js';
import { CallDetailView } from '../components/CallDetailView.js';
import { useAsync, useDebounced } from '../components/useAsync.js';
import { EMPTY_FILTERS, buildCallQuery } from '../calls/query.js';
import type { FlagFilter } from '../calls/query.js';
import { useSession } from '../session.js';

/** The contract's ceiling; beyond it the caller is told to narrow the window. */
const PAGE_LIMIT = 200;

export function CallExplorer() {
  const { api, role, teams, can, teamName, session } = useSession();

  const [from, setFrom] = useState('');
  const [to, setTo] = useState('');
  const [disposition, setDisposition] = useState<Disposition | ''>('');
  const [flags, setFlags] = useState<FlagFilter>('any');
  const [teamId, setTeamId] = useState('');
  const [query, setQuery] = useState('');
  const debouncedQuery = useDebounced(query);

  // Only a role that can see across teams gains anything from a team filter. A
  // supervisor's scope is one team already, so the control would offer choices that
  // could only ever return an empty page.
  const canPickTeam = can('all_tenant_calls') && teams.length > 1;

  const page = useAsync(
    async (signal) =>
      api.listCalls(
        buildCallQuery(
          role,
          { ...EMPTY_FILTERS, from, to, disposition, flags, q: debouncedQuery, teamId: canPickTeam ? teamId : '' },
          PAGE_LIMIT,
        ),
        signal,
      ),
    [api, role, from, to, disposition, flags, debouncedQuery, canPickTeam ? teamId : ''],
  );

  const items = page.data?.items ?? [];

  // Says what the caller is looking at, because "12 shown" means something very
  // different to a supervisor and to a QA reviewer.
  const scopeLabel = can('all_tenant_calls')
    ? teamId
      ? teamName(teamId)
      : 'Whole tenant'
    : can('team_calls')
      ? teamName(session?.user.team_id)
      : 'Your calls';

  return (
    <div className="pt-explorer">
      <Panel title="Call explorer" actions={<span className="sx-muted">{scopeLabel} · {items.length} shown</span>}>
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
          <label>
            <span>Compliance</span>
            <select value={flags} onChange={(event) => setFlags(event.target.value as FlagFilter)}>
              <option value="any">Any</option>
              <option value="flagged">Flagged only</option>
              <option value="unflagged">No flags</option>
            </select>
          </label>
          {canPickTeam ? (
            <label>
              <span>Team</span>
              <select value={teamId} onChange={(event) => setTeamId(event.target.value)}>
                <option value="">All teams</option>
                {teams.map((team) => (
                  <option key={team.id} value={team.id}>
                    {team.name}
                  </option>
                ))}
              </select>
            </label>
          ) : null}
          <label className="pt-filters__grow">
            <span>Search</span>
            <input
              value={query}
              placeholder="Full-text over transcripts in scope"
              onChange={(event) => setQuery(event.target.value)}
            />
          </label>
        </div>

        {page.loading ? (
          <LoadState />
        ) : page.error ? (
          <ErrorState error={page.error} onRetry={page.reload} />
        ) : items.length === 0 ? (
          <EmptyState label="No calls match these filters." />
        ) : (
          <>
            <table className="pt-table">
              <thead>
                <tr>
                  <th>Started</th>
                  <th>Agent</th>
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
                    <td className="sx-mono">{call.user_uid}</td>
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
                      {call.flag_count > 0 ? (
                        <SeverityBadge severity={call.max_severity} count={call.flag_count} />
                      ) : (
                        '—'
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {page.data?.next_cursor != null ? (
              <p className="pt-notice">
                More calls match than are listed here: these are the {PAGE_LIMIT} most recent. Narrow the dates or the
                filters to see the rest.
              </p>
            ) : null}
          </>
        )}
      </Panel>
    </div>
  );
}

export function CallDetailScreen() {
  const { callId = '' } = useParams();
  const { api, canPlayAudio } = useSession();

  const detail = useAsync(async (signal) => api.getCall(callId, signal), [api, callId]);

  return (
    <Panel title="Call" actions={<Link to="/calls">Back to explorer</Link>}>
      {detail.loading ? (
        <LoadState />
      ) : detail.error ? (
        // A call outside the caller's scope answers 404, deliberately, so the shared
        // wording is "not found, or not visible at your access level". Do not
        // sharpen it into "no such call": the reader has not been told that.
        <ErrorState error={detail.error} onRetry={detail.reload} />
      ) : detail.data ? (
        <CallDetailView call={detail.data} audioAllowed={canPlayAudio} />
      ) : null}
    </Panel>
  );
}
