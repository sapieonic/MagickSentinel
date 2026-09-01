import { useEffect, useState } from 'react';
import { CaptureTierBadge, RecordingIndicator, SentimentSparkline, formatDuration } from '@sentinel/shared';
import type { CaptureTier, SentimentPoint } from '@sentinel/shared';

export interface InCallProps {
  tier: CaptureTier;
  startedAtEpochMs: number;
  /** Live sentiment is a phase-5 feature; until then these stay empty and the
   *  sparklines render as flat baselines rather than disappearing, so the widget
   *  does not change size mid-call. */
  far?: readonly SentimentPoint[];
  near?: readonly SentimentPoint[];
}

export function InCall({ tier, startedAtEpochMs, far, near }: InCallProps) {
  const elapsed = useElapsed(startedAtEpochMs);
  return (
    <div className="wg-panel wg-incall">
      <div className="wg-row">
        <span className="wg-timer sx-nums">{formatDuration(elapsed)}</span>
        <CaptureTierBadge tier={tier} />
      </div>
      <div className="wg-sparks">
        <div className="wg-spark-row">
          <span className="sx-muted">Borrower</span>
          <SentimentSparkline points={far} channel="far" />
        </div>
        <div className="wg-spark-row">
          <span className="sx-muted">You</span>
          <SentimentSparkline points={near} channel="near" />
        </div>
      </div>
      <RecordingIndicator active tierB={tier === 'B'} />
    </div>
  );
}

/** Ticks once a second. A rAF loop would repaint 60× for a display that changes 1×. */
function useElapsed(startedAtEpochMs: number): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);
  return Math.max(0, now - startedAtEpochMs);
}
