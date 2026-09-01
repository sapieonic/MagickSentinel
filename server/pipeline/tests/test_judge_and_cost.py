"""Tier-2 judge and the cost controls."""

import re

import pytest

from conftest import COMPLIANT_OPENING, call, channel
from sentinel_pipeline.compliance.judge import ComplianceJudge, escalate, should_judge
from sentinel_pipeline.cost import (
    BudgetState,
    CostPolicy,
    ModelPricing,
    TenantBudget,
    alerts_for,
)
from sentinel_pipeline.models import Channel, Finding, Severity
from sentinel_pipeline.providers import FakeJudgeProvider


@pytest.fixture
def finding():
    return Finding(
        rule_id="false_legal_threat", severity=Severity.CRITICAL, tier=1,
        span_start_ms=30_000, span_end_ms=34_000,
        evidence_text="we will file a police case",
    )


@pytest.fixture
def transcript():
    return call(
        near=channel(Channel.NEAR, (0, COMPLIANT_OPENING),
                     (30_000, "We will file a police case against you tomorrow.")),
        far=channel(Channel.FAR, (20_000, "I need more time.")),
    )


# ------------------------------------------------------------------- judge


def test_an_upheld_verdict_becomes_a_tier_two_finding(finding, transcript):
    judge = ComplianceJudge(FakeJudgeProvider())
    verdict, _, _ = judge.review(finding, transcript)
    assert verdict.upheld
    applied = judge.apply(finding, verdict)
    assert applied.tier == 2
    assert applied.confidence == pytest.approx(0.9)
    assert applied.rationale
    assert applied.evidence_text == "we will file a police case"


def test_an_upheld_verdict_with_no_evidence_is_discarded(finding, transcript):
    # The schema requires a span and verbatim text whenever a violation is upheld. A
    # flag a reviewer cannot trace to specific words is not usable with a bank, so an
    # untraceable verdict must never become a flag on someone's record.
    provider = FakeJudgeProvider(responses=[{
        "verdict": "upheld", "rule_id": "false_legal_threat", "confidence": 0.95,
        "rationale": "The agent clearly threatened legal action without any basis.",
    }])
    verdict, _, _ = ComplianceJudge(provider).review(finding, transcript)
    assert verdict is None


def test_a_verdict_about_a_different_rule_is_discarded(finding, transcript):
    provider = FakeJudgeProvider(responses=[{
        "verdict": "upheld", "rule_id": "abusive_language", "confidence": 0.9,
        "rationale": "The agent used demeaning language throughout the exchange.",
        "evidence_span_ms": [1, 2], "evidence_text": "shameless",
    }])
    verdict, _, _ = ComplianceJudge(provider).review(finding, transcript)
    assert verdict is None, "a verdict about another rule cannot be attached to this finding"


def test_malformed_json_from_the_model_is_discarded(finding, transcript):
    provider = FakeJudgeProvider(responses=[{"verdict": "maybe", "rule_id": "x", "confidence": 5}])
    verdict, _, _ = ComplianceJudge(provider).review(finding, transcript)
    assert verdict is None


def test_not_a_violation_removes_the_finding(finding, transcript):
    provider = FakeJudgeProvider(responses=[{
        "verdict": "not_a_violation", "rule_id": "false_legal_threat", "confidence": 0.8,
        "rationale": "The agent described a lawful civil recovery process accurately "
                     "and did not claim any criminal consequence.",
    }])
    judge = ComplianceJudge(provider)
    verdict, _, _ = judge.review(finding, transcript)
    assert judge.apply(finding, verdict) is None


def test_inconclusive_leaves_the_tier_one_finding_for_a_human(finding, transcript):
    provider = FakeJudgeProvider(responses=[{
        "verdict": "inconclusive", "rule_id": "false_legal_threat", "confidence": 0.4,
        "rationale": "The audio quality makes the decisive sentence hard to attribute "
                     "with confidence either way.",
    }])
    judge = ComplianceJudge(provider)
    verdict, _, _ = judge.review(finding, transcript)
    kept = judge.apply(finding, verdict)
    assert kept == finding


def test_the_prompt_tells_the_judge_a_rude_borrower_is_not_a_violation(finding, transcript):
    provider = FakeJudgeProvider()
    ComplianceJudge(provider).review(finding, transcript)
    prompt = provider.prompts[0]
    assert "A borrower being rude, angry or abusive is never a violation by the agent" in prompt
    assert "AGENT:" in prompt and "BORROWER:" in prompt


def test_the_prompt_is_windowed_around_the_span():
    # Sending twenty minutes of transcript to judge one sentence costs tokens without
    # improving the verdict.
    long_near = channel(Channel.NEAR, *[(i * 1000, f"sentence {i}") for i in range(1200)])
    t = call(near=long_near, duration_ms=1_200_000)
    judge = ComplianceJudge(FakeJudgeProvider())
    windowed = judge.render_transcript(t, around_ms=600_000)
    whole = judge.render_transcript(t, around_ms=None)
    assert len(windowed) < len(whole)
    assert "sentence 600" in windowed
    # Word-boundary match: "sentence 5" is a prefix of "sentence 510".
    assert not re.search(r"\bsentence 5\b", windowed), "the window reaches back too far"
    assert re.search(r"\bsentence 5\b", whole)


def test_escalation_only_covers_rules_the_tenant_marked_for_judging():
    findings = [
        Finding(rule_id="false_legal_threat", severity=Severity.CRITICAL, tier=1),
        Finding(rule_id="outside_call_hours", severity=Severity.HIGH, tier=1),
    ]
    escalated = escalate(findings, {"false_legal_threat"})
    assert [f.rule_id for f in escalated] == ["false_legal_threat"]


def test_sampling_is_deterministic_in_the_call_id():
    # A pipeline whose cost and output change when you replay it is impossible to
    # reason about, and re-runs happen constantly during an ASR evaluation.
    args = dict(tier1_hit=False, judge_rules=set(), rule_ids=set(), sample_pct=50.0)
    first = [should_judge(f"call-{i}", **args) for i in range(200)]
    second = [should_judge(f"call-{i}", **args) for i in range(200)]
    assert first == second
    rate = sum(first) / len(first)
    assert 0.35 < rate < 0.65, f"50% sampling produced {rate:.0%}"


def test_a_flagged_call_is_always_judged_regardless_of_the_sample():
    assert should_judge("any-call", tier1_hit=True, judge_rules={"false_legal_threat"},
                        rule_ids={"false_legal_threat"}, sample_pct=0.0)


def test_a_flag_on_a_rule_the_tenant_does_not_judge_is_not_escalated():
    assert not should_judge("any-call", tier1_hit=True, judge_rules={"abusive_language"},
                            rule_ids={"outside_call_hours"}, sample_pct=0.0)


# -------------------------------------------------------------------- cost


PRICING = {
    "test-model": ModelPricing("test-model", input_paise_per_mtok=25_000,
                               output_paise_per_mtok=125_000),
}


def test_cost_is_integer_paise_and_rounds_up():
    p = PRICING["test-model"]
    # 1000 input tokens is 25 paise; a fraction must round up, because under-reporting
    # spend compounds across 60,000 minutes a day.
    assert p.cost_paise(1_000_000, 0) == 25_000
    assert p.cost_paise(1, 0) == 1
    assert p.cost_paise(0, 0) == 0
    assert isinstance(p.cost_paise(1234, 567), int)


def test_budget_states_step_through_the_thresholds():
    b = TenantBudget("t", monthly_budget_paise=1_000_000)
    assert b.state is BudgetState.OK
    b.record(700_000)
    assert b.state is BudgetState.WARN_70
    b.record(200_000)
    assert b.state is BudgetState.WARN_90
    b.record(100_000)
    assert b.state is BudgetState.EXHAUSTED
    assert b.remaining_paise == 0


def test_a_tenant_with_no_budget_is_never_throttled():
    b = TenantBudget("t", monthly_budget_paise=None)
    b.record(10**12)
    assert b.state is BudgetState.OK
    assert b.remaining_paise is None


def test_short_calls_are_skipped_before_anything_else():
    policy = CostPolicy(pricing=PRICING)
    d = policy.decide(TenantBudget("t", None), duration_ms=9_000, tier1_hit=False)
    assert not d.analyse and not d.judge
    assert "shorter than" in d.reason


def test_the_kill_switch_drops_to_tier_one_rules_only():
    policy = CostPolicy(pricing=PRICING)
    b = TenantBudget("t", monthly_budget_paise=None, kill_switch=True)
    d = policy.decide(b, duration_ms=300_000, tier1_hit=True)
    assert not d.analyse and not d.judge
    assert "kill switch" in d.reason


def test_an_exhausted_budget_stops_models_but_not_compliance():
    policy = CostPolicy(pricing=PRICING)
    b = TenantBudget("t", monthly_budget_paise=100)
    b.record(200)
    d = policy.decide(b, duration_ms=300_000, tier1_hit=True)
    assert not d.analyse and not d.judge
    # Tier-1 rules are not gated by cost anywhere: they are free, and they are the
    # thing the bank is being shown.
    assert "tier-1" in d.reason


def test_at_ninety_percent_sampled_judging_stops_before_flagged_judging():
    policy = CostPolicy(pricing=PRICING)
    b = TenantBudget("t", monthly_budget_paise=1_000)
    b.record(920)
    unflagged = policy.decide(b, duration_ms=300_000, tier1_hit=False)
    flagged = policy.decide(b, duration_ms=300_000, tier1_hit=True)
    assert unflagged.analyse and not unflagged.judge
    assert flagged.judge, "a call tier 1 flagged still gets reviewed near the budget cap"


def test_an_unpriced_model_raises_rather_than_recording_zero():
    policy = CostPolicy(pricing=PRICING)
    with pytest.raises(KeyError):
        policy.cost_paise("some-new-model", 1000, 100)


def test_budget_alerts_are_edge_triggered():
    assert alerts_for(BudgetState.OK, BudgetState.WARN_70)
    assert alerts_for(BudgetState.WARN_70, BudgetState.WARN_90)
    # A tenant sitting at 91% for three weeks generates one alert, not one per call.
    assert alerts_for(BudgetState.WARN_90, BudgetState.WARN_90) == []
    # Nor does spend going back down re-alert.
    assert alerts_for(BudgetState.EXHAUSTED, BudgetState.WARN_70) == []
