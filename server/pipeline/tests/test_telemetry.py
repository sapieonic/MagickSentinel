"""Telemetry: what gets exported, and what must never be.

The privacy tests here are the important ones. Telemetry leaves the trust boundary —
it goes to a collector and usually onward to a vendor — so the attribute allowlists
are a security control, not tidiness. ``tenant_id`` identifies a business; a borrower's
words, an agent's uid and an account reference identify people, and no consent covers
putting those in an observability index.

The other half is that all of it is inert by default: a deployment with no collector
must import nothing and export nothing, because a pipeline that stops transcribing
calls when the collector is down is worse than one with no telemetry at all.
"""

import pytest

from sentinel_pipeline import telemetry


@pytest.fixture(autouse=True)
def _telemetry_off():
    # Every test starts from the disabled state and leaves it that way: these are
    # process-global providers, and a leaked one would silently change other tests.
    telemetry.shutdown()
    telemetry._tracer = None
    telemetry._instruments = None
    yield
    telemetry.shutdown()
    telemetry._tracer = None
    telemetry._instruments = None


# --------------------------------------------------------------------- switched off


def test_telemetry_is_off_with_no_environment_at_all():
    config = telemetry.TelemetryConfig.from_env({})
    assert config.enabled is False
    assert telemetry.configure(config) is False
    assert telemetry.is_enabled() is False


def test_setting_the_standard_endpoint_is_what_turns_it_on():
    config = telemetry.TelemetryConfig.from_env(
        {"OTEL_EXPORTER_OTLP_ENDPOINT": "http://collector:4318"})
    assert config.enabled is True
    assert config.endpoint == "http://collector:4318"
    assert config.service_name == "sentinel-pipeline"


def test_the_standard_kill_switch_is_honoured():
    # An operator turning telemetry off reaches for OTEL_SDK_DISABLED first.
    config = telemetry.TelemetryConfig.from_env({
        "OTEL_EXPORTER_OTLP_ENDPOINT": "http://collector:4318",
        "OTEL_SDK_DISABLED": "true",
    })
    assert config.enabled is False


def test_an_explicit_flag_wins_over_the_presence_of_an_endpoint():
    on = telemetry.TelemetryConfig.from_env({"SENTINEL_OTEL_ENABLED": "1"})
    assert on.enabled is True and on.endpoint is None  # the OTLP default endpoint
    off = telemetry.TelemetryConfig.from_env({
        "SENTINEL_OTEL_ENABLED": "0",
        "OTEL_EXPORTER_OTLP_ENDPOINT": "http://collector:4318",
    })
    assert off.enabled is False


def test_the_service_name_and_protocol_use_the_standard_variables():
    config = telemetry.TelemetryConfig.from_env({
        "OTEL_EXPORTER_OTLP_ENDPOINT": "http://collector:4317",
        "OTEL_EXPORTER_OTLP_PROTOCOL": "grpc",
        "OTEL_SERVICE_NAME": "sentinel-pipeline-canary",
    })
    assert config.protocol == "grpc"
    assert config.service_name == "sentinel-pipeline-canary"


def test_every_call_site_is_inert_when_disabled():
    # worker.py calls these unconditionally, including inside the per-channel ASR
    # loop, so "disabled" has to mean "does nothing" rather than "raises".
    with telemetry.span("finalize", tenant_id="t1", call_id="01J8") as span:
        span.set(status="complete")
        span.degraded("analysis provider failed", RuntimeError("boom"))
    telemetry.record_finalize("complete", 12.5, tenant_id="t1")
    telemetry.record_stage("analysis", 5.0, status="ok", tenant_id="t1")
    telemetry.record_asr("google-transcribe", 900.0, ok=False, tenant_id="t1", channel=0)
    telemetry.record_model_spend(417, model="claude-sonnet-5", tenant_id="t1")
    telemetry.record_judge_review("upheld", tenant_id="t1", rule_id="abusive_language")
    telemetry.record_judge_escalation(2, tenant_id="t1")
    telemetry.record_retention(objects=10, rows=2, table="transcripts", tenant_id="t1")
    telemetry.record_coverage(97.5, tenant_id="t1")
    telemetry.record_dlq("exhausted")


def test_a_telemetry_setup_failure_degrades_to_silence(monkeypatch):
    # A collector that is not there, an SDK that is not installed, an exporter that
    # cannot be built: all operational problems with telemetry, none of them reasons
    # to stop producing compliance records.
    monkeypatch.setattr(telemetry, "_exporters",
                        lambda config: (_ for _ in ()).throw(RuntimeError("no exporter")))
    assert telemetry.configure(telemetry.TelemetryConfig(enabled=True)) is False
    assert telemetry.is_enabled() is False


# ------------------------------------------------------------- attribute policy


FORBIDDEN = {
    "user_uid": "KnA1xyz",
    "account_ref": "LN-88213",
    "transcript": "if you do not pay we will file a police case",
    "evidence_text": "we will file a police case",
    "summary": "The agent threatened the borrower.",
    "borrower_name": "R. Kumar",
    "phone": "+919876543210",
}


@pytest.mark.parametrize("key,value", sorted(FORBIDDEN.items()))
def test_borrower_and_user_data_is_dropped_from_spans(key, value):
    kept = telemetry._filtered({key: value, "tenant_id": "t1"},
                               telemetry.SPAN_ATTRIBUTES, "span")
    assert kept == {"tenant_id": "t1"}


@pytest.mark.parametrize("key,value", sorted(FORBIDDEN.items()))
def test_borrower_and_user_data_is_dropped_from_metrics(key, value):
    kept = telemetry._filtered({key: value, "tenant_id": "t1"},
                               telemetry.METRIC_ATTRIBUTES, "metric")
    assert kept == {"tenant_id": "t1"}


def test_a_call_id_is_a_span_attribute_and_never_a_metric_label():
    # On a span it is the join key from a slow trace to the call it was about. As a
    # metric label it is one time series per call — 60,000 a day on one floor, which
    # breaks the collector long before it tells anyone anything.
    assert "call_id" in telemetry.SPAN_ATTRIBUTES
    assert "call_id" not in telemetry.METRIC_ATTRIBUTES
    assert telemetry._filtered({"call_id": "01J8"}, telemetry.METRIC_ATTRIBUTES,
                               "metric") == {}
    assert telemetry._filtered({"call_id": "01J8"}, telemetry.SPAN_ATTRIBUTES,
                               "span") == {"call_id": "01J8"}


def test_every_permitted_metric_label_is_bounded():
    # A metric label with unbounded values is a collector outage waiting to happen.
    # Anything per-call, per-user or per-borrower must not be on this list.
    assert telemetry.METRIC_ATTRIBUTES <= {
        "tenant_id", "stage", "provider", "model", "rule_id", "status", "verdict",
        "channel", "job", "reason", "table", "dry_run",
    }


def test_the_allowlists_are_the_enforcement_and_not_just_documentation():
    # The point of _filtered is that a well-meaning "let's add the account ref so we
    # can search by it" has to be argued for in telemetry.py rather than merged.
    assert "account_ref" not in telemetry.SPAN_ATTRIBUTES
    assert "evidence_text" not in telemetry.SPAN_ATTRIBUTES
    assert "user_uid" not in telemetry.SPAN_ATTRIBUTES | telemetry.METRIC_ATTRIBUTES


def test_absent_values_are_dropped_rather_than_exported_as_none():
    kept = telemetry._filtered({"tenant_id": "t1", "language": None},
                               telemetry.SPAN_ATTRIBUTES, "span")
    assert kept == {"tenant_id": "t1"}


# ------------------------------------------------- the span tree, with a real SDK


def _finalizer(analysis_explodes: bool):
    """A Finalizer over fakes, wired the way the consumer wires the real one."""
    from sentinel_pipeline.analysis import CallAnalyzer
    from sentinel_pipeline.compliance.engine import RuleEngine, load_default_rule_set
    from sentinel_pipeline.cost import CostPolicy, ModelPricing
    from sentinel_pipeline.models import Channel
    from sentinel_pipeline.providers import FakeAnalysisProvider, FakeASR
    from sentinel_pipeline.worker import Finalizer

    class Segments:
        def channel_audio(self, call_id, channel):
            return b"\x00" * 320 if channel is Channel.NEAR else None

    class Sink:
        def save_transcript(self, *a): pass
        def save_analysis(self, *a): pass
        def save_findings(self, *a): pass
        def set_status(self, *a): pass

    class Exploding:
        name, version = "boom", "1"

        def complete(self, prompt, *, max_output_tokens):
            raise RuntimeError("provider unavailable")

    provider = Exploding() if analysis_explodes else FakeAnalysisProvider()
    return Finalizer(
        asr=FakeASR(text="hello this is a test call"),
        analyzer=CallAnalyzer(provider),
        rules=RuleEngine(load_default_rule_set()),
        judge=None,
        segments=Segments(),
        sink=Sink(),
        cost_policy=CostPolicy(pricing={
            "fake-analysis": ModelPricing("fake-analysis", 1, 1)}),
    )


def _context():
    from datetime import datetime, timezone

    from sentinel_pipeline.models import CallContext

    return CallContext(call_id="01J8ZQ8H2Q7X9K3M4N5P6R7S8T", tenant_id="t1",
                       user_uid="agent-a",
                       started_at=datetime(2026, 9, 1, 5, 30, tzinfo=timezone.utc),
                       duration_ms=300_000, account_ref="LN-1")


def _record_spans(analysis_explodes: bool):
    pytest.importorskip("opentelemetry.sdk")
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import SimpleSpanProcessor
    from opentelemetry.sdk.trace.export.in_memory_span_exporter import (
        InMemorySpanExporter,
    )

    from sentinel_pipeline.cost import TenantBudget

    exporter = InMemorySpanExporter()
    provider = TracerProvider()
    provider.add_span_processor(SimpleSpanProcessor(exporter))
    # A local provider rather than the global one: configure() installs process-wide
    # providers that cannot be replaced, and a test must not decide that for the rest
    # of the suite.
    telemetry._tracer = provider.get_tracer("test")
    try:
        _finalizer(analysis_explodes).finalize(_context(), TenantBudget("t1", None))
    finally:
        telemetry._tracer = None
    return list(exporter.get_finished_spans())


def test_one_finalize_produces_one_span_tree_with_a_child_per_stage():
    spans = _record_spans(analysis_explodes=False)
    names = [s.name for s in spans]
    assert names.count("finalize") == 1
    # Both channels are attempted, so both fetches are visible even though the
    # borrower channel had no audio — which is itself the thing worth seeing.
    assert names.count("segments.fetch") == 2
    assert names.count("asr") == 1
    assert names.count("analysis") == 1

    finalize = next(s for s in spans if s.name == "finalize")
    for child in (s for s in spans if s.name != "finalize"):
        assert child.parent.span_id == finalize.context.span_id


def test_a_failed_analysis_is_an_error_on_its_own_span_and_not_on_the_finalize():
    # The degradation order made visible: compliance completed, the summary did not.
    # Marking the whole finalize failed would hide the calls that genuinely produced
    # nothing; marking nothing at all is how "analysis has been broken for three
    # days" stays invisible.
    spans = _record_spans(analysis_explodes=True)
    # Imported after _record_spans, which skips the test when the SDK is absent.
    from opentelemetry.trace import StatusCode  # noqa: PLC0415

    analysis = next(s for s in spans if s.name == "analysis")
    finalize = next(s for s in spans if s.name == "finalize")

    assert analysis.status.status_code is StatusCode.ERROR
    assert analysis.attributes["degraded"] == "analysis provider failed"
    assert analysis.attributes["error.type"] == "RuntimeError"
    assert finalize.status.status_code is not StatusCode.ERROR
    assert finalize.attributes["status"] == "complete"


def test_no_span_attribute_carries_call_content_or_a_user_uid():
    # The provider's exception message is deliberately not recorded: a provider that
    # echoes its input can put a transcript fragment in it, and record_exception
    # would ship the message and its stack to the collector.
    spans = _record_spans(analysis_explodes=True)
    rendered = " ".join(f"{k}={v}" for s in spans
                        for k, v in (s.attributes or {}).items())
    assert "agent-a" not in rendered
    assert "LN-1" not in rendered
    assert "hello this is a test call" not in rendered
    assert "provider unavailable" not in rendered
    # tenant_id and call_id are the two identifiers that are allowed on a span.
    assert "t1" in rendered and "01J8ZQ8H2Q7X9K3M4N5P6R7S8T" in rendered
