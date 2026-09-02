"""One LLM call per finalized call, producing the record in ``analyses``.

The output is constrained by ``contracts/schemas/analysis.json`` and validated before
anything is written. A schema failure retries once and then marks the call ``failed``
rather than persisting partial data: half an analysis on a compliance record is worse
than a visible gap, because nobody knows which half to trust.
"""

from __future__ import annotations

import json
from datetime import date
from pathlib import Path
from typing import Protocol

from jsonschema import Draft202012Validator

from ..models import (
    Analysis,
    CallContext,
    Channel,
    Disposition,
    PromiseToPay,
    Transcript,
)

ANALYSIS_PROMPT_VERSION = "analysis-v1"

_SCHEMA_PATH = (
    Path(__file__).resolve().parents[4] / "contracts" / "schemas" / "analysis.json"
)

# Calls shorter than this are logged but not analysed: a 9-second call is a hangup or
# a wrong number, and the model has nothing to work with. Skipping them is the single
# largest cost saving available and costs no information.
MIN_ANALYSABLE_MS = 15_000


class SchemaViolation(Exception):
    """The model returned something the contract does not allow."""

    def __init__(self, errors: list[str]):
        super().__init__("; ".join(errors[:5]))
        self.errors = errors


class AnalysisProvider(Protocol):
    """The model call, isolated behind an interface."""

    name: str
    version: str

    def complete(self, prompt: str, *, max_output_tokens: int) -> tuple[dict, int, int]:
        """Return the parsed JSON object plus (input_tokens, output_tokens)."""


PROMPT_TEMPLATE = """\
You are summarising one debt collection call from an Indian collections floor for the
agent who made it and for their supervisor.

The two speakers were recorded on separate channels, so attribution is exact:
- AGENT: the collections agent
- BORROWER: the person called

Call metadata:
- started at: {started_at} ({timezone})
- duration: {duration_s} seconds
- account reference: {account_ref}

Transcript:
{transcript}

Produce a JSON object with exactly these fields and nothing else:

- summary: 2 to 4 sentences. State what was discussed and what was agreed. Factual
  only: no advice to the agent, no coaching, no speculation about the borrower's
  circumstances or ability to pay.
- disposition: one of ptp, refusal, dispute, wrong_number, no_contact,
  callback_requested, partial_payment, escalation, other.
- ptp: {{"present": bool, "amount_paise": integer or null, "due_date": "YYYY-MM-DD"
  or null, "confidence": 0..1, "evidence_span_ms": [start, end]}}.
  **amount_paise is in paise, not rupees**: fifteen thousand rupees is 1500000.
  Only set present=true when the borrower actually committed to an amount or a date.
  "I will try" is not a promise to pay. If you are unsure of the amount, say so with
  a low confidence rather than guessing — a wrong amount is worse than no amount.
  evidence_span_ms must point at the words the amount and date came from.
- sentiment: {{"far": [...], "near": [...], "far_open": n, "far_close": n,
  "delta": n}} where far is the borrower and near is the agent, each a list of
  {{"t_ms": int, "v": -1..1}} sampled every 30 seconds.
- next_action: one short string, or null.
- talk_ratio: fraction of speech time on the agent's channel, 0..1.
- interruptions: count of times the agent began speaking while the borrower was
  already speaking.

Respond with the JSON object only.
"""


class CallAnalyzer:
    def __init__(self, provider: AnalysisProvider, *, max_output_tokens: int = 1_500,
                 max_transcript_chars: int = 48_000):
        self.provider = provider
        self.max_output_tokens = max_output_tokens
        self.max_transcript_chars = max_transcript_chars
        with _SCHEMA_PATH.open(encoding="utf-8") as fh:
            self._validator = Draft202012Validator(json.load(fh))

    # ------------------------------------------------------------------ prompt

    def render_transcript(self, transcript: Transcript) -> tuple[str, bool]:
        """Interleave the channels into an attributed script.

        Returns the text and whether it had to be truncated. A call that exceeds the
        per-call token ceiling is truncated with a marker rather than dropped: a
        partial analysis of a very long call is still worth having, and the marker
        tells anyone reading it that the model did not see the end.
        """
        turns: list[tuple[int, str, str]] = []
        for channel, ct in transcript.channels.items():
            speaker = "AGENT" if channel is Channel.NEAR else "BORROWER"
            if ct.words:
                turns.extend((w.start_ms, speaker, w.text) for w in ct.words)
            elif ct.text:
                turns.append((0, speaker, ct.text))
        turns.sort(key=lambda t: t[0])

        lines: list[str] = []
        speaker: str | None = None
        buffer: list[str] = []
        start = 0
        for t_ms, who, text in turns:
            if who != speaker:
                if buffer:
                    lines.append(f"[{start} ms] {speaker}: {' '.join(buffer)}")
                speaker, buffer, start = who, [], t_ms
            buffer.append(text)
        if buffer:
            lines.append(f"[{start} ms] {speaker}: {' '.join(buffer)}")

        rendered = "\n".join(lines)
        if len(rendered) <= self.max_transcript_chars:
            return rendered, False
        return rendered[: self.max_transcript_chars] + "\n[transcript truncated]", True

    def build_prompt(self, transcript: Transcript) -> tuple[str, bool]:
        ctx = transcript.context
        rendered, truncated = self.render_transcript(transcript)
        return (
            PROMPT_TEMPLATE.format(
                started_at=ctx.started_at.isoformat(),
                timezone=ctx.tenant_timezone,
                duration_s=ctx.duration_ms // 1000,
                account_ref=ctx.account_ref or "unknown",
                transcript=rendered,
            ),
            truncated,
        )

    # ----------------------------------------------------------------- analyse

    def analyse(self, transcript: Transcript) -> Analysis:
        """Analyse one call. Raises :class:`SchemaViolation` after one retry."""
        prompt, truncated = self.build_prompt(transcript)

        last_errors: list[str] = []
        for attempt in (1, 2):
            raw, in_tokens, out_tokens = self.provider.complete(
                prompt, max_output_tokens=self.max_output_tokens
            )
            errors = [
                f"{'/'.join(str(p) for p in e.absolute_path) or '<root>'}: {e.message}"
                for e in sorted(self._validator.iter_errors(raw), key=lambda e: e.path)
            ]
            if not errors:
                return self._to_analysis(raw, transcript.context, in_tokens, out_tokens, truncated)
            last_errors = errors
            if attempt == 1:
                # One retry, with the failure fed back. Models usually fix a
                # structural mistake when told exactly which field was wrong.
                prompt = (
                    prompt
                    + "\n\nYour previous response was rejected for these reasons:\n"
                    + "\n".join(f"- {e}" for e in errors[:5])
                    + "\nReturn corrected JSON only."
                )
        raise SchemaViolation(last_errors)

    def _to_analysis(self, raw: dict, ctx: CallContext, in_tokens: int, out_tokens: int,
                     truncated: bool) -> Analysis:
        ptp_raw = raw["ptp"]
        span = ptp_raw.get("evidence_span_ms")
        due = ptp_raw.get("due_date")
        ptp = PromiseToPay(
            present=bool(ptp_raw["present"]),
            amount_paise=ptp_raw.get("amount_paise"),
            due_date=date.fromisoformat(due) if due else None,
            confidence=float(ptp_raw.get("confidence", 0.0)),
            evidence_span_ms=(int(span[0]), int(span[1])) if span else None,
        )
        sentiment = dict(raw["sentiment"])
        # Recompute the delta rather than trusting the model's arithmetic. It is the
        # number supervisors actually act on, and it is defined as close minus open.
        sentiment["delta"] = float(sentiment["far_close"]) - float(sentiment["far_open"])
        return Analysis(
            summary=raw["summary"],
            disposition=Disposition(raw["disposition"]),
            ptp=ptp,
            sentiment=sentiment,
            talk_ratio=float(raw["talk_ratio"]),
            interruptions=int(raw["interruptions"]),
            next_action=raw.get("next_action"),
            model=self.provider.name,
            prompt_version=ANALYSIS_PROMPT_VERSION,
            input_tokens=in_tokens,
            output_tokens=out_tokens,
            truncated=truncated,
        )


def should_analyse(ctx: CallContext) -> bool:
    """Whether a call is worth a model call at all."""
    return ctx.duration_ms >= MIN_ANALYSABLE_MS


def sentiment_delta(sentiment: dict) -> float:
    """Open minus close on the borrower channel — the metric supervisors use.

    Computed server-side, never in the UI, so every surface reports the same number.
    """
    return float(sentiment.get("far_close", 0.0)) - float(sentiment.get("far_open", 0.0))


