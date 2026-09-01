import type { Severity } from '../api/types.js';

const LABEL: Record<Severity, string> = {
  low: 'Low',
  medium: 'Medium',
  high: 'High',
  critical: 'Critical',
};

/** Ordering used everywhere flags are sorted, so the queue and the detail agree. */
export const SEVERITY_RANK: Record<Severity, number> = { critical: 3, high: 2, medium: 1, low: 0 };

export function SeverityBadge({ severity, count }: { severity: Severity | undefined; count?: number }) {
  if (!severity) return <span className="sx-chip sx-chip--muted">—</span>;
  return (
    <span className={`sx-badge sx-sev sx-sev--${severity}`}>
      {LABEL[severity]}
      {count !== undefined && count > 1 ? ` ×${count}` : ''}
    </span>
  );
}
