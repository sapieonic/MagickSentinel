"""The finalize pipeline: ASR → analysis → compliance, for one call.

Written as a pure orchestration function over injected interfaces rather than a
long-running consumer, so the whole sequence — including the order in which failures
degrade — is testable without NATS, Postgres or a model provider.
:mod:`sentinel_pipeline.consumer` wraps it in the JetStream loop.

The degradation order matters and is the reason this is one function rather than
three independent subscribers:

* ASR failing means there is nothing to analyse or judge; the call is marked failed.
* Analysis failing must **not** stop compliance. Tier-1 rules run off the transcript
  and are the thing the bank is being shown; losing a summary is an inconvenience,
  losing compliance coverage is a breach of what was sold.
* The judge failing leaves the tier-1 findings standing, unreviewed.

That order is also what the trace tree records. One span per finalize with a child
span per stage, and a stage that degraded is marked ``ERROR`` on its own span while
the finalize span above it still ends ``OK`` — because from the product's point of
view the call *did* complete: it has compliance findings, it just has no summary.
Swallowing that distinction is how "analysis has been broken for three days" becomes
invisible. Concrete implementations of the two interfaces below live in
:mod:`sentinel_pipeline.persistence`; :mod:`sentinel_pipeline.telemetry` is a no-op
unless a collector is configured, so nothing here depends on one.
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from typing import Protocol

from . import telemetry
from .analysis import CallAnalyzer, SchemaViolation
from .compliance.engine import RuleEngine
from .compliance.judge import ComplianceJudge, escalate, should_judge
from .cost import CostPolicy, TenantBudget
from .models import Analysis, CallContext, Channel, Finding, Transcript

log = logging.getLogger(__name__)


def _elapsed_ms(started: float) -> float:
    return (time.perf_counter() - started) * 1000.0


class SegmentSource(Protocol):
    """Fetches a call's stored audio, one channel at a time.

    Segments marked ``foreign`` — tier B loopback captured while the softphone
    session was inactive — are stored but MUST NOT be transcribed. Filtering happens
    in the implementation of this interface so there is exactly one place to get it
    wrong.
    """

    def channel_audio(self, call_id: str, channel: Channel) -> bytes | None:
        ...


class Sink(Protocol):
    """Persists the results."""

    def save_transcript(self, call_id: str, transcript: Transcript) -> None: ...
    def save_analysis(self, call_id: str, analysis: Analysis, cost_paise: int) -> None: ...
    def save_findings(self, call_id: str, rule_set_version: int, findings: list[Finding]) -> None: ...
    def set_status(self, call_id: str, status: str) -> None: ...


@dataclass
class Outcome:
    call_id: str
    status: str
    transcript: Transcript | None = None
    analysis: Analysis | None = None
    findings: list[Finding] = field(default_factory=list)
    cost_paise: int = 0
    notes: list[str] = field(default_factory=list)


@dataclass
class Finalizer:
    asr: object                 # BatchASR
    analyzer: CallAnalyzer | None
    rules: RuleEngine
    judge: ComplianceJudge | None
    segments: SegmentSource
    sink: Sink
    cost_policy: CostPolicy

    def finalize(self, ctx: CallContext, budget: TenantBudget) -> Outcome:
        """Finalize one call, under one span.

        The span wrapper is separate from the body so the orchestration below stays
        exactly as readable as it was before there was any telemetry: nothing in
        ``_finalize`` branches on whether a collector exists.
        """
        started = time.perf_counter()
        with telemetry.span("finalize", tenant_id=ctx.tenant_id, call_id=ctx.call_id,
                            language=ctx.language) as root:
            outcome = self._finalize(ctx, budget, root)
            root.set(status=outcome.status, findings=len(outcome.findings))
        telemetry.record_finalize(outcome.status, _elapsed_ms(started),
                                  tenant_id=ctx.tenant_id)
        return outcome

    def _finalize(self, ctx: CallContext, budget: TenantBudget,
                  root: telemetry.Span) -> Outcome:
        outcome = Outcome(call_id=ctx.call_id, status="transcribing")

        transcript = self._transcribe(ctx, outcome)
        if transcript is None:
            self.sink.set_status(ctx.call_id, "failed")
            outcome.status = "failed"
            # The only failure that stops the call, and therefore the only one that
            # makes the finalize span itself an error.
            root.degraded("no audio could be transcribed")
            return outcome
        outcome.transcript = transcript
        self.sink.save_transcript(ctx.call_id, transcript)

        decision = self.cost_policy.decide(
            budget, ctx.duration_ms, tier1_hit=False
        )
        self.sink.set_status(ctx.call_id, "analyzing")

        if decision.analyse and self.analyzer is not None:
            outcome.analysis = self._analyse(transcript, outcome, budget)
            if outcome.analysis is not None:
                # The rules need the interruption count, which only the analysis
                # produces, so the context is enriched before tier 1 runs.
                ctx.interruptions = outcome.analysis.interruptions
        else:
            outcome.notes.append(f"analysis skipped: {decision.reason}")

        findings = self.rules.evaluate(transcript)
        outcome.findings = findings

        judge_rules = self.rules.rule_set.judge_rules()
        wants_judge = should_judge(
            ctx.call_id,
            tier1_hit=bool(findings),
            judge_rules=judge_rules,
            rule_ids={f.rule_id for f in findings},
            sample_pct=self.rules.rule_set.judge_sample_pct,
        )
        # Re-decide with the tier-1 result in hand: past 90% of budget the flagged
        # calls still get judged even though the random sample does not.
        judge_decision = self.cost_policy.decide(budget, ctx.duration_ms, tier1_hit=bool(findings))
        if wants_judge and judge_decision.judge and self.judge is not None:
            outcome.findings = self._review(findings, transcript, judge_rules, outcome, budget)
        elif wants_judge:
            outcome.notes.append(f"judge skipped: {judge_decision.reason}")

        self.sink.save_findings(ctx.call_id, self.rules.rule_set.version, outcome.findings)
        self.sink.set_status(ctx.call_id, "complete")
        outcome.status = "complete"
        outcome.cost_paise = budget.spent_paise
        return outcome

    # ------------------------------------------------------------------ steps

    def _transcribe(self, ctx: CallContext, outcome: Outcome) -> Transcript | None:
        transcript = Transcript(context=ctx)
        provider = str(getattr(self.asr, "name", "unknown"))
        for channel in (Channel.FAR, Channel.NEAR):
            # A separate span from the transcription itself: object-storage latency
            # and provider latency are different problems with different owners, and
            # a finalize that is slow because S3 is slow looks identical to one that
            # is slow because the model is, until you can see the two apart.
            with telemetry.span("segments.fetch", tenant_id=ctx.tenant_id,
                                call_id=ctx.call_id, channel=int(channel)) as fetch_span:
                fetch_started = time.perf_counter()
                audio = self.segments.channel_audio(ctx.call_id, channel)
                fetch_span.set(segments=len(audio) if audio else 0)
                telemetry.record_stage("segments.fetch", _elapsed_ms(fetch_started),
                                       status="ok" if audio else "empty",
                                       tenant_id=ctx.tenant_id, channel=int(channel))
            if not audio:
                # One channel missing is survivable and happens: a headset unplugged
                # mid-call, or tier B suppression removing everything on the far
                # side. Both channels missing is not.
                outcome.notes.append(f"no audio on the {channel.speaker} channel")
                continue
            with telemetry.span("asr", tenant_id=ctx.tenant_id, call_id=ctx.call_id,
                                channel=int(channel), provider=provider,
                                language=ctx.language) as asr_span:
                started = time.perf_counter()
                try:
                    # The hint is what routes the call. On a floor whose language the
                    # default provider cannot read, dropping it here would send the audio
                    # to that provider anyway and produce a clean-looking transcript with
                    # no flags on it — the exact failure registry.validate refuses at
                    # startup, reintroduced at run time.
                    result = self.asr.transcribe(audio, sample_rate=16_000,
                                                 language_hint=ctx.language)
                except Exception as exc:  # noqa: BLE001 - provider errors are opaque
                    # No call content in the log line: the exception message could
                    # contain a transcript fragment from a provider that echoes input.
                    # For the same reason the span records the exception *type* and
                    # not the message — see telemetry.Span.degraded.
                    log.error("asr failed", extra={"call_id": ctx.call_id,
                                                   "channel": int(channel),
                                                   "error_type": type(exc).__name__})
                    asr_span.degraded("asr provider failed", exc)
                    telemetry.record_asr(provider, _elapsed_ms(started), ok=False,
                                         tenant_id=ctx.tenant_id, channel=int(channel))
                    outcome.notes.append(f"asr failed on the {channel.speaker} channel")
                    continue
                telemetry.record_asr(provider, _elapsed_ms(started), ok=True,
                                     tenant_id=ctx.tenant_id, channel=int(channel))
                # The provider that actually ran, which is not necessarily the one
                # this loop asked: a routed floor hands Tamil to a different adapter.
                asr_span.set(provider=result.provider)
            transcript.channels[channel] = result.to_channel_transcript(channel)
        if not transcript.channels:
            return None
        return transcript

    def _analyse(self, transcript: Transcript, outcome: Outcome,
                 budget: TenantBudget) -> Analysis | None:
        ctx = transcript.context
        provider = str(getattr(getattr(self.analyzer, "provider", None), "name", "unknown"))
        with telemetry.span("analysis", tenant_id=ctx.tenant_id, call_id=ctx.call_id,
                            provider=provider) as span_:
            started = time.perf_counter()
            analysis = self._analyse_once(transcript, outcome, budget, span_)
            telemetry.record_stage("analysis", _elapsed_ms(started),
                                   status="ok" if analysis is not None else "degraded",
                                   tenant_id=ctx.tenant_id)
            if analysis is not None:
                span_.set(model=analysis.model)
            return analysis

    def _analyse_once(self, transcript: Transcript, outcome: Outcome,
                      budget: TenantBudget, span_: telemetry.Span) -> Analysis | None:
        try:
            analysis = self.analyzer.analyse(transcript)
        except SchemaViolation as exc:
            # Marked failed rather than persisted partially: half an analysis on a
            # compliance record is worse than a visible gap.
            log.error("analysis rejected by schema",
                      extra={"call_id": transcript.context.call_id, "errors": exc.errors[:3]})
            outcome.notes.append("analysis failed schema validation")
            # ERROR on this span, not on the finalize span above it: compliance is
            # still going to run, and marking the whole call failed here would hide
            # the calls that genuinely produced nothing.
            span_.degraded("analysis failed schema validation", exc)
            return None
        except Exception as exc:  # noqa: BLE001
            log.error("analysis failed", extra={"call_id": transcript.context.call_id,
                                                "error_type": type(exc).__name__})
            outcome.notes.append("analysis provider failed")
            span_.degraded("analysis provider failed", exc)
            return None

        try:
            paise = self.cost_policy.cost_paise(
                analysis.model, analysis.input_tokens, analysis.output_tokens
            )
        except KeyError:
            # An unpriced model must be loud: silently recording zero spend is how a
            # budget gets blown by a model nobody added to the pricing table.
            log.error("no pricing configured", extra={"model": analysis.model})
            outcome.notes.append(f"model {analysis.model} has no pricing configured")
            span_.degraded("model has no pricing configured")
            paise = 0
        budget.record(paise)
        # Per-tenant spend, per model, as it happens. Without this the only record of
        # model spend is the provider's monthly invoice, which arrives a month after
        # the budget it blew (cost.py's alerts at 70% and 90% are what this feeds).
        telemetry.record_model_spend(paise, model=analysis.model,
                                     tenant_id=transcript.context.tenant_id)
        self.sink.save_analysis(transcript.context.call_id, analysis, paise)
        return analysis

    def _review(self, findings: list[Finding], transcript: Transcript,
                judge_rules: set[str], outcome: Outcome, budget: TenantBudget) -> list[Finding]:
        reviewed: list[Finding] = []
        to_review = escalate(findings, judge_rules)
        untouched = [f for f in findings if f not in to_review]
        ctx = transcript.context
        model = str(getattr(self.judge.provider, "name", "unknown"))

        # Escalation rate is escalations over reviews, computed by whatever reads the
        # metrics rather than here: a rate computed in-process goes wrong the moment a
        # worker restarts mid-call, and a rate above 1 in a compliance dashboard costs
        # more credibility than the number is worth.
        telemetry.record_judge_escalation(len(to_review), tenant_id=ctx.tenant_id,
                                          model=model)
        started = time.perf_counter()
        with telemetry.span("judge", tenant_id=ctx.tenant_id, call_id=ctx.call_id,
                            provider=model, findings=len(to_review)) as span_:
            for finding in to_review:
                try:
                    verdict, in_tokens, out_tokens = self.judge.review(finding, transcript)
                except Exception as exc:  # noqa: BLE001
                    log.error("judge failed", extra={"call_id": transcript.context.call_id,
                                                     "rule_id": finding.rule_id,
                                                     "error_type": type(exc).__name__})
                    # An unreviewed tier-1 finding still stands. Dropping it because the
                    # judge was unavailable would silently reduce compliance coverage —
                    # which is why this is an error on the judge span and not on the
                    # finalize span: the flag survives, unreviewed.
                    span_.degraded("judge provider failed", exc)
                    telemetry.record_judge_review("failed", tenant_id=ctx.tenant_id,
                                                  model=model, rule_id=finding.rule_id)
                    reviewed.append(finding)
                    continue
                try:
                    paise = self.cost_policy.cost_paise(
                        self.judge.provider.name, in_tokens, out_tokens)
                except KeyError:
                    outcome.notes.append(
                        f"model {self.judge.provider.name} has no pricing configured")
                else:
                    budget.record(paise)
                    telemetry.record_model_spend(paise, model=model,
                                                 tenant_id=ctx.tenant_id)
                if verdict is None:
                    outcome.notes.append(
                        f"judge returned an unusable verdict for {finding.rule_id}")
                    telemetry.record_judge_review("unusable", tenant_id=ctx.tenant_id,
                                                  model=model, rule_id=finding.rule_id)
                    reviewed.append(finding)
                    continue
                applied = self.judge.apply(finding, verdict)
                telemetry.record_judge_review(
                    "upheld" if applied is not None else "dismissed",
                    tenant_id=ctx.tenant_id, model=model, rule_id=finding.rule_id)
                if applied is not None:
                    reviewed.append(applied)
        telemetry.record_stage("judge", _elapsed_ms(started), status="ok",
                               tenant_id=ctx.tenant_id)
        return sorted(reviewed + untouched, key=lambda f: (-f.severity.rank, f.span_start_ms or 0))
