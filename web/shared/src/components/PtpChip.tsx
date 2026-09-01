import type { Ptp } from '../api/types.js';
import { formatPaise } from '../money.js';

/**
 * Shows the agent's confirmed figures in preference to the model's, because a
 * confirmation is the human correcting the extraction and the corrected number is
 * the one the floor acts on. The model's value stays visible in the tooltip so a
 * reviewer can still see what was extracted.
 */
export function PtpChip({ ptp }: { ptp: Ptp | null | undefined }) {
  if (!ptp || ptp.present === false) return <span className="sx-chip sx-chip--muted">No PTP</span>;

  const amount = ptp.agent_amount_paise ?? ptp.amount_paise ?? null;
  const dueDate = ptp.agent_due_date ?? ptp.due_date ?? null;
  const corrected = ptp.agent_amount_paise !== null && ptp.agent_amount_paise !== undefined;

  if (amount === null && dueDate === null) {
    return <span className="sx-chip sx-ptp">PTP</span>;
  }

  const modelValue =
    corrected && ptp.amount_paise !== null && ptp.amount_paise !== undefined
      ? `Model extracted ${formatPaise(ptp.amount_paise)}`
      : undefined;

  return (
    <span className={`sx-chip sx-ptp${corrected ? ' sx-ptp--confirmed' : ''}`} {...(modelValue ? { title: modelValue } : {})}>
      {amount !== null ? formatPaise(amount, { compactWholeRupees: true }) : 'PTP'}
      {dueDate ? <span className="sx-ptp__date"> by {dueDate}</span> : null}
      {corrected ? <span className="sx-ptp__tick" aria-label="agent confirmed"> ✓</span> : null}
    </span>
  );
}
