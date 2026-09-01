/**
 * Screen 3 — Agent self-view (spec 13.3): three tabs, no more.
 *
 * Two hard rules from the spec drive the shape of this file:
 *
 *  - "My performance" compares against the team median, never a leaderboard. There
 *    is no ranked list here and no other agent is nameable from this screen; the
 *    only external number is `/v1/teams/{id}/scorecards`.median. Leaderboards on a
 *    collections floor produce gaming, not improvement.
 *  - An agent must not see other agents' calls, other agents' scores, or raw rule
 *    definitions. Every request on this screen is in the `me` namespace except the
 *    median, and the median is rendered as a single comparison value.
 */
import { useState } from 'react';
import {
  DispositionChip,
  PtpChip,
  SentimentDeltaChip,
  formatDateTime,
  formatDuration,
  formatPaise,
  formatPercent,
  paiseToInputValue,
  parseFailureMessage,
  parseRupeesToPaise,
  SeverityBadge,
} from '@sentinel/shared';
import type { CallSummary, Disposition, Flag } from '@sentinel/shared';
import { ApiError, DISPOSITIONS, DISPOSITION_LABEL } from '@sentinel/shared';
import { EmptyState, ErrorState, LoadState, Panel } from '../components/Async.js';
import { CallDetailView } from '../components/CallDetailView.js';
import { useAsync, useDebounced } from '../components/useAsync.js';
import { useSession } from '../session.js';

type Tab = 'calls' | 'performance' | 'flags';

export function AgentSelfView() {
  const [tab, setTab] = useState<Tab>('calls');
  return (
    <Panel
      title="My work"
      actions={
        <nav className="pt-subtabs">
          <button className={tabClass(tab === 'calls')} onClick={() => setTab('calls')}>
            My calls
          </button>
          <button className={tabClass(tab === 'performance')} onClick={() => setTab('performance')}>
            My performance
          </button>
          <button className={tabClass(tab === 'flags')} onClick={() => setTab('flags')}>
            My flags
          </button>
        </nav>
      }
    >
      {tab === 'calls' ? <MyCalls /> : tab === 'performance' ? <MyPerformance /> : <MyFlags />}
    </Panel>
  );
}

function tabClass(active: boolean): string {
  return active ? 'pt-subtab pt-subtab--on' : 'pt-subtab';
}

/* ------------------------------------------------------------- my calls */

/** Spec 13.3: PTP correction inside a 24 h window. The server enforces it (409 on
 *  `/confirm` when the window has closed); this only decides whether to offer the
 *  form, because a disabled button beats an error toast. */
const PTP_CORRECTION_WINDOW_HOURS = 24;

function withinCorrectionWindow(call: CallSummary): boolean {
  if (!call.ended_at) return false;
  const ended = new Date(call.ended_at).getTime();
  if (Number.isNaN(ended)) return false;
  return Date.now() - ended < PTP_CORRECTION_WINDOW_HOURS * 3600_000;
}

function MyCalls() {
  const { api, canPlayAudio } = useSession();
  const [query, setQuery] = useState('');
  const [disposition, setDisposition] = useState<Disposition | ''>('');
  const [openId, setOpenId] = useState<string | null>(null);
  const debouncedQuery = useDebounced(query);

  const calls = useAsync(
    async (signal) =>
      api.listMyCalls(
        { q: debouncedQuery || undefined, disposition: disposition || undefined, limit: 100 },
        signal,
      ),
    [api, debouncedQuery, disposition],
  );

  const detail = useAsync(
    async (signal) => (openId ? api.getMyCall(openId, signal) : null),
    [api, openId],
  );

  return (
    <>
      <div className="pt-filters">
        <label className="pt-filters__grow">
          <span>Search my transcripts</span>
          <input value={query} onChange={(event) => setQuery(event.target.value)} />
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
      </div>

      {calls.loading ? (
        <LoadState />
      ) : calls.error ? (
        <ErrorState error={calls.error} onRetry={calls.reload} />
      ) : (calls.data?.items.length ?? 0) === 0 ? (
        <EmptyState label="No calls found." />
      ) : (
        <table className="pt-table">
          <thead>
            <tr>
              <th>Started</th>
              <th>Account</th>
              <th>Duration</th>
              <th>Disposition</th>
              <th>PTP</th>
              <th>Sentiment</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {(calls.data?.items ?? []).map((call) => (
              <tr key={call.id}>
                <td>{formatDateTime(call.started_at)}</td>
                <td>{call.account_ref ?? '—'}</td>
                <td className="sx-nums">{formatDuration(call.duration_ms)}</td>
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
                  <button onClick={() => setOpenId(openId === call.id ? null : call.id)}>
                    {openId === call.id ? 'Close' : 'Open'}
                  </button>
                  {withinCorrectionWindow(call) ? (
                    <CorrectionForm call={call} onSaved={calls.reload} />
                  ) : (
                    <span className="sx-muted pt-window">Correction window closed</span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {openId ? (
        detail.loading ? (
          <LoadState />
        ) : detail.error ? (
          <ErrorState error={detail.error} onRetry={detail.reload} />
        ) : detail.data ? (
          <CallDetailView call={detail.data} audioAllowed={canPlayAudio} />
        ) : null
      ) : null}
    </>
  );
}

function CorrectionForm({ call, onSaved }: { call: CallSummary; onSaved: () => void }) {
  const { api } = useSession();
  const [open, setOpen] = useState(false);
  const [disposition, setDisposition] = useState<Disposition>(call.disposition ?? 'other');
  const [amount, setAmount] = useState(paiseToInputValue(call.ptp?.agent_amount_paise ?? call.ptp?.amount_paise));
  const [dueDate, setDueDate] = useState(call.ptp?.agent_due_date ?? call.ptp?.due_date ?? '');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  if (!open) {
    return (
      <button className="pt-linkish" onClick={() => setOpen(true)}>
        Correct
      </button>
    );
  }

  const save = async () => {
    let amountPaise: number | null = null;
    if (amount.trim() !== '') {
      const parsed = parseRupeesToPaise(amount);
      if (!parsed.ok) {
        setError(parseFailureMessage(parsed.reason));
        return;
      }
      amountPaise = parsed.paise;
    }
    setBusy(true);
    setError(null);
    try {
      await api.confirmMyCall(call.id, {
        disposition,
        ptp_present: amountPaise !== null,
        ptp_amount_paise: amountPaise,
        ptp_due_date: dueDate || null,
      });
      setOpen(false);
      onSaved();
    } catch (cause) {
      setError(
        cause instanceof ApiError && cause.isConflict
          ? 'The correction window closed while this form was open.'
          : 'Could not save the correction.',
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="pt-correct">
      <select value={disposition} onChange={(event) => setDisposition(event.target.value as Disposition)}>
        {DISPOSITIONS.map((value) => (
          <option key={value} value={value}>
            {DISPOSITION_LABEL[value]}
          </option>
        ))}
      </select>
      <input inputMode="decimal" placeholder="Amount ₹" value={amount} onChange={(e) => setAmount(e.target.value)} />
      <input type="date" value={dueDate} onChange={(event) => setDueDate(event.target.value)} />
      <button className="sx-btn--primary" disabled={busy} onClick={() => void save()}>
        Save
      </button>
      <button disabled={busy} onClick={() => setOpen(false)}>
        Cancel
      </button>
      {error ? <p className="sx-error">{error}</p> : null}
    </div>
  );
}

/* ------------------------------------------------------- my performance */

function MyPerformance() {
  const { api, session } = useSession();
  const teamId = session?.user.team_id ?? null;

  const mine = useAsync(async (signal) => api.getMyStats({}, signal), [api]);

  // Only the median is read from the team endpoint. The per-agent array it also
  // returns is deliberately discarded here: rendering it would be the leaderboard
  // spec 13.3 rules out.
  const median = useAsync(
    async (signal) => (teamId ? (await api.getTeamScorecards(teamId, {}, signal)).median : null),
    [api, teamId],
  );

  if (mine.loading) return <LoadState />;
  if (mine.error) return <ErrorState error={mine.error} onRetry={mine.reload} />;
  if (!mine.data) return <EmptyState label="No stats yet." />;

  return (
    <>
      <p className="sx-muted">
        Compared against your team&apos;s median. Sentinel does not rank agents against each other.
      </p>
      <table className="pt-table pt-table--compare">
        <thead>
          <tr>
            <th>Metric</th>
            <th>You</th>
            <th>Team median</th>
          </tr>
        </thead>
        <tbody>
          <Comparison label="Calls" you={mine.data.calls} median={median.data?.calls} format={(v) => String(v)} />
          <Comparison label="Coverage" you={mine.data.coverage_pct} median={median.data?.coverage_pct} format={(v) => formatPercent(v, 1)} />
          <Comparison label="PTP rate" you={mine.data.ptp_rate} median={median.data?.ptp_rate} format={(v) => formatPercent(v, 1)} />
          <Comparison label="PTP value" you={mine.data.ptp_amount_paise} median={median.data?.ptp_amount_paise} format={(v) => formatPaise(v, { compactWholeRupees: true })} />
          <Comparison label="Sentiment delta" you={mine.data.avg_sentiment_delta} median={median.data?.avg_sentiment_delta} format={(v) => v.toFixed(2)} />
          <Comparison label="Talk to listen" you={mine.data.talk_ratio} median={median.data?.talk_ratio} format={(v) => formatPercent(v, 0)} />
          <Comparison label="Flags per 1000 calls" you={mine.data.flags_per_1000} median={median.data?.flags_per_1000} format={(v) => v.toFixed(1)} />
        </tbody>
      </table>
      {!teamId ? (
        <p className="pt-notice">You are not assigned to a team, so there is no median to compare against.</p>
      ) : median.error instanceof ApiError && median.error.isForbidden ? (
        // The median lives on a supervisor-scoped endpoint, so an agent — the very
        // role this comparison is for — cannot fetch it. Saying so beats an empty
        // column that reads as "your team has no data".
        <p className="pt-notice">
          The team median comes from a supervisor-scoped report your role may not read, so no comparison is shown.
        </p>
      ) : null}
    </>
  );
}

function Comparison({
  label,
  you,
  median,
  format,
}: {
  label: string;
  you: number | null | undefined;
  median: number | null | undefined;
  format: (value: number) => string;
}) {
  return (
    <tr>
      <td>{label}</td>
      <td className="sx-nums">{typeof you === 'number' ? format(you) : '—'}</td>
      <td className="sx-nums sx-muted">{typeof median === 'number' ? format(median) : '—'}</td>
    </tr>
  );
}

/* ------------------------------------------------------------- my flags */

function MyFlags() {
  const { api } = useSession();
  const flags = useAsync(async (signal) => api.listMyFlags(signal), [api]);

  if (flags.loading) return <LoadState />;
  if (flags.error) return <ErrorState error={flags.error} onRetry={flags.reload} />;
  if ((flags.data?.length ?? 0) === 0) return <EmptyState label="No flags on your calls." />;

  return (
    <ul className="pt-myflags">
      {(flags.data ?? []).map((flag) => (
        <MyFlagRow key={flag.id} flag={flag} onResponded={flags.reload} />
      ))}
    </ul>
  );
}

function MyFlagRow({ flag, onResponded }: { flag: Flag; onResponded: () => void }) {
  const { api } = useSession();
  const [response, setResponse] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    if (!response.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await api.respondToMyFlag(flag.id, response.trim());
      setResponse('');
      onResponded();
    } catch {
      setError('Could not send your response.');
    } finally {
      setBusy(false);
    }
  };

  return (
    <li className="pt-myflag">
      <div className="pt-myflag__head">
        <SeverityBadge severity={flag.severity} />
        {/* The rule id, not the rule body: agents must not see raw rule definitions. */}
        <span className="sx-mono">{flag.rule_id}</span>
        <span className="sx-muted">{flag.status}</span>
        {flag.span_start_ms !== null && flag.span_start_ms !== undefined ? (
          <span className="sx-muted sx-nums">at {formatDuration(flag.span_start_ms)}</span>
        ) : null}
      </div>
      {flag.evidence_text ? <blockquote>{flag.evidence_text}</blockquote> : null}
      {flag.judge_rationale ? <p className="sx-muted">{flag.judge_rationale}</p> : null}
      {flag.agent_response ? (
        <p className="pt-myflag__mine">Your response: {flag.agent_response}</p>
      ) : (
        <div className="pt-myflag__reply">
          <textarea
            value={response}
            maxLength={2000}
            placeholder="Explain or contest this flag"
            onChange={(event) => setResponse(event.target.value)}
          />
          <button className="sx-btn--primary" disabled={busy || !response.trim()} onClick={() => void submit()}>
            Send
          </button>
        </div>
      )}
      {error ? <p className="sx-error">{error}</p> : null}
    </li>
  );
}
