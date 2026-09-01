import { CaptureTierBadge, formatPercent } from '@sentinel/shared';
import type { CaptureTier } from '@sentinel/shared';

/** Collapsed pill: ready dot, tier badge, today's coverage (spec 13.1). */
export function IdlePill({ tier, coverage }: { tier: CaptureTier; coverage: number | null }) {
  return (
    <div className="wg-pill">
      <span className="wg-pill__dot" title="Ready to record" aria-label="Ready to record" />
      <CaptureTierBadge tier={tier} />
      <span className="wg-pill__coverage sx-nums" title="Today's capture coverage">
        {formatPercent(coverage)}
      </span>
    </div>
  );
}
