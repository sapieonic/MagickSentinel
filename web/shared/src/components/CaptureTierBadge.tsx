import type { CaptureTier } from '../api/types.js';

/**
 * Tier B is visually distinct on purpose: on B the two channels are not separately
 * captured, so speaker separation and per-side sentiment are weaker. Anyone reading a
 * B call needs to know that before they trust the numbers.
 */
export function CaptureTierBadge({ tier, verbose = false }: { tier: CaptureTier | undefined; verbose?: boolean }) {
  if (!tier) return null;
  const title =
    tier === 'A'
      ? 'Tier A — per-process loopback, channels captured separately'
      : 'Tier B — mixed loopback, reduced speaker separation';
  return (
    <span className={`sx-badge sx-tier sx-tier--${tier.toLowerCase()}`} title={title}>
      {verbose ? `Tier ${tier}` : tier}
      {tier === 'B' && verbose ? <span className="sx-tier__note"> · mixed audio</span> : null}
    </span>
  );
}
