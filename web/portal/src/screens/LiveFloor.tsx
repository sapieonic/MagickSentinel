/**
 * Screen 6 — Live floor (spec 13.3): the calls in flight right now.
 *
 * `state` is rendered verbatim rather than mapped through the capture-state labels:
 * the gateway currently puts the call's pipeline status in that field, and a lookup
 * table would turn an unexpected value into a blank cell instead of showing it.
 *
 * The stream carries no live sentiment yet — the gateway sends nulls until the
 * streaming ASR path lands — so those columns say "—" rather than 0. A zero on a
 * sentiment scale reads as "neutral", which is a measurement, and this screen must
 * not invent one.
 */
import { useEffect, useState } from 'react';
import { formatDuration } from '@sentinel/shared';
import { EmptyState, LoadState, Panel } from '../components/Async.js';
import { useLiveFloor } from '../live/useLiveFloor.js';
import type { LiveRow, LiveState } from '../live/connection.js';
import { useSession } from '../session.js';

export function LiveFloor() {
  const { api, session, teams, teamsResolved, can } = useSession();
  const claimTeam = session?.user.team_id ?? null;
  // Roles that see the whole tenant pick a team; a supervisor's floor is their own.
  const choosable = can('all_tenant_calls');
  const [chosen, setChosen] = useState<string>('');
  const teamId = choosable ? chosen || teams[0]?.id || null : claimTeam;

  const live = useLiveFloor(api, teamId);
  const nowMs = useSecondTick();

  if (!teamId) {
    return (
      <Panel title="Live floor">
        {choosable && !teamsResolved ? (
          <LoadState label="Finding teams…" />
        ) : (
          <p className="pt-notice">
            {choosable
              ? 'No teams are visible on this tenant, so there is no floor to watch.'
              : 'You are not assigned to a team, so there is no floor to watch.'}
          </p>
        )}
      </Panel>
    );
  }

  return (
    <Panel
      title="Live floor"
      actions={
        <div className="pt-actions">
          {choosable ? (
            <select value={teamId} onChange={(event) => setChosen(event.target.value)} aria-label="Team">
              {teams.map((team) => (
                <option key={team.id} value={team.id}>
                  {team.name}
                </option>
              ))}
            </select>
          ) : null}
          <ConnectionBadge state={live} />
        </div>
      }
    >
      {live.status === 'refused' ? (
        <p className="pt-notice sx-error">{live.refusal}</p>
      ) : live.status === 'reconnecting' ? (
        <p className="pt-notice">
          Lost the stream and is reconnecting (attempt {live.attempt}). Nothing on this screen is being updated until
          it comes back.
        </p>
      ) : null}

      {live.calls.length === 0 ? (
        // "Nothing is happening" and "we are not being told what is happening" look
        // identical on an empty table, and only one of them is good news.
        <EmptyState label={emptyLabel(live)} />
      ) : (
        <table className="pt-table">
          <thead>
            <tr>
              <th>Agent</th>
              <th>State</th>
              <th>Elapsed</th>
              <th>Borrower sentiment</th>
              <th>Agent sentiment</th>
              <th>Alert</th>
            </tr>
          </thead>
          <tbody>
            {live.calls.map((call) => (
              <FloorRow key={call.call_id} call={call} nowMs={nowMs} />
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}

function FloorRow({ call, nowMs }: { call: LiveRow; nowMs: number }) {
  const started = Date.parse(call.started_at);
  const elapsedMs = Number.isNaN(started) ? 0 : Math.max(0, nowMs - started);
  return (
    <tr className={call.alert ? 'pt-cell--warn' : undefined}>
      <td>{call.display_name ?? call.user_uid}</td>
      <td>{call.state}</td>
      <td className="sx-nums">{formatDuration(elapsedMs)}</td>
      <td className="sx-nums">{formatSentiment(call.sentiment_far)}</td>
      <td className="sx-nums">{formatSentiment(call.sentiment_near)}</td>
      <td>{call.alert ?? '—'}</td>
    </tr>
  );
}

function ConnectionBadge({ state }: { state: LiveState }) {
  const label =
    state.status === 'live'
      ? 'Live'
      : state.status === 'connecting'
        ? 'Connecting…'
        : state.status === 'reconnecting'
          ? 'Reconnecting…'
          : state.status === 'refused'
            ? 'Not connected'
            : 'Stopped';
  return <span className={`sx-chip${state.status === 'live' ? ' sx-chip--live' : ' sx-chip--muted'}`}>{label}</span>;
}

function emptyLabel(live: LiveState): string {
  if (live.status === 'reconnecting' || live.status === 'refused') return 'Not receiving updates.';
  return live.lastEventAt === null ? 'Waiting for the first update…' : 'No calls in progress on this team.';
}

function formatSentiment(value: number | null | undefined): string {
  return typeof value === 'number' ? value.toFixed(2) : '—';
}

/**
 * One clock for the whole table. Elapsed time is counted from `started_at` rather
 * than the snapshot's `elapsed_ms`, because a snapshot arrives every few seconds and
 * a duration that only moves when it does looks frozen.
 */
function useSecondTick(): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);
  return now;
}
