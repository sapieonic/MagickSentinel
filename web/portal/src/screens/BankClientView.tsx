/**
 * Screen 7 — Bank client view (spec 13.3): read-only compliance posture for the
 * lender whose book the agency is collecting, with a drill-down into the calls that
 * were flagged.
 *
 * Everything on this screen comes from the shared call listing at the client role's
 * scope, which row-level security restricts to flagged calls. There is no separate
 * "client" endpoint and no client-side filtering standing in for one: if a call
 * reaches this screen, the database decided it may.
 *
 * The role cannot resolve, assign or annotate anything, so nothing on this screen
 * offers to. Read-only is the product, not a limitation to work around.
 */
import { useState } from 'react';
import {
  CaptureTierBadge,
  SEVERITIES,
  SeverityBadge,
  formatDateTime,
  formatDuration,
} from '@sentinel/shared';
import type { CallSummary } from '@sentinel/shared';
import { EmptyState, ErrorState, LoadState, Panel } from '../components/Async.js';
import { CallDetailView } from '../components/CallDetailView.js';
import { useAsync } from '../components/useAsync.js';
import { EMPTY_FILTERS, buildCallQuery, isoDateDaysAgo } from '../calls/query.js';
import { summarisePosture } from '../calls/posture.js';
import { useSession } from '../session.js';

/** The contract's ceiling. Asking for more silently gets 200 rows anyway. */
const PAGE_LIMIT = 200;

export function BankClientView() {
  const { api, role, canPlayAudio } = useSession();
  const [from, setFrom] = useState(() => isoDateDaysAgo(30));
  const [to, setTo] = useState(() => isoDateDaysAgo(0));
  const [openId, setOpenId] = useState<string | null>(null);

  const page = useAsync(
    async (signal) =>
      api.listCalls(buildCallQuery(role, { ...EMPTY_FILTERS, from, to, flags: 'flagged' }, PAGE_LIMIT), signal),
    [api, role, from, to],
  );

  const detail = useAsync(async (signal) => (openId ? api.getCall(openId, signal) : null), [api, openId]);

  const calls: readonly CallSummary[] = page.data?.items ?? [];
  const posture = summarisePosture(calls, page.data?.next_cursor != null);

  return (
    <div className="pt-stack">
      <Panel
        title="Compliance posture"
        actions={
          <div className="pt-filters pt-filters--inline">
            <label>
              <span>From</span>
              <input type="date" value={from} onChange={(event) => setFrom(event.target.value)} />
            </label>
            <label>
              <span>To</span>
              <input type="date" value={to} onChange={(event) => setTo(event.target.value)} />
            </label>
          </div>
        }
      >
        {page.loading ? (
          <LoadState />
        ) : page.error ? (
          <ErrorState error={page.error} onRetry={page.reload} />
        ) : (
          <>
            <div className="pt-stats">
              <Stat label="Flagged calls" value={String(posture.flaggedCalls)} />
              <Stat label="Breaches raised" value={String(posture.totalFlags)} />
              <Stat
                label="High or critical"
                value={String(posture.seriousCalls)}
                warn={posture.seriousCalls > 0}
              />
              <Stat label="Worst severity" value={posture.worst ?? 'none'} warn={posture.worst === 'critical'} />
            </div>

            <table className="pt-table pt-table--compare">
              <thead>
                <tr>
                  <th>Severity</th>
                  <th>Flagged calls</th>
                </tr>
              </thead>
              <tbody>
                {SEVERITIES.map((severity) => (
                  <tr key={severity}>
                    <td>
                      <SeverityBadge severity={severity} />
                    </td>
                    <td className="sx-nums">{posture.bySeverity[severity]}</td>
                  </tr>
                ))}
              </tbody>
            </table>

            <p className="sx-muted">
              {posture.earliest && posture.latest
                ? `Covering calls from ${formatDateTime(posture.earliest)} to ${formatDateTime(posture.latest)}.`
                : 'No flagged calls in this window.'}
            </p>
            {posture.partial ? (
              <p className="pt-notice">
                More flagged calls match this window than are counted here: these are the {PAGE_LIMIT} most recent.
                Narrow the dates for a complete count.
              </p>
            ) : null}
          </>
        )}
      </Panel>

      <Panel title="Flagged calls">
        {page.loading ? (
          <LoadState />
        ) : page.error ? (
          <ErrorState error={page.error} onRetry={page.reload} />
        ) : calls.length === 0 ? (
          <EmptyState label="No flagged calls in this window." />
        ) : (
          <table className="pt-table">
            <thead>
              <tr>
                <th>Started</th>
                <th>Account</th>
                <th>Duration</th>
                <th>Tier</th>
                <th>Breaches</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {calls.map((call) => (
                <tr key={call.id}>
                  <td>{formatDateTime(call.started_at)}</td>
                  <td>{call.account_ref ?? '—'}</td>
                  <td className="sx-nums">{formatDuration(call.duration_ms)}</td>
                  <td>
                    <CaptureTierBadge tier={call.capture_tier} />
                  </td>
                  <td>
                    <SeverityBadge severity={call.max_severity} count={call.flag_count} />
                  </td>
                  <td>
                    <button onClick={() => setOpenId(openId === call.id ? null : call.id)}>
                      {openId === call.id ? 'Close' : 'Open'}
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      {openId ? (
        <Panel title="Call evidence">
          {detail.loading ? (
            <LoadState />
          ) : detail.error ? (
            <ErrorState error={detail.error} onRetry={detail.reload} />
          ) : detail.data ? (
            <CallDetailView call={detail.data} audioAllowed={canPlayAudio} />
          ) : null}
        </Panel>
      ) : null}
    </div>
  );
}

function Stat({ label, value, warn }: { label: string; value: string; warn?: boolean }) {
  return (
    <div className={warn ? 'pt-stat pt-stat--warn' : 'pt-stat'}>
      <span className="pt-stat__value">{value}</span>
      <span className="pt-stat__label sx-muted">{label}</span>
    </div>
  );
}
