/**
 * History tab (spec 13.2): last 20 calls for the signed-in user.
 *
 * This is one of the two things the widget is allowed to fetch over the API, since
 * the native layer holds only the current call. Rows expand inline; anything richer
 * deep-links to the portal.
 */
import { useEffect, useState } from 'react';
import {
  ApiError,
  DispositionChip,
  PtpChip,
  SentimentDeltaChip,
  formatDuration,
  formatTime,
} from '@sentinel/shared';
import type { ApiClient, CallSummary } from '@sentinel/shared';

const HISTORY_LIMIT = 20;

export function HistoryTab({ api, onOpenPortal }: { api: ApiClient | null; onOpenPortal: (path: string) => void }) {
  const [calls, setCalls] = useState<CallSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);

  useEffect(() => {
    if (!api) return;
    const controller = new AbortController();
    void api
      .listMyCalls({ limit: HISTORY_LIMIT }, controller.signal)
      .then((page) => setCalls(page.items))
      .catch((cause: unknown) => {
        if (controller.signal.aborted) return;
        setError(historyErrorMessage(cause));
      });
    return () => controller.abort();
  }, [api]);

  if (error) return <p className="sx-error wg-history__msg">{error}</p>;
  if (!calls) return <p className="sx-muted wg-history__msg">Loading…</p>;
  if (calls.length === 0) return <p className="sx-muted wg-history__msg">No calls yet today.</p>;

  return (
    <ul className="wg-history">
      {calls.map((call) => {
        const open = expanded === call.id;
        return (
          <li key={call.id} className="wg-history__item">
            <button
              className="wg-history__row"
              aria-expanded={open}
              onClick={() => setExpanded(open ? null : call.id)}
            >
              <span className="wg-history__time sx-nums">{formatTime(call.started_at)}</span>
              <span className="wg-history__ref">{call.account_ref ?? '—'}</span>
              <span className="wg-history__dur sx-nums">{formatDuration(call.duration_ms)}</span>
              <DispositionChip disposition={call.disposition} />
              <PtpChip ptp={call.ptp} />
              <SentimentDeltaChip delta={call.sentiment_delta} />
            </button>
            {open ? (
              <div className="wg-history__detail">
                <p>{call.summary ?? 'No summary yet.'}</p>
                <button onClick={() => onOpenPortal(`/me/calls/${call.id}`)}>Open in portal</button>
              </div>
            ) : null}
          </li>
        );
      })}
    </ul>
  );
}

/**
 * Three outcomes, three different instructions to the agent.
 *
 * The distinction that matters on a collections desktop is between "offline" and
 * "signed out". Being offline for a minute is routine and the agent should wait; being
 * signed out means the token the native layer holds is gone or was refused, and no
 * amount of waiting fixes it. Telling an agent to wait for a session that has ended is
 * how a shift gets spent not knowing the widget needed a sign-in.
 */
export function historyErrorMessage(cause: unknown): string {
  if (!(cause instanceof ApiError)) return 'Could not load your history.';
  if (cause.isMissingCredentials) return 'Sign in to see your history.';
  if (cause.isTransport) return 'History is unavailable while offline.';
  return 'Could not load your history.';
}
