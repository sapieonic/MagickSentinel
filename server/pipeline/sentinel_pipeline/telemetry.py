"""OpenTelemetry traces and metrics for the pipeline.

Off unless a collector is configured. A deployment with no OTLP endpoint — every
development machine, and the first customer's floor if the collector is late —
imports nothing, exports nothing and pays nothing: the call sites below resolve to
no-op objects. Telemetry that can take the pipeline down is worse than no telemetry,
so a missing collector, a missing SDK and a broken exporter all degrade to silence
with one log line rather than to a failed finalize.

**One span per finalize, with a child span per stage.** That tree is the whole point:
``worker.py``'s degradation order is deliberate — ASR failure stops the call, an
analysis failure must *not* stop compliance, a judge failure leaves tier-1 findings
standing unreviewed — and from the outside all three look like "the call completed".
The child spans record those failures as span status ``ERROR`` on the stage that
failed while the parent finalize span still ends ``OK``, which is the only honest
rendering: compliance did complete, and the summary did not.

## What may be an attribute, and what may never be

This is a compliance product operating on borrower data, and telemetry leaves the
trust boundary — it goes to a collector, then usually to a vendor.

* ``tenant_id`` is fine everywhere. It is the dimension every operational question is
  asked along, and it identifies a business, not a person.
* ``call_id`` is allowed on **spans** and refused on **metrics**. A span is one
  record of one event, and the call id is the join key an operator needs to get from
  a slow trace to the call it was about. A metric label is a time series per distinct
  value: 60,000 calls a day would create 60,000 series, which breaks the collector
  long before it tells anyone anything.
* ``user_uid`` and anything borrower-related — ``account_ref``, transcript text,
  evidence spans, the summary, a phone number, a name — are refused everywhere.
  Transcript text in a span attribute is the borrower's words in a third-party
  observability vendor's index, and no consent covers that.

The rule is enforced rather than documented: :func:`_filtered` drops any attribute
whose key is not on the allowlist for that signal, and the allowlists below are
deliberately short. Adding a key is a decision someone has to make on purpose, which
is the point — this is the file where a well-meaning "let's add the account ref so we
can search by it" has to be argued for rather than merged.
"""

from __future__ import annotations

import logging
import os
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Iterator

log = logging.getLogger(__name__)

SERVICE_NAME = "sentinel-pipeline"

#: Keys permitted on spans. ``call_id`` is here and not in the metric list; see the
#: module docstring for why the two differ.
SPAN_ATTRIBUTES = frozenset({
    "tenant_id", "call_id", "channel", "stage", "provider", "model", "rule_id",
    "status", "degraded", "reason", "job", "day", "attempt", "subject", "dry_run",
    "segments", "findings", "language",
})

#: Keys permitted on metrics. Every one of these is bounded: a handful of stages, a
#: handful of providers and models, two channels, ten rule ids, one tenant per
#: customer. Nothing per-call, ever.
METRIC_ATTRIBUTES = frozenset({
    "tenant_id", "stage", "provider", "model", "rule_id", "status", "verdict",
    "channel", "job", "reason", "dry_run",
})

_warned_keys: set[str] = set()


def _filtered(attributes: dict[str, object], allowed: frozenset[str],
              signal: str) -> dict[str, object]:
    """Drop attributes that are not on the allowlist for this signal.

    Silent dropping would hide a typo, so the first sighting of an unknown key is
    logged — the key, never the value, because the value is exactly the thing this
    function exists to keep out of the exporter.
    """
    out: dict[str, object] = {}
    for key, value in attributes.items():
        if value is None:
            continue
        if key not in allowed:
            if key not in _warned_keys:
                _warned_keys.add(key)
                log.warning("telemetry attribute refused",
                            extra={"attribute": key, "signal": signal})
            continue
        out[key] = value
    return out


# --------------------------------------------------------------------------- config


@dataclass(frozen=True)
class TelemetryConfig:
    """How telemetry is turned on, using the standard OTel variables where they exist.

    ``OTEL_EXPORTER_OTLP_ENDPOINT``  collector endpoint; setting it enables export.
    ``OTEL_EXPORTER_OTLP_PROTOCOL``  ``http/protobuf`` (default) or ``grpc``.
    ``SENTINEL_OTEL_ENABLED``        turn on against the OTLP default endpoint, or
                                     force off (``0``) despite an endpoint being set.
    ``OTEL_SERVICE_NAME``            defaults to ``sentinel-pipeline``.
    ``OTEL_SDK_DISABLED``            honoured, because it is the standard kill switch
                                     an operator will reach for first.
    """

    enabled: bool = False
    endpoint: str | None = None
    protocol: str = "http/protobuf"
    service_name: str = SERVICE_NAME
    #: Metric export interval. 60 s rather than the SDK's default so a 5-minute
    #: incident is still four or five points.
    export_interval_ms: int = 60_000

    @staticmethod
    def from_env(env: dict[str, str] | None = None) -> "TelemetryConfig":
        env = dict(os.environ if env is None else env)
        endpoint = (env.get("OTEL_EXPORTER_OTLP_ENDPOINT") or "").strip() or None
        explicit = env.get("SENTINEL_OTEL_ENABLED")
        disabled = (env.get("OTEL_SDK_DISABLED") or "").lower() in {"1", "true", "yes"}

        if explicit is not None:
            enabled = explicit.lower() in {"1", "true", "yes", "on"}
        else:
            # No explicit answer: the presence of an endpoint is the answer. This is
            # what keeps "off by default" true without a second variable to remember.
            enabled = endpoint is not None
        return TelemetryConfig(
            enabled=enabled and not disabled,
            endpoint=endpoint,
            protocol=(env.get("OTEL_EXPORTER_OTLP_PROTOCOL") or "http/protobuf").strip(),
            service_name=(env.get("OTEL_SERVICE_NAME") or SERVICE_NAME).strip(),
            export_interval_ms=int(env.get("SENTINEL_OTEL_EXPORT_INTERVAL_MS", "60000")),
        )


# ------------------------------------------------------------------------- runtime

_tracer: object | None = None
_instruments: "_Instruments | None" = None
_providers: tuple[object, object] | None = None


class _Instruments:
    """The metric handles, created once.

    Named ``sentinel.*`` and left as counters and histograms rather than
    pre-computed rates: a rate belongs in the query, not in the process. Judge
    escalation rate, for instance, is ``escalations / reviews`` computed by whatever
    is reading the metrics, so a partial scrape or a restarted worker cannot produce
    a rate above 1.
    """

    def __init__(self, meter: object) -> None:
        self.finalize_duration = meter.create_histogram(
            "sentinel.finalize.duration", unit="ms",
            description="Wall time of one finalize stage, by stage and outcome.")
        self.finalize_calls = meter.create_counter(
            "sentinel.finalize.calls", unit="{call}",
            description="Finalized calls, by terminal status.")
        self.asr_duration = meter.create_histogram(
            "sentinel.asr.duration", unit="ms",
            description="ASR provider latency for one channel of one call.")
        self.asr_failures = meter.create_counter(
            "sentinel.asr.failures", unit="{failure}",
            description="ASR calls that raised, by provider.")
        self.model_spend = meter.create_counter(
            "sentinel.model.spend", unit="paise",
            description="Model spend per tenant and model, in integer paise.")
        self.judge_reviews = meter.create_counter(
            "sentinel.judge.reviews", unit="{review}",
            description="Tier-2 reviews performed, by verdict.")
        self.judge_escalations = meter.create_counter(
            "sentinel.judge.escalations", unit="{finding}",
            description="Findings escalated to the tier-2 judge.")
        self.retention_objects = meter.create_counter(
            "sentinel.retention.objects_deleted", unit="{object}",
            description="Audio objects deleted by the retention purge.")
        self.retention_rows = meter.create_counter(
            "sentinel.retention.rows_deleted", unit="{row}",
            description="Rows deleted by the retention purge, by table.")
        self.coverage_pct = meter.create_histogram(
            "sentinel.coverage.pct", unit="%",
            description="Daily capture coverage against the dialer CDR, per tenant.")
        self.dlq = meter.create_counter(
            "sentinel.consumer.dlq", unit="{message}",
            description="Finalize messages sent to the dead-letter subject, by reason.")


def configure(config: TelemetryConfig | None = None) -> bool:
    """Set up traces and metrics. Returns whether export is on.

    Idempotent, and safe to call from a job entrypoint that may run in the same
    process as another. Every failure path here ends in ``False`` and a log line:
    the SDK not being installed, the exporter package not being installed, or the
    endpoint being unreachable are all operational problems with telemetry, not
    reasons to stop transcribing calls.
    """
    global _tracer, _instruments, _providers

    config = config or TelemetryConfig.from_env()
    if not config.enabled:
        log.debug("telemetry disabled")
        return False
    if _providers is not None:
        return True

    try:
        # Imported here, never at module scope: this is the only place that needs
        # the SDK, and the unit tests must run without it installed.
        from opentelemetry import metrics, trace  # noqa: PLC0415
        from opentelemetry.sdk.metrics import MeterProvider  # noqa: PLC0415
        from opentelemetry.sdk.metrics.export import (  # noqa: PLC0415
            PeriodicExportingMetricReader,
        )
        from opentelemetry.sdk.resources import Resource  # noqa: PLC0415
        from opentelemetry.sdk.trace import TracerProvider  # noqa: PLC0415
        from opentelemetry.sdk.trace.export import BatchSpanProcessor  # noqa: PLC0415

        span_exporter, metric_exporter = _exporters(config)
    except Exception as exc:  # noqa: BLE001 - any import or construction failure
        log.error("telemetry unavailable; continuing without it",
                  extra={"error_type": type(exc).__name__})
        return False

    resource = Resource.create({
        "service.name": config.service_name,
        "service.version": _version(),
    })
    tracer_provider = TracerProvider(resource=resource)
    tracer_provider.add_span_processor(BatchSpanProcessor(span_exporter))
    trace.set_tracer_provider(tracer_provider)

    reader = PeriodicExportingMetricReader(
        metric_exporter, export_interval_millis=config.export_interval_ms
    )
    meter_provider = MeterProvider(resource=resource, metric_readers=[reader])
    metrics.set_meter_provider(meter_provider)

    _tracer = trace.get_tracer(config.service_name)
    _instruments = _Instruments(metrics.get_meter(config.service_name))
    _providers = (tracer_provider, meter_provider)
    log.info("telemetry enabled", extra={"endpoint": config.endpoint or "otlp-default",
                                         "protocol": config.protocol})
    return True


def _exporters(config: TelemetryConfig) -> tuple[object, object]:
    kwargs = {"endpoint": config.endpoint} if config.endpoint else {}
    if config.protocol == "grpc":
        from opentelemetry.exporter.otlp.proto.grpc.metric_exporter import (  # noqa: PLC0415
            OTLPMetricExporter,
        )
        from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import (  # noqa: PLC0415
            OTLPSpanExporter,
        )
    else:
        from opentelemetry.exporter.otlp.proto.http.metric_exporter import (  # noqa: PLC0415
            OTLPMetricExporter,
        )
        from opentelemetry.exporter.otlp.proto.http.trace_exporter import (  # noqa: PLC0415
            OTLPSpanExporter,
        )
        # The HTTP exporters take a full signal URL rather than a base endpoint, and
        # the SDK appends the path itself when the endpoint comes from the standard
        # variable — so leave it to the SDK and pass nothing when unset.
    return OTLPSpanExporter(**kwargs), OTLPMetricExporter(**kwargs)


def _version() -> str:
    from . import __version__  # noqa: PLC0415 - avoids a circular import at module load

    return __version__


def shutdown() -> None:
    """Flush and stop. Called from the entrypoints so a short-lived job exports."""
    global _tracer, _instruments, _providers
    if _providers is None:
        return
    tracer_provider, meter_provider = _providers
    for provider in (tracer_provider, meter_provider):
        try:
            provider.shutdown()
        except Exception as exc:  # noqa: BLE001
            log.warning("telemetry shutdown failed",
                        extra={"error_type": type(exc).__name__})
    _tracer, _instruments, _providers = None, None, None


def is_enabled() -> bool:
    return _providers is not None


# ---------------------------------------------------------------------------- spans


class Span:
    """A thin wrapper over an OTel span, or over nothing at all.

    Exists so ``worker.py`` reads the same whether telemetry is on or off and never
    imports OpenTelemetry. ``_span`` is ``None`` in the disabled case and every
    method becomes a no-op.
    """

    __slots__ = ("_span",)

    def __init__(self, span: object | None) -> None:
        self._span = span

    def set(self, **attributes: object) -> None:
        if self._span is None:
            return
        for key, value in _filtered(attributes, SPAN_ATTRIBUTES, "span").items():
            self._span.set_attribute(key, value)

    def degraded(self, reason: str, exc: BaseException | None = None) -> None:
        """Mark this stage failed while letting the caller carry on.

        This is how ``worker.py``'s degradation order becomes visible instead of
        invisible: the stage span goes ``ERROR`` with a reason, and the finalize span
        above it still succeeds, because compliance did in fact complete.

        The exception *type* is recorded and the message is not. A provider that
        echoes its input can put a transcript fragment in an exception message, and
        ``record_exception`` would ship that message and its stack to the collector.
        """
        if self._span is None:
            return
        from opentelemetry.trace import Status, StatusCode  # noqa: PLC0415

        self._span.set_status(Status(StatusCode.ERROR, reason))
        self._span.set_attribute("degraded", reason)
        if exc is not None:
            self._span.set_attribute("error.type", type(exc).__name__)


_NULL_SPAN = Span(None)


@contextmanager
def span(name: str, **attributes: object) -> Iterator[Span]:
    """Start a span, or nothing.

    The no-op path allocates nothing beyond the shared ``_NULL_SPAN``, which is why
    call sites can be unconditional even in the per-channel ASR loop.
    """
    if _tracer is None:
        yield _NULL_SPAN
        return
    safe = _filtered(attributes, SPAN_ATTRIBUTES, "span")
    with _tracer.start_as_current_span(name, attributes=safe) as raw:
        yield Span(raw)


# -------------------------------------------------------------------------- metrics


def record_stage(stage: str, duration_ms: float, *, status: str, **attributes) -> None:
    if _instruments is None:
        return
    attrs = _filtered({"stage": stage, "status": status, **attributes},
                      METRIC_ATTRIBUTES, "metric")
    _instruments.finalize_duration.record(duration_ms, attrs)


def record_finalize(status: str, duration_ms: float, **attributes) -> None:
    if _instruments is None:
        return
    attrs = _filtered({"status": status, **attributes}, METRIC_ATTRIBUTES, "metric")
    _instruments.finalize_duration.record(duration_ms, {**attrs, "stage": "finalize"})
    _instruments.finalize_calls.add(1, attrs)


def record_asr(provider: str, duration_ms: float, *, ok: bool, **attributes) -> None:
    if _instruments is None:
        return
    attrs = _filtered({"provider": provider, **attributes}, METRIC_ATTRIBUTES, "metric")
    _instruments.asr_duration.record(duration_ms, {**attrs, "status": "ok" if ok else "error"})
    if not ok:
        _instruments.asr_failures.add(1, attrs)


def record_model_spend(paise: int, *, model: str, **attributes) -> None:
    """Per-tenant model spend, in integer paise.

    ``cost.py`` already computes this per call; without it here, the only place spend
    is visible is the provider's invoice at the end of the month, which is a month too
    late for a budget with alerts at 70% and 90%. Recorded as an integer count of
    paise for the same reason money is integer paise everywhere else: a float rupee
    value that has been through a histogram bucket is not an accounting record.
    """
    if _instruments is None or paise <= 0:
        return
    attrs = _filtered({"model": model, **attributes}, METRIC_ATTRIBUTES, "metric")
    _instruments.model_spend.add(int(paise), attrs)


def record_judge_review(verdict: str, **attributes) -> None:
    if _instruments is None:
        return
    attrs = _filtered({"verdict": verdict, **attributes}, METRIC_ATTRIBUTES, "metric")
    _instruments.judge_reviews.add(1, attrs)


def record_judge_escalation(count: int, **attributes) -> None:
    if _instruments is None or count <= 0:
        return
    attrs = _filtered(dict(attributes), METRIC_ATTRIBUTES, "metric")
    _instruments.judge_escalations.add(count, attrs)


def record_retention(*, objects: int, rows: int, table: str, **attributes) -> None:
    if _instruments is None:
        return
    attrs = _filtered(dict(attributes), METRIC_ATTRIBUTES, "metric")
    if objects:
        _instruments.retention_objects.add(objects, attrs)
    if rows:
        _instruments.retention_rows.add(rows, {**attrs, "status": table})


def record_coverage(pct: float, **attributes) -> None:
    if _instruments is None:
        return
    attrs = _filtered(dict(attributes), METRIC_ATTRIBUTES, "metric")
    _instruments.coverage_pct.record(pct, attrs)


def record_dlq(reason: str, **attributes) -> None:
    """A message that could not be finalized after every retry.

    The one metric an operator should alert on unconditionally: everything else here
    describes how the pipeline is behaving, and this one says a call has no
    compliance record and no further attempt is coming.
    """
    if _instruments is None:
        return
    attrs = _filtered({"reason": reason, **attributes}, METRIC_ATTRIBUTES, "metric")
    _instruments.dlq.add(1, attrs)
