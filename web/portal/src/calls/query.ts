/**
 * Turns the explorer's form state into a `GET /v1/calls` query.
 *
 * Kept out of the component because two of the rules here are easy to get subtly
 * wrong and impossible to see in a screenshot: the inclusive end date, and the bank
 * client's flagged-only scope.
 */
import type { CallQuery, Disposition, Role } from '@sentinel/shared';

/** Tri-state rather than a checkbox: "any" and "not flagged" are different questions. */
export type FlagFilter = 'any' | 'flagged' | 'unflagged';

export interface ExplorerFilters {
  /** `yyyy-mm-dd` as produced by a date input, or '' for unset. */
  from: string;
  to: string;
  disposition: Disposition | '';
  flags: FlagFilter;
  /** Full-text over transcripts in scope. */
  q: string;
  teamId: string;
}

export const EMPTY_FILTERS: ExplorerFilters = {
  from: '',
  to: '',
  disposition: '',
  flags: 'any',
  q: '',
  teamId: '',
};

export function buildCallQuery(role: Role | null, filters: ExplorerFilters, limit: number): CallQuery {
  const query: CallQuery = { limit };

  if (filters.from) query.from = startOfDayIso(filters.from);
  // The gateway compares `started_at < to`, so a bare end date would drop the day
  // the user picked. Advance to the following midnight to make the range inclusive
  // of both endpoints, which is what a date picker labelled "to" promises.
  if (filters.to) query.to = startOfNextDayIso(filters.to);
  if (filters.disposition) query.disposition = filters.disposition;
  if (filters.q) query.q = filters.q;
  if (filters.teamId) query.team_id = filters.teamId;
  if (filters.flags !== 'any') query.has_flags = filters.flags === 'flagged';

  // A bank client's visible set is flagged calls and nothing else — row-level
  // security enforces that, and no filter here can widen it. Pinning the parameter
  // keeps the UI from ever showing a control that appears to promise otherwise, and
  // keeps the query on the partial index the gateway expects for this role.
  if (role === 'client') query.has_flags = true;

  return query;
}

/**
 * Day boundaries are the viewer's local midnight, not UTC. A supervisor in Kolkata
 * asking for "1 September" means their 1 September; UTC boundaries would silently
 * shift the window by five and a half hours and lose the last calls of the shift.
 */
function startOfDayIso(date: string): string {
  return dayStart(date).toISOString();
}

function startOfNextDayIso(date: string): string {
  const next = dayStart(date);
  next.setDate(next.getDate() + 1);
  return next.toISOString();
}

function dayStart(date: string): Date {
  // Constructed from parts: `new Date('2026-09-01')` is UTC midnight by spec, while
  // `new Date('2026-09-01T00:00:00')` is local — a difference too easy to lose in a
  // refactor to rely on.
  const [year, month, day] = date.split('-').map(Number);
  return new Date(year ?? 1970, (month ?? 1) - 1, day ?? 1, 0, 0, 0, 0);
}

/** `yyyy-mm-dd` for a date input, `days` before `now`, in local time. */
export function isoDateDaysAgo(days: number, now: Date = new Date()): string {
  const then = new Date(now.getTime());
  then.setDate(then.getDate() - days);
  const month = `${then.getMonth() + 1}`.padStart(2, '0');
  const day = `${then.getDate()}`.padStart(2, '0');
  return `${then.getFullYear()}-${month}-${day}`;
}
