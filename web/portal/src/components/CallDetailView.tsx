/**
 * Call detail: player, speaker-separated transcript, sentiment along the timeline,
 * flags as pins that seek the player.
 *
 * Presentational on purpose — it takes a CallDetail and nothing else — so the same
 * component serves the QA explorer and the agent's own-call view, with playback
 * gated by the caller rather than re-decided here.
 */
import { useEffect, useMemo, useRef, useState } from 'react';
import {
  CaptureTierBadge,
  DispositionChip,
  PtpChip,
  SentimentDeltaChip,
  SeverityBadge,
  formatDateTime,
  formatDuration,
  formatPercent,
} from '@sentinel/shared';
import type { CallDetail, Flag, TranscriptTurn } from '@sentinel/shared';
import { CallTimeline } from './CallTimeline.js';

export interface CallDetailViewProps {
  call: CallDetail;
  /** Role + tenant policy decision, made by the caller. */
  audioAllowed: boolean;
  /** Rendered under the flag list; lets the compliance queue add its own controls. */
  flagActions?: (flag: Flag) => React.ReactNode;
}

export function CallDetailView({ call, audioAllowed, flagActions }: CallDetailViewProps) {
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const [positionMs, setPositionMs] = useState(0);
  const [selectedFlagId, setSelectedFlagId] = useState<string | null>(null);

  const turns = call.transcript ?? [];
  const flags = call.flags ?? [];

  // Prefer the recorded duration; fall back to the transcript's end so the timeline
  // still has a sane domain on a call whose duration never got written.
  const durationMs = useMemo(() => {
    if (call.duration_ms) return call.duration_ms;
    const last = turns[turns.length - 1];
    return last ? last.end_ms : 0;
  }, [call.duration_ms, turns]);

  const seek = (ms: number) => {
    setPositionMs(ms);
    const audio = audioRef.current;
    // Seeking without audio still moves the transcript highlight: a reviewer with no
    // playback rights can still navigate the call by its flags.
    if (audio && Number.isFinite(audio.duration)) audio.currentTime = ms / 1000;
  };

  useEffect(() => {
    setPositionMs(0);
    setSelectedFlagId(null);
  }, [call.id]);

  const activeTurnIndex = useMemo(() => findTurnAt(turns, positionMs), [turns, positionMs]);

  return (
    <div className="pt-detail">
      <header className="pt-detail__head">
        <div>
          <h2>{call.account_ref ?? 'Call'}</h2>
          <p className="sx-muted">
            {formatDateTime(call.started_at)} · {formatDuration(call.duration_ms)} · {call.status}
          </p>
        </div>
        <div className="pt-detail__chips">
          <CaptureTierBadge tier={call.capture_tier} verbose />
          <DispositionChip disposition={call.disposition} />
          <PtpChip ptp={call.ptp} />
          <SentimentDeltaChip delta={call.sentiment_delta} />
        </div>
      </header>

      {call.capture_tier === 'B' ? (
        <p className="pt-notice">
          Captured on tier B: both sides are mixed into one stream, so speaker labels and per-side sentiment are
          less reliable than on tier A.
        </p>
      ) : null}

      {call.summary ? <p className="pt-detail__summary">{call.summary}</p> : null}

      <CallTimeline
        durationMs={durationMs}
        sentiment={call.sentiment}
        flags={flags}
        positionMs={positionMs}
        onSeek={seek}
        selectedFlagId={selectedFlagId}
        onSelectFlag={(flag) => setSelectedFlagId(flag.id)}
      />

      {audioAllowed && call.audio_url ? (
        <audio
          ref={audioRef}
          className="pt-player"
          controls
          preload="none"
          src={call.audio_url}
          onTimeUpdate={(event) => setPositionMs(event.currentTarget.currentTime * 1000)}
        />
      ) : (
        <p className="pt-notice sx-muted">
          {audioAllowed
            ? 'No recording is available for this call.'
            : 'Audio playback is not enabled for your role on this tenant. The transcript and flags are unaffected.'}
        </p>
      )}

      <div className="pt-detail__grid">
        <section className="pt-transcript" aria-label="Transcript">
          {turns.length === 0 ? (
            <p className="sx-muted">No transcript yet.</p>
          ) : (
            turns.map((turn, index) => (
              <button
                key={`${turn.start_ms}-${index}`}
                className={`pt-turn pt-turn--${turn.speaker}${index === activeTurnIndex ? ' pt-turn--active' : ''}`}
                onClick={() => seek(turn.start_ms)}
              >
                <span className="pt-turn__meta sx-nums">
                  {formatDuration(turn.start_ms)} · {turn.speaker}
                </span>
                <span className="pt-turn__text">{turn.text}</span>
              </button>
            ))
          )}
        </section>

        <aside className="pt-side">
          <h3>Flags ({flags.length})</h3>
          {flags.length === 0 ? (
            <p className="sx-muted">No compliance flags.</p>
          ) : (
            <ul className="pt-flaglist">
              {flags.map((flag) => (
                <li
                  key={flag.id}
                  className={selectedFlagId === flag.id ? 'pt-flaglist__item pt-flaglist__item--on' : 'pt-flaglist__item'}
                >
                  <button
                    className="pt-flaglist__seek"
                    onClick={() => {
                      setSelectedFlagId(flag.id);
                      if (flag.span_start_ms !== null && flag.span_start_ms !== undefined) seek(flag.span_start_ms);
                    }}
                  >
                    <SeverityBadge severity={flag.severity} />
                    <span className="pt-flaglist__rule sx-mono">{flag.rule_id}</span>
                    <span className="sx-muted">{flag.tier === 1 ? 'rule' : 'judge'}</span>
                  </button>
                  {flag.evidence_text ? <blockquote>{flag.evidence_text}</blockquote> : null}
                  {flag.judge_rationale ? <p className="sx-muted">{flag.judge_rationale}</p> : null}
                  {flagActions?.(flag)}
                </li>
              ))}
            </ul>
          )}

          <h3>Call metrics</h3>
          <dl className="pt-kv">
            <dt>Talk ratio</dt>
            <dd>{formatPercent(call.talk_ratio, 0)}</dd>
            <dt>Interruptions</dt>
            <dd>{call.interruptions ?? '—'}</dd>
            <dt>Next action</dt>
            <dd>{call.next_action ?? '—'}</dd>
          </dl>

          {call.analysis ? (
            <p className="pt-prov sx-muted sx-mono">
              {call.analysis.asr_provider ?? '?'} {call.analysis.asr_version ?? ''} · {call.analysis.model ?? '?'}{' '}
              {call.analysis.prompt_version ?? ''}
            </p>
          ) : null}
        </aside>
      </div>
    </div>
  );
}

/**
 * Binary search rather than a linear scan: this runs on every timeupdate (roughly
 * 4 Hz) over transcripts that reach a few thousand turns on a long call.
 */
function findTurnAt(turns: readonly TranscriptTurn[], positionMs: number): number {
  let low = 0;
  let high = turns.length - 1;
  let found = -1;
  while (low <= high) {
    const mid = (low + high) >> 1;
    const turn = turns[mid]!;
    if (positionMs < turn.start_ms) {
      high = mid - 1;
    } else {
      found = mid;
      low = mid + 1;
    }
  }
  if (found === -1) return -1;
  return positionMs <= turns[found]!.end_ms ? found : -1;
}
