/**
 * Aggregate compliance posture for the bank client view.
 *
 * Computed in the browser from the page of flagged calls the caller can see,
 * because no aggregate endpoint exists and inventing one client-side that looks
 * authoritative would be worse than showing the arithmetic over a stated window.
 * `partial` says whether the window was truncated, so the numbers are never
 * presented as a complete count when they are not one.
 *
 * There is deliberately no per-agent breakdown. A bank contracts with the agency,
 * not with its agents; handing the client a ranked list of named collectors turns a
 * compliance report into a performance-management channel nobody agreed to.
 */
import { SEVERITY_RANK } from '@sentinel/shared';
import type { CallSummary, Severity } from '@sentinel/shared';

export interface PosturePeriod {
  /** Calls in the window that carry at least one flag. */
  flaggedCalls: number;
  /** Sum of `flag_count`: one call can breach several rules. */
  totalFlags: number;
  /** Flagged calls counted by their worst flag. */
  bySeverity: Record<Severity, number>;
  /** Worst severity seen, or undefined when nothing is flagged. */
  worst: Severity | undefined;
  /** Flagged calls whose worst flag is high or critical — the escalation set. */
  seriousCalls: number;
  earliest: string | undefined;
  latest: string | undefined;
  /** True when the listing was cut short, so these are the most recent, not all. */
  partial: boolean;
}

export function summarisePosture(calls: readonly CallSummary[], partial: boolean): PosturePeriod {
  const bySeverity: Record<Severity, number> = { low: 0, medium: 0, high: 0, critical: 0 };
  let totalFlags = 0;
  let worst: Severity | undefined;
  let seriousCalls = 0;
  let earliest: string | undefined;
  let latest: string | undefined;
  let earliestAt = Infinity;
  let latestAt = -Infinity;

  for (const call of calls) {
    totalFlags += call.flag_count;
    const severity = call.max_severity;
    if (severity) {
      bySeverity[severity] += 1;
      if (!worst || SEVERITY_RANK[severity] > SEVERITY_RANK[worst]) worst = severity;
      if (severity === 'high' || severity === 'critical') seriousCalls += 1;
    }
    // Compared as instants, not as strings: two valid RFC 3339 timestamps with
    // different offsets do not sort lexicographically.
    const at = Date.parse(call.started_at);
    if (Number.isNaN(at)) continue;
    if (at < earliestAt) [earliestAt, earliest] = [at, call.started_at];
    if (at > latestAt) [latestAt, latest] = [at, call.started_at];
  }

  return {
    flaggedCalls: calls.length,
    totalFlags,
    bySeverity,
    ...(worst ? { worst } : { worst: undefined }),
    seriousCalls,
    ...(earliest ? { earliest } : { earliest: undefined }),
    ...(latest ? { latest } : { latest: undefined }),
    partial,
  };
}
