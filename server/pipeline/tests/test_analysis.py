"""CallAnalyzer: schema enforcement, retry, and the things a model gets wrong."""

import copy

import pytest

from conftest import COMPLIANT_OPENING, call, channel
from sentinel_pipeline.analysis import CallAnalyzer, SchemaViolation
from sentinel_pipeline.analysis.analyzer import (
    MIN_ANALYSABLE_MS,
    sentiment_delta,
    should_analyse,
)
from sentinel_pipeline.models import Channel, Disposition
from sentinel_pipeline.providers import FakeAnalysisProvider


@pytest.fixture
def transcript():
    return call(
        near=channel(Channel.NEAR, (0, COMPLIANT_OPENING),
                     (30_000, "Can you pay Rs 15,000 by the fifteenth?")),
        far=channel(Channel.FAR, (40_000, "Yes, I will pay by the fifteenth.")),
    )


def test_a_valid_response_is_accepted_and_attributed(transcript):
    provider = FakeAnalysisProvider()
    analysis = CallAnalyzer(provider).analyse(transcript)
    assert analysis.disposition is Disposition.PTP
    assert analysis.ptp.present
    assert analysis.ptp.amount_paise == 1_500_000, "fifteen thousand rupees is 1500000 paise"
    assert analysis.model == "fake-analysis"
    assert analysis.prompt_version == "analysis-v1"
    assert analysis.input_tokens > 0 and analysis.output_tokens > 0


def test_the_delta_is_recomputed_not_taken_from_the_model(transcript):
    # The fake deliberately returns delta=0.9 alongside far_open=-0.1, far_close=-0.4.
    # Supervisors act on this number, so it is computed server-side from the two
    # endpoints rather than trusted.
    analysis = CallAnalyzer(FakeAnalysisProvider()).analyse(transcript)
    assert analysis.sentiment["delta"] == pytest.approx(-0.3)
    assert sentiment_delta(analysis.sentiment) == pytest.approx(-0.3)


def test_an_invalid_response_is_retried_once_with_the_errors_fed_back(transcript):
    bad = {"summary": "too short", "disposition": "not_a_disposition"}
    good = FakeAnalysisProvider()._derive("Rs 15,000")
    provider = FakeAnalysisProvider(responses=[bad, good])
    analysis = CallAnalyzer(provider).analyse(transcript)
    assert provider.calls == 2
    assert analysis.disposition is Disposition.PTP
    assert "was rejected for these reasons" in provider.prompts[1], \
        "the retry must tell the model what was wrong, or it will repeat the mistake"


def test_two_invalid_responses_fail_the_call_rather_than_persisting_partial_data(transcript):
    bad = {"summary": "x", "disposition": "ptp"}
    provider = FakeAnalysisProvider(responses=[bad, dict(bad)])
    with pytest.raises(SchemaViolation) as exc:
        CallAnalyzer(provider).analyse(transcript)
    assert provider.calls == 2, "exactly one retry, not an unbounded loop"
    assert exc.value.errors


def test_a_ptp_without_an_amount_or_evidence_is_rejected(transcript):
    # The schema requires amount, confidence and a span whenever present is true. A
    # promise with no traceable amount is the failure mode that destroys trust.
    payload = FakeAnalysisProvider()._derive("Rs 15,000")
    payload["ptp"] = {"present": True}
    provider = FakeAnalysisProvider(responses=[payload, copy.deepcopy(payload)])
    with pytest.raises(SchemaViolation):
        CallAnalyzer(provider).analyse(transcript)


def test_a_negative_amount_is_rejected(transcript):
    payload = FakeAnalysisProvider()._derive("Rs 15,000")
    payload["ptp"]["amount_paise"] = -100
    provider = FakeAnalysisProvider(responses=[payload, copy.deepcopy(payload)])
    with pytest.raises(SchemaViolation):
        CallAnalyzer(provider).analyse(transcript)


def test_sentiment_outside_the_valence_range_is_rejected(transcript):
    payload = FakeAnalysisProvider()._derive("Rs 15,000")
    payload["sentiment"]["far"] = [{"t_ms": 0, "v": -4.0}]
    provider = FakeAnalysisProvider(responses=[payload, copy.deepcopy(payload)])
    with pytest.raises(SchemaViolation):
        CallAnalyzer(provider).analyse(transcript)


def test_an_unexpected_extra_field_is_rejected(transcript):
    # additionalProperties is false throughout: a model inventing a field is a signal
    # the prompt and the schema have drifted, and silently dropping it hides that.
    payload = FakeAnalysisProvider()._derive("Rs 15,000")
    payload["agent_score"] = 7
    provider = FakeAnalysisProvider(responses=[payload, copy.deepcopy(payload)])
    with pytest.raises(SchemaViolation):
        CallAnalyzer(provider).analyse(transcript)


def test_short_calls_are_not_analysed():
    ctx = call(near=channel(Channel.NEAR, (0, "hello")), duration_ms=9_000).context
    assert not should_analyse(ctx)
    ctx.duration_ms = MIN_ANALYSABLE_MS
    assert should_analyse(ctx)


def test_the_prompt_attributes_speakers_by_channel_not_by_guesswork(transcript):
    provider = FakeAnalysisProvider()
    CallAnalyzer(provider).analyse(transcript)
    prompt = provider.prompts[0]
    assert "AGENT:" in prompt and "BORROWER:" in prompt
    assert "diariz" not in prompt.lower(), "channels are already separate; never diarize"
    assert "amount_paise is in paise, not rupees" in prompt


def test_a_very_long_call_is_truncated_with_a_marker_not_dropped():
    long_near = channel(Channel.NEAR, *[(i * 1000, "the borrower said something") for i in range(4000)])
    t = call(near=long_near, duration_ms=4_000_000)
    analyzer = CallAnalyzer(FakeAnalysisProvider(), max_transcript_chars=2_000)
    rendered, truncated = analyzer.render_transcript(t)
    assert truncated
    assert rendered.endswith("[transcript truncated]")
    analysis = analyzer.analyse(t)
    assert analysis.truncated, "the record must say the model did not see the whole call"


def test_transcript_rendering_is_time_ordered_across_channels():
    t = call(
        near=channel(Channel.NEAR, (0, "hello"), (20_000, "can you pay")),
        far=channel(Channel.FAR, (10_000, "yes speaking"), (30_000, "next week")),
    )
    rendered, _ = CallAnalyzer(FakeAnalysisProvider()).render_transcript(t)
    positions = [rendered.index(s) for s in ("hello", "yes speaking", "can you pay", "next week")]
    assert positions == sorted(positions), rendered
