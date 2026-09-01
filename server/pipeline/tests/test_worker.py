"""The finalize pipeline, and how it degrades.

The ordering assertions here are the point. Losing a summary is an inconvenience;
losing compliance coverage is a breach of what was sold, so the tests below pin the
rule that analysis failure must never stop tier-1 evaluation.
"""

from dataclasses import dataclass, field

import pytest

from conftest import COMPLIANT_OPENING, call, channel
from sentinel_pipeline.analysis import CallAnalyzer
from sentinel_pipeline.compliance.engine import RuleEngine, load_default_rule_set
from sentinel_pipeline.compliance.judge import ComplianceJudge
from sentinel_pipeline.cost import CostPolicy, ModelPricing, TenantBudget
from sentinel_pipeline.models import Analysis, Channel, Finding, Transcript
from sentinel_pipeline.providers import FakeAnalysisProvider, FakeASR, FakeJudgeProvider
from sentinel_pipeline.worker import Finalizer

CLEAN = (
    COMPLIANT_OPENING
    + " Can you pay Rs 15,000 by the fifteenth? Thank you, I have noted that."
)
THREATENING = COMPLIANT_OPENING + " If you do not pay we will file a police case against you."

PRICING = {
    "fake-analysis": ModelPricing("fake-analysis", 25_000, 125_000),
    "fake-judge": ModelPricing("fake-judge", 25_000, 125_000),
}


@dataclass
class FakeSegments:
    have_far: bool = True
    have_near: bool = True

    def channel_audio(self, call_id: str, channel_: Channel) -> bytes | None:
        if channel_ is Channel.FAR and not self.have_far:
            return None
        if channel_ is Channel.NEAR and not self.have_near:
            return None
        return b"\x00" * 320


@dataclass
class FakeSink:
    transcripts: dict = field(default_factory=dict)
    analyses: dict = field(default_factory=dict)
    findings: dict = field(default_factory=dict)
    statuses: list = field(default_factory=list)

    def save_transcript(self, call_id: str, transcript: Transcript) -> None:
        self.transcripts[call_id] = transcript

    def save_analysis(self, call_id: str, analysis: Analysis, cost_paise: int) -> None:
        self.analyses[call_id] = (analysis, cost_paise)

    def save_findings(self, call_id: str, rule_set_version: int, findings: list[Finding]) -> None:
        self.findings[call_id] = (rule_set_version, findings)

    def set_status(self, call_id: str, status: str) -> None:
        self.statuses.append(status)


class ExplodingASR:
    name, version = "boom", "1"

    def transcribe(self, audio, *, sample_rate, language_hint=None):
        raise RuntimeError("provider unavailable")


class ExplodingAnalysis:
    name, version = "boom", "1"

    def complete(self, prompt, *, max_output_tokens):
        raise RuntimeError("provider unavailable")


class ExplodingJudge:
    name, version = "fake-judge", "1"

    def judge(self, prompt):
        raise RuntimeError("provider unavailable")


def build(*, asr=None, analysis_provider=None, judge_provider=None, segments=None,
          policy=None) -> tuple[Finalizer, FakeSink]:
    sink = FakeSink()
    rules = RuleEngine(load_default_rule_set())
    analyzer = CallAnalyzer(analysis_provider) if analysis_provider is not None else None
    judge = ComplianceJudge(judge_provider) if judge_provider is not None else None
    return (
        Finalizer(
            asr=asr or FakeASR(text=CLEAN),
            analyzer=analyzer,
            rules=rules,
            judge=judge,
            segments=segments or FakeSegments(),
            sink=sink,
            cost_policy=policy or CostPolicy(pricing=PRICING),
        ),
        sink,
    )


def ctx(duration_ms=300_000):
    return call(duration_ms=duration_ms).context


def test_a_clean_call_completes_with_a_transcript_analysis_and_no_flags():
    f, sink = build(analysis_provider=FakeAnalysisProvider())
    budget = TenantBudget("t", 10_000_000)
    outcome = f.finalize(ctx(), budget)

    assert outcome.status == "complete"
    assert outcome.transcript is not None
    assert outcome.analysis is not None
    assert outcome.findings == []
    assert sink.statuses[-1] == "complete"
    assert budget.spent_paise > 0, "model spend must be recorded per call"


def test_a_threatening_call_is_flagged_and_judged():
    f, sink = build(asr=FakeASR(text=THREATENING),
                    analysis_provider=FakeAnalysisProvider(),
                    judge_provider=FakeJudgeProvider())
    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))
    rule_ids = {x.rule_id for x in outcome.findings}
    assert "false_legal_threat" in rule_ids
    judged = next(x for x in outcome.findings if x.rule_id == "false_legal_threat")
    assert judged.tier == 2
    assert judged.evidence_text and judged.rationale


def test_analysis_failure_does_not_stop_compliance():
    # The rule that matters most in this file. Tier-1 rules run off the transcript,
    # cost nothing, and are what the customer is actually buying.
    f, sink = build(asr=FakeASR(text=THREATENING), analysis_provider=ExplodingAnalysis())
    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert outcome.analysis is None
    assert "analysis provider failed" in outcome.notes
    assert {x.rule_id for x in outcome.findings} >= {"false_legal_threat"}
    assert outcome.status == "complete"


def test_asr_failure_fails_the_call():
    f, sink = build(asr=ExplodingASR(), analysis_provider=FakeAnalysisProvider())
    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))
    assert outcome.status == "failed"
    assert sink.statuses[-1] == "failed"
    assert sink.transcripts == {}


def test_one_missing_channel_is_survivable():
    # A headset unplugged mid-call, or tier B suppression removing the far side.
    f, sink = build(asr=FakeASR(text=THREATENING),
                    analysis_provider=FakeAnalysisProvider(),
                    segments=FakeSegments(have_far=False))
    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))
    assert outcome.status == "complete"
    assert Channel.FAR not in outcome.transcript.channels
    assert any("borrower channel" in n for n in outcome.notes)


def test_judge_failure_leaves_the_tier_one_finding_standing():
    # Dropping an unreviewed finding because the judge was unavailable would
    # silently reduce compliance coverage, which is the opposite of the product.
    f, sink = build(asr=FakeASR(text=THREATENING),
                    analysis_provider=FakeAnalysisProvider(),
                    judge_provider=ExplodingJudge())
    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))
    kept = next(x for x in outcome.findings if x.rule_id == "false_legal_threat")
    assert kept.tier == 1
    assert outcome.status == "complete"


def test_an_unusable_verdict_leaves_the_finding_and_says_so():
    provider = FakeJudgeProvider(responses=[{
        "verdict": "upheld", "rule_id": "false_legal_threat", "confidence": 0.9,
        "rationale": "The agent asserted a criminal consequence with no lawful basis.",
    }])  # upheld with no evidence: rejected by the schema
    f, sink = build(asr=FakeASR(text=THREATENING),
                    analysis_provider=FakeAnalysisProvider(), judge_provider=provider)
    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))
    assert any("unusable verdict" in n for n in outcome.notes)
    assert next(x for x in outcome.findings if x.rule_id == "false_legal_threat").tier == 1


def test_a_short_call_is_not_analysed_but_is_still_evaluated():
    f, sink = build(asr=FakeASR(text=THREATENING), analysis_provider=FakeAnalysisProvider())
    budget = TenantBudget("t", 10_000_000)
    outcome = f.finalize(ctx(duration_ms=9_000), budget)
    assert outcome.analysis is None
    assert any("shorter than" in n for n in outcome.notes)
    assert budget.spent_paise == 0
    assert {x.rule_id for x in outcome.findings} >= {"false_legal_threat"}


def test_the_kill_switch_leaves_compliance_running():
    f, sink = build(asr=FakeASR(text=THREATENING),
                    analysis_provider=FakeAnalysisProvider(),
                    judge_provider=FakeJudgeProvider())
    budget = TenantBudget("t", None, kill_switch=True)
    outcome = f.finalize(ctx(), budget)
    assert outcome.analysis is None
    assert budget.spent_paise == 0
    assert {x.rule_id for x in outcome.findings} >= {"false_legal_threat"}
    assert all(x.tier == 1 for x in outcome.findings)


def test_the_interruption_count_from_analysis_reaches_the_rules():
    provider = FakeAnalysisProvider()
    payload = provider._derive("Rs 15,000")
    payload["interruptions"] = 40
    provider.responses = [payload]
    f, sink = build(analysis_provider=provider)
    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))
    assert "excessive_interruption" in {x.rule_id for x in outcome.findings}


def test_an_unpriced_model_is_reported_rather_than_recorded_as_free():
    f, sink = build(analysis_provider=FakeAnalysisProvider(),
                    policy=CostPolicy(pricing={}))
    budget = TenantBudget("t", 10_000_000)
    outcome = f.finalize(ctx(), budget)
    assert any("no pricing configured" in n for n in outcome.notes)
    assert budget.spent_paise == 0


def test_findings_reach_the_sink_with_the_rule_set_version():
    f, sink = build(asr=FakeASR(text=THREATENING), analysis_provider=FakeAnalysisProvider())
    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))
    version, findings = sink.findings[outcome.call_id]
    assert version == 1, "a flag must be traceable to the rule text that raised it"
    assert findings == outcome.findings
