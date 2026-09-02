"""Tier-2 LLM judge.

Runs on calls tier 1 flagged, plus a small random sample of everything else, so
coercive framing and misrepresentation that no term list catches still gets seen.

The judge must return the transcript span it relied on. That is enforced here rather
than trusted: a verdict of ``upheld`` with no span is discarded, because a flag a
reviewer cannot trace to specific words is not usable as evidence with a bank.
"""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from jsonschema import Draft202012Validator

from ..models import Finding, Severity, Transcript

_SCHEMA_PATH = (
    Path(__file__).resolve().parents[4] / "contracts" / "schemas" / "judge.json"
)


def _validator() -> Draft202012Validator:
    with _SCHEMA_PATH.open(encoding="utf-8") as fh:
        return Draft202012Validator(json.load(fh))


@dataclass(frozen=True)
class JudgeVerdict:
    verdict: str
    rule_id: str
    confidence: float
    rationale: str
    evidence_span_ms: tuple[int, int] | None = None
    evidence_text: str | None = None

    @property
    def upheld(self) -> bool:
        return self.verdict == "upheld"


class JudgeProvider(Protocol):
    """The model call, isolated behind an interface.

    No provider SDK is imported outside :mod:`sentinel_pipeline.providers`; this is
    the seam.
    """

    name: str
    version: str

    def judge(self, prompt: str) -> tuple[dict, int, int]:
        """Return the parsed JSON verdict plus (input_tokens, output_tokens)."""


def should_judge(call_id: str, tier1_hit: bool, judge_rules: set[str], rule_ids: set[str],
                 sample_pct: float) -> bool:
    """Decide whether a call goes to the judge.

    Sampling is deterministic in the call id rather than random, so a re-run of the
    pipeline over the same call reaches the same decision. A pipeline whose cost and
    output change when you replay it is impossible to reason about, and re-runs
    happen constantly during an ASR evaluation.
    """
    if tier1_hit and (rule_ids & judge_rules):
        return True
    if sample_pct <= 0:
        return False
    digest = hashlib.sha256(call_id.encode("utf-8")).digest()
    bucket = int.from_bytes(digest[:4], "big") % 10_000
    return bucket < sample_pct * 100


PROMPT_VERSION = "judge-v1"

PROMPT_TEMPLATE = """\
You are reviewing one debt collection call for compliance with RBI fair practices
and recovery agent guidelines in India.

The two speakers are on separate channels, so attribution is exact and not inferred:
- AGENT: the collections agent
- BORROWER: the person called

Rule under review: {rule_id}
Rule description: {rule_description}

Decide whether the AGENT violated this rule. Judge only what was actually said.
Do not infer intent from tone, and do not apply rules other than the one named.

A borrower being rude, angry or abusive is never a violation by the agent.
An agent accurately describing a lawful consequence is not a threat; an agent
asserting a consequence the lender cannot lawfully impose is.

Transcript:
{transcript}

Respond with JSON only, matching this shape:
{{"verdict": "upheld" | "not_a_violation" | "inconclusive",
  "rule_id": "{rule_id}",
  "confidence": 0.0-1.0,
  "rationale": "one paragraph referring to what was actually said",
  "evidence_span_ms": [start_ms, end_ms],
  "evidence_text": "the verbatim words you relied on"}}

If you uphold the violation you MUST supply evidence_span_ms and evidence_text
quoting the agent verbatim. A verdict with no traceable evidence will be discarded.
"""

RULE_DESCRIPTIONS = {
    "abusive_language": "The agent used abusive, humiliating or demeaning language.",
    "threat_of_violence": "The agent threatened physical harm, or implied it.",
    "false_legal_threat": (
        "The agent claimed arrest, a police complaint, a criminal case or a warrant "
        "when a defaulted unsecured debt is a civil matter."
    ),
    "false_seizure_threat": (
        "The agent claimed property or a vehicle would be seized without describing a "
        "lawful process."
    ),
    "third_party_disclosure": (
        "The agent disclosed debt details to someone who had said they were not the "
        "borrower."
    ),
}


class ComplianceJudge:
    """Runs the tier-2 review over one finding."""

    def __init__(self, provider: JudgeProvider, max_transcript_chars: int = 24_000):
        self.provider = provider
        self.max_transcript_chars = max_transcript_chars
        self._validator = _validator()

    def render_transcript(self, transcript: Transcript, around_ms: int | None = None) -> str:
        """Interleave the two channels into a readable, attributed script.

        When a span is known the transcript is centred on it, because sending the
        whole of a 20-minute call to judge one sentence costs tokens without
        improving the verdict.
        """
        turns: list[tuple[int, str, str]] = []
        for channel, ct in transcript.channels.items():
            speaker = "AGENT" if channel.speaker == "agent" else "BORROWER"
            if ct.words:
                for w in ct.words:
                    turns.append((w.start_ms, speaker, w.text))
            elif ct.text:
                turns.append((0, speaker, ct.text))
        turns.sort(key=lambda t: t[0])

        if around_ms is not None:
            lo, hi = around_ms - 90_000, around_ms + 90_000
            windowed = [t for t in turns if lo <= t[0] <= hi]
            if windowed:
                turns = windowed

        lines: list[str] = []
        current_speaker: str | None = None
        buffer: list[str] = []
        start = 0
        for t_ms, speaker, text in turns:
            if speaker != current_speaker:
                if buffer:
                    lines.append(f"[{start} ms] {current_speaker}: {' '.join(buffer)}")
                current_speaker, buffer, start = speaker, [], t_ms
            buffer.append(text)
        if buffer:
            lines.append(f"[{start} ms] {current_speaker}: {' '.join(buffer)}")

        rendered = "\n".join(lines)
        if len(rendered) > self.max_transcript_chars:
            # Truncate with a marker rather than dropping the call: a partial review
            # of a long call is still worth having, and the marker tells a reviewer
            # the judge did not see everything.
            rendered = rendered[: self.max_transcript_chars] + "\n[transcript truncated]"
        return rendered

    def build_prompt(self, finding: Finding, transcript: Transcript) -> str:
        return PROMPT_TEMPLATE.format(
            rule_id=finding.rule_id,
            rule_description=RULE_DESCRIPTIONS.get(finding.rule_id, finding.rule_id),
            transcript=self.render_transcript(transcript, finding.span_start_ms),
        )

    def review(self, finding: Finding, transcript: Transcript) -> tuple[JudgeVerdict | None, int, int]:
        """Judge one finding.

        Returns ``(verdict, input_tokens, output_tokens)``. The verdict is ``None``
        when the model's output does not satisfy the schema — which includes the
        case of upholding a violation without citing the words — so a malformed
        response can never become a flag on someone's record.
        """
        prompt = self.build_prompt(finding, transcript)
        raw, in_tokens, out_tokens = self.provider.judge(prompt)

        errors = sorted(self._validator.iter_errors(raw), key=lambda e: e.path)
        if errors:
            return None, in_tokens, out_tokens
        if raw.get("rule_id") != finding.rule_id:
            # The judge answered about a different rule; that verdict cannot be
            # attached to this finding.
            return None, in_tokens, out_tokens

        span = raw.get("evidence_span_ms")
        return (
            JudgeVerdict(
                verdict=raw["verdict"],
                rule_id=raw["rule_id"],
                confidence=float(raw["confidence"]),
                rationale=raw["rationale"],
                evidence_span_ms=(int(span[0]), int(span[1])) if span else None,
                evidence_text=raw.get("evidence_text"),
            ),
            in_tokens,
            out_tokens,
        )

    def apply(self, finding: Finding, verdict: JudgeVerdict) -> Finding | None:
        """Turn an upheld verdict into a tier-2 finding.

        A ``not_a_violation`` verdict removes the finding entirely: the point of the
        judge is to keep the queue defensible, and a tier-1 hit the judge overturned
        would waste a reviewer's time. ``inconclusive`` keeps the tier-1 finding as
        it was, for a human to settle.
        """
        if verdict.verdict == "not_a_violation":
            return None
        if verdict.verdict == "inconclusive":
            return finding
        span = verdict.evidence_span_ms or (finding.span_start_ms, finding.span_end_ms)
        return Finding(
            rule_id=finding.rule_id,
            severity=finding.severity,
            tier=2,
            span_start_ms=span[0] if span else None,
            span_end_ms=span[1] if span else None,
            evidence_text=verdict.evidence_text or finding.evidence_text,
            rationale=verdict.rationale,
            confidence=verdict.confidence,
        )


def escalate(findings: list[Finding], judge_rules: set[str]) -> list[Finding]:
    """The subset of tier-1 findings the tenant wants a judge to review."""
    return [f for f in findings if f.rule_id in judge_rules]


def severity_of(findings: list[Finding]) -> Severity | None:
    if not findings:
        return None
    return max((f.severity for f in findings), key=lambda s: s.rank)
