/**
 * Sentiment sparkline: inline SVG, no chart library.
 *
 * Sentiment is bipolar (-1..1), so the zero line is drawn explicitly — without it a
 * falling-but-still-positive call looks identical to a call that went hostile.
 */
import type { SentimentPoint } from '../api/types.js';

export interface SentimentSparklineProps {
  points: readonly SentimentPoint[] | undefined;
  width?: number;
  height?: number;
  /** far = borrower, near = agent. Only used for the accessible label and hue. */
  channel?: 'far' | 'near';
  label?: string;
  /** Domain end in ms; defaults to the last sample. Pass call duration to keep two
   *  sparklines on the same time base — otherwise a short series looks stretched. */
  durationMs?: number;
}

const DEFAULT_WIDTH = 96;
const DEFAULT_HEIGHT = 24;

export function SentimentSparkline({
  points,
  width = DEFAULT_WIDTH,
  height = DEFAULT_HEIGHT,
  channel = 'far',
  label,
  durationMs,
}: SentimentSparklineProps) {
  const series = points ?? [];
  const title = label ?? (channel === 'far' ? 'Borrower sentiment' : 'Agent sentiment');

  if (series.length === 0) {
    return (
      <svg className="sx-spark sx-spark--empty" width={width} height={height} role="img" aria-label={`${title}: no data`}>
        <line x1={0} y1={height / 2} x2={width} y2={height / 2} className="sx-spark__zero" />
      </svg>
    );
  }

  const first = series[0]!;
  const last = series[series.length - 1]!;
  const t0 = first.t_ms;
  const t1 = Math.max(durationMs ?? last.t_ms, t0 + 1);
  const span = t1 - t0;

  const x = (t: number) => ((t - t0) / span) * width;
  // v is contractually -1..1; clamp anyway so a bad sample cannot draw outside the box.
  const y = (v: number) => ((1 - Math.min(1, Math.max(-1, v))) / 2) * height;

  const path = series.map((p, i) => `${i === 0 ? 'M' : 'L'}${x(p.t_ms).toFixed(2)},${y(p.v).toFixed(2)}`).join(' ');
  const endValue = last.v;

  return (
    <svg
      className={`sx-spark sx-spark--${channel}`}
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      role="img"
      aria-label={`${title}, ending at ${endValue.toFixed(2)} on a scale of -1 to 1`}
    >
      <line x1={0} y1={height / 2} x2={width} y2={height / 2} className="sx-spark__zero" />
      <path d={path} className="sx-spark__line" fill="none" />
      <circle cx={x(last.t_ms)} cy={y(endValue)} r={2} className="sx-spark__end" />
    </svg>
  );
}

/**
 * Single-number sentiment chip for list rows, where a sparkline would be noise.
 * Delta is close-minus-open on the far channel.
 */
export function SentimentDeltaChip({ delta }: { delta: number | null | undefined }) {
  if (delta === null || delta === undefined || !Number.isFinite(delta)) {
    return <span className="sx-chip sx-chip--muted">—</span>;
  }
  const tone = delta > 0.05 ? 'up' : delta < -0.05 ? 'down' : 'flat';
  const arrow = tone === 'up' ? '▲' : tone === 'down' ? '▼' : '▬';
  return (
    <span className={`sx-chip sx-sentiment sx-sentiment--${tone}`} title="Borrower sentiment, close minus open">
      {arrow} {delta > 0 ? '+' : ''}
      {delta.toFixed(2)}
    </span>
  );
}
