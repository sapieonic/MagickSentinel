/**
 * The synced timeline: sentiment for both channels, compliance flags as pins, and
 * the playhead — one SVG, no chart library.
 *
 * Pins are clickable and seek the player. They are rendered after the sentiment
 * paths so they always sit on top; a pin hidden behind a line is a flag a reviewer
 * never opens.
 */
import { SEVERITY_RANK } from '@sentinel/shared';
import type { Flag, SentimentSeries } from '@sentinel/shared';
import { formatDuration } from '@sentinel/shared';

export interface CallTimelineProps {
  durationMs: number;
  sentiment: SentimentSeries | undefined;
  flags: readonly Flag[];
  positionMs: number;
  onSeek: (ms: number) => void;
  selectedFlagId?: string | null;
  onSelectFlag?: (flag: Flag) => void;
}

const HEIGHT = 72;
const PIN_ROW = 10;

export function CallTimeline({
  durationMs,
  sentiment,
  flags,
  positionMs,
  onSeek,
  selectedFlagId,
  onSelectFlag,
}: CallTimelineProps) {
  const span = Math.max(durationMs, 1);
  const plotTop = PIN_ROW + 6;
  const plotHeight = HEIGHT - plotTop;

  const x = (ms: number) => clamp01(ms / span) * 100;
  const y = (v: number) => plotTop + ((1 - clamp(v, -1, 1)) / 2) * plotHeight;

  const path = (points: SentimentSeries['far']) =>
    (points ?? [])
      .map((point, index) => `${index === 0 ? 'M' : 'L'}${x(point.t_ms).toFixed(3)},${y(point.v).toFixed(2)}`)
      .join(' ');

  // Only flags with a span can be pinned. One without a start offset is still in the
  // list below the player; it just has nowhere to sit on the timeline.
  const pinned = flags.filter((flag) => flag.span_start_ms !== null && flag.span_start_ms !== undefined);

  const seekFromEvent = (event: React.MouseEvent<SVGSVGElement>) => {
    const rect = event.currentTarget.getBoundingClientRect();
    if (rect.width === 0) return;
    onSeek(clamp01((event.clientX - rect.left) / rect.width) * span);
  };

  return (
    <div className="pt-timeline">
      <svg
        viewBox={`0 0 100 ${HEIGHT}`}
        preserveAspectRatio="none"
        height={HEIGHT}
        className="pt-timeline__svg"
        onClick={seekFromEvent}
        role="slider"
        aria-label="Call timeline"
        aria-valuemin={0}
        aria-valuemax={Math.round(span / 1000)}
        aria-valuenow={Math.round(positionMs / 1000)}
        aria-valuetext={formatDuration(positionMs)}
        tabIndex={0}
        onKeyDown={(event) => {
          // Keyboard scrubbing in 5 s steps; QA reviewers work through long calls and
          // dragging a 40-minute timeline with a mouse is unusable.
          if (event.key === 'ArrowRight') onSeek(Math.min(span, positionMs + 5000));
          if (event.key === 'ArrowLeft') onSeek(Math.max(0, positionMs - 5000));
        }}
      >
        <line x1={0} y1={y(0)} x2={100} y2={y(0)} className="pt-timeline__zero" vectorEffect="non-scaling-stroke" />
        <path d={path(sentiment?.far)} className="pt-timeline__far" vectorEffect="non-scaling-stroke" fill="none" />
        <path d={path(sentiment?.near)} className="pt-timeline__near" vectorEffect="non-scaling-stroke" fill="none" />

        {pinned.map((flag) => (
          <g key={flag.id} className="pt-pin-group">
            <line
              x1={x(flag.span_start_ms!)}
              y1={plotTop}
              x2={x(flag.span_start_ms!)}
              y2={HEIGHT}
              className={`pt-pin__stem pt-pin__stem--${flag.severity}`}
              vectorEffect="non-scaling-stroke"
            />
          </g>
        ))}

        <line
          x1={x(positionMs)}
          y1={0}
          x2={x(positionMs)}
          y2={HEIGHT}
          className="pt-timeline__playhead"
          vectorEffect="non-scaling-stroke"
        />
      </svg>

      {/* Pins are HTML rather than SVG circles so they are real buttons: focusable,
          keyboard-activatable and able to carry a tooltip without extra plumbing. */}
      <div className="pt-pins">
        {[...pinned]
          .sort((a, b) => SEVERITY_RANK[b.severity] - SEVERITY_RANK[a.severity])
          .map((flag) => (
            <button
              key={flag.id}
              className={`pt-pin pt-pin--${flag.severity}${selectedFlagId === flag.id ? ' pt-pin--on' : ''}`}
              style={{ left: `${x(flag.span_start_ms!)}%` }}
              title={`${flag.rule_id} · ${flag.severity} · ${formatDuration(flag.span_start_ms!)}`}
              onClick={() => {
                onSeek(flag.span_start_ms!);
                onSelectFlag?.(flag);
              }}
            >
              <span className="sx-visually-hidden">
                {flag.rule_id}, {flag.severity}, at {formatDuration(flag.span_start_ms!)}
              </span>
            </button>
          ))}
      </div>
    </div>
  );
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function clamp01(value: number): number {
  return clamp(value, 0, 1);
}
