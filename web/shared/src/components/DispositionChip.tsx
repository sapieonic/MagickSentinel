import type { Disposition } from '../api/types.js';

/** Agent-facing wording. The wire values stay snake_case; only labels are prose. */
export const DISPOSITION_LABEL: Record<Disposition, string> = {
  ptp: 'Promise to pay',
  refusal: 'Refusal',
  dispute: 'Dispute',
  wrong_number: 'Wrong number',
  no_contact: 'No contact',
  callback_requested: 'Callback requested',
  partial_payment: 'Partial payment',
  escalation: 'Escalation',
  other: 'Other',
};

export function DispositionChip({ disposition }: { disposition: Disposition | undefined }) {
  if (!disposition) return <span className="sx-chip sx-chip--muted">Unset</span>;
  return <span className={`sx-chip sx-disp sx-disp--${disposition}`}>{DISPOSITION_LABEL[disposition]}</span>;
}
