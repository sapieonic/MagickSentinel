import { RecordingIndicator } from '@sentinel/shared';
import type { CaptureTier } from '@sentinel/shared';

/**
 * The 3–5 s gap between hangup and the post-call card. The indicator stays up
 * because the tail of the call is still being flushed.
 */
export function Wrap({ tier }: { tier: CaptureTier }) {
  return (
    <div className="wg-panel wg-wrap">
      <span className="wg-spinner" aria-hidden="true" />
      <span>Processing…</span>
      <RecordingIndicator active tierB={tier === 'B'} />
    </div>
  );
}
