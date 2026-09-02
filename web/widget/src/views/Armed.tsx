import { CaptureTierBadge, RecordingIndicator } from '@sentinel/shared';
import type { CaptureTier } from '@sentinel/shared';

export function Armed({ tier }: { tier: CaptureTier }) {
  return (
    <div className="wg-panel wg-armed">
      <div className="wg-row">
        <span className="wg-armed__text">Call detected…</span>
        <CaptureTierBadge tier={tier} />
      </div>
      {/* Capture is already running in ARMED; the indicator goes up with it. */}
      <RecordingIndicator active tierB={tier === 'B'} />
    </div>
  );
}
