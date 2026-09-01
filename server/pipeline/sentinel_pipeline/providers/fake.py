"""Deterministic providers for tests and local development.

These are not mocks that assert they were called. They produce plausible, schema-valid
output derived from the input, so a test can exercise the whole pipeline — validation,
retry, cost accounting, flag creation — and fail for a real reason.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field

from ..models import Channel, Transcript, Word
from ..asr.base import ASRResult


@dataclass
class FakeASR:
    """Returns a canned transcript, with word timings laid out at a real speaking rate."""

    name: str = "fake-asr"
    version: str = "1"
    text: str = "hello this is a test call"
    language: str = "en"
    words_per_minute: int = 150

    def transcribe(self, audio: bytes, *, sample_rate: int,
                   language_hint: str | None = None) -> ASRResult:
        ms_per_word = int(60_000 / self.words_per_minute)
        words = []
        t = 0
        for token in self.text.split():
            words.append(Word(text=token, start_ms=t, end_ms=t + ms_per_word, confidence=0.9))
            t += ms_per_word
        return ASRResult(
            text=self.text, words=words, language=language_hint or self.language,
            confidence=0.9, provider=self.name, provider_version=self.version,
        )


@dataclass
class FakeAnalysisProvider:
    """Produces a schema-valid analysis from the transcript in the prompt.

    ``responses`` lets a test queue exact payloads — including deliberately invalid
    ones, to exercise the retry-then-fail path.
    """

    name: str = "fake-analysis"
    version: str = "1"
    responses: list[dict] = field(default_factory=list)
    calls: int = 0
    prompts: list[str] = field(default_factory=list)

    def complete(self, prompt: str, *, max_output_tokens: int) -> tuple[dict, int, int]:
        self.calls += 1
        self.prompts.append(prompt)
        if self.responses:
            payload = self.responses.pop(0)
        else:
            payload = self._derive(prompt)
        return payload, len(prompt) // 4, len(json.dumps(payload)) // 4

    def _derive(self, prompt: str) -> dict:
        # Pull an amount out of the transcript if there is one, so PTP extraction
        # tests exercise a real path rather than a constant.
        m = re.search(r"(?:rs\.?|₹)\s*([\d,]+)", prompt, re.IGNORECASE)
        amount_paise = int(m.group(1).replace(",", "")) * 100 if m else None
        return {
            "summary": "The agent contacted the borrower about an overdue amount and "
                       "discussed repayment. The borrower responded.",
            "disposition": "ptp" if amount_paise else "no_contact",
            "ptp": {
                "present": amount_paise is not None,
                "amount_paise": amount_paise,
                "due_date": "2026-09-15" if amount_paise else None,
                "confidence": 0.8 if amount_paise else 0.0,
                "evidence_span_ms": [1000, 2000] if amount_paise else None,
            },
            "sentiment": {
                "far": [{"t_ms": 0, "v": -0.1}, {"t_ms": 30000, "v": -0.4}],
                "near": [{"t_ms": 0, "v": 0.2}, {"t_ms": 30000, "v": 0.1}],
                # Deliberately wrong: the analyser must recompute this rather than
                # trusting the model's arithmetic.
                "far_open": -0.1, "far_close": -0.4, "delta": 0.9,
            },
            "next_action": "Call back on the fifteenth to confirm payment.",
            "talk_ratio": 0.55,
            "interruptions": 2,
        }


@dataclass
class FakeJudgeProvider:
    """Returns a queued verdict, or upholds by default with a traceable span."""

    name: str = "fake-judge"
    version: str = "1"
    responses: list[dict] = field(default_factory=list)
    prompts: list[str] = field(default_factory=list)

    def judge(self, prompt: str) -> tuple[dict, int, int]:
        self.prompts.append(prompt)
        if self.responses:
            payload = self.responses.pop(0)
        else:
            payload = {
                "verdict": "upheld",
                "rule_id": _rule_id_in(prompt),
                "confidence": 0.9,
                "rationale": "The agent asserted a consequence the lender cannot "
                             "lawfully impose, which is the substance of this rule.",
                "evidence_span_ms": [30000, 34000],
                "evidence_text": "we will file a police case",
            }
        return payload, len(prompt) // 4, 120


def _rule_id_in(prompt: str) -> str:
    m = re.search(r"Rule under review: (\S+)", prompt)
    return m.group(1) if m else "unknown"


def transcript_text(transcript: Transcript) -> str:
    parts = []
    for channel in (Channel.NEAR, Channel.FAR):
        ct = transcript.get(channel)
        if ct:
            parts.append(f"{channel.speaker}: {ct.text}")
    return "\n".join(parts)
