"""Wiring: the composition root for every way this package is run.

Until this module existed the pipeline had a worker that was a pure function, a
consumer with no producer, two nightly jobs that ran against ``Protocol`` interfaces
nothing implemented, and no way to start any of it. Everything here is assembly —
read the environment, construct the real collaborators, hand them to code that does
not know where they came from. The decisions all live in the modules being assembled.

Three jobs come out of it, and :mod:`sentinel_pipeline.__main__` is the CLI over them:

* ``consume``  — the long-running JetStream consumer.
* ``retention`` — the nightly purge. **Dry run unless told otherwise.**
* ``coverage``  — the nightly CDR reconciliation.

One structural note about the consumer, because getting it wrong is subtle. The
finalize path is synchronous and I/O-bound — object storage, an ASR provider, two LLM
calls, Postgres — while the consumer is asyncio. Running it inline on the event loop
would block every other message's ack, ``AckWait`` would expire on calls that were
being processed perfectly well, and JetStream would redeliver them: a pipeline that
gets slower and then starts doing everything twice. So each finalize runs in a worker
thread and the loop stays free to ack.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import sys
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta, timezone
from typing import Mapping

from . import telemetry
from .analysis import CallAnalyzer
from .blobstore import blob_store_from_env
from .cdr import CdrUnavailable, cdr_source_from_env
from .compliance.judge import ComplianceJudge
from .consumer import ConsumerConfig, Unprocessable, run as run_loop
from .cost import CostPolicy, ModelPricing
from .coverage import reconcile
from .db import Database, DatabaseConfig
from .persistence import (
    CallNotFound,
    PostgresCallRepository,
    PostgresCoverageStore,
    PostgresRetentionStore,
    PostgresSegmentIndex,
    PostgresSink,
    list_tenants,
)
from .providers.registry import build_batch_asr, settings_from_env, warnings_for
from .retention import RetentionJob
from .segments import SegmentCodec, StoredSegmentSource
from .worker import Finalizer, Outcome

log = logging.getLogger(__name__)


# ------------------------------------------------------------------------- logging


class _JsonFormatter(logging.Formatter):
    """Structured logs, because the invariant is structured logging on every tier.

    The call sites in this package pass their fields through ``extra=`` and never
    interpolate call content into the message, so rendering the record as JSON is
    enough — there is no place here where a transcript could arrive as a positional
    format argument. Anything the standard library put on the record is dropped,
    which also means an accidental ``exc_info`` cannot smuggle a provider's echoed
    input into the log stream.
    """

    _STANDARD = frozenset(logging.LogRecord("", 0, "", 0, "", None, None).__dict__)

    def format(self, record: logging.LogRecord) -> str:
        payload = {
            "level": record.levelname.lower(),
            "logger": record.name,
            "msg": record.getMessage(),
        }
        for key, value in record.__dict__.items():
            if key in self._STANDARD or key in {"message", "asctime", "taskName"}:
                continue
            payload[key] = value if isinstance(value, (str, int, float, bool)) else str(value)
        return json.dumps(payload, default=str)


def configure_logging(level: str | None = None) -> None:
    handler = logging.StreamHandler(sys.stderr)
    handler.setFormatter(_JsonFormatter())
    root = logging.getLogger()
    root.handlers = [handler]
    root.setLevel((level or os.environ.get("SENTINEL_LOG_LEVEL") or "INFO").upper())


# ------------------------------------------------------------------------- pricing


def pricing_from_env(env: Mapping[str, str] | None = None) -> dict[str, ModelPricing]:
    """Model prices in paise per million tokens.

    ``SENTINEL_MODEL_PRICING="claude-sonnet-5=300/1500,gpt-4.1=250/1000"``

    Empty by default, and that is deliberate rather than lazy: ``cost.py`` raises on
    an unpriced model and ``worker.py`` turns that into a loud note on the call
    instead of recording zero spend. A wrong price is worse than a missing one — it
    produces a budget that looks fine — so the table is configuration a human enters
    per deployment, not a constant that drifts out of date in the source tree.
    """
    env = dict(os.environ if env is None else env)
    raw = env.get("SENTINEL_MODEL_PRICING", "").strip()
    pricing: dict[str, ModelPricing] = {}
    for entry in (part.strip() for part in raw.split(",")):
        if not entry:
            continue
        model, sep, prices = entry.partition("=")
        input_price, slash, output_price = prices.partition("/")
        if not sep or not slash:
            raise ValueError(
                f"SENTINEL_MODEL_PRICING entry {entry!r} is not "
                f"model=<input paise per Mtok>/<output paise per Mtok>"
            )
        pricing[model.strip()] = ModelPricing(
            model=model.strip(),
            input_paise_per_mtok=int(input_price),
            output_paise_per_mtok=int(output_price),
        )
    return pricing


# ----------------------------------------------------------------- model providers


def _model_provider(env: Mapping[str, str], slot: str, schema_name: str) -> object | None:
    """Build the analysis or judge provider named in the environment.

    ``none`` is an explicit, supported answer: it runs tier-1 compliance only, which
    is what the kill switch does too and is a legitimate deployment. What is *not*
    supported is guessing — an absent variable does not silently become the fake
    provider, because a fake analysis stored against a real call is indistinguishable
    from a real one in the portal.
    """
    name = (env.get(f"SENTINEL_{slot}_PROVIDER") or "").strip().lower()
    if not name or name == "none":
        log.warning("no %s provider configured; tier-1 compliance only", slot.lower())
        return None

    if name == "anthropic":
        from .providers.anthropic import AnthropicProvider  # noqa: PLC0415

        key = env.get("SENTINEL_ANTHROPIC_API_KEY")
        if not key:
            raise RuntimeError(f"SENTINEL_{slot}_PROVIDER=anthropic needs "
                               f"SENTINEL_ANTHROPIC_API_KEY")
        return AnthropicProvider(api_key=key, schema_name=schema_name,
                                 **_model_kwargs(env, slot))
    if name == "openai":
        from .providers.openai import OpenAIProvider  # noqa: PLC0415

        key = env.get("SENTINEL_OPENAI_API_KEY")
        if not key:
            raise RuntimeError(f"SENTINEL_{slot}_PROVIDER=openai needs "
                               f"SENTINEL_OPENAI_API_KEY")
        return OpenAIProvider(api_key=key, schema_name=schema_name,
                              **_model_kwargs(env, slot))
    if name == "fake":
        from .providers.fake import FakeAnalysisProvider, FakeJudgeProvider  # noqa: PLC0415

        # Deliberately loud. The fake is for development and for the test suite; a
        # deployment that reaches this line is storing invented analyses.
        log.warning("using the deterministic fake %s provider", slot.lower())
        return FakeAnalysisProvider() if schema_name == "analysis.json" else FakeJudgeProvider()
    raise RuntimeError(
        f"unknown SENTINEL_{slot}_PROVIDER={name!r}; expected anthropic, openai, "
        f"fake or none"
    )


def _model_kwargs(env: Mapping[str, str], slot: str) -> dict[str, object]:
    model = env.get(f"SENTINEL_{slot}_MODEL")
    return {"model": model} if model else {}


# --------------------------------------------------------------------- the message


@dataclass(frozen=True)
class FinalizeMessage:
    """The bus message, and nothing more than the bus message.

    ``attempt`` and ``finalized_at`` are carried for observability — a call on its
    third attempt is worth noticing — and are not inputs to the pipeline. Unknown
    fields are ignored rather than rejected so that a gateway which grows a field
    does not stall the stream; missing ``call_id`` or ``tenant_id`` is permanent
    (:class:`sentinel_pipeline.consumer.Unprocessable`), because there is nothing to
    look up and no retry will change that.
    """

    call_id: str
    tenant_id: str
    attempt: int = 0
    finalized_at: str | None = None

    @staticmethod
    def from_payload(payload: Mapping[str, object]) -> "FinalizeMessage":
        call_id = str(payload.get("call_id") or "").strip()
        tenant_id = str(payload.get("tenant_id") or "").strip()
        if not call_id or not tenant_id:
            raise Unprocessable("finalize message is missing call_id or tenant_id")
        try:
            attempt = int(payload.get("attempt") or 0)
        except (TypeError, ValueError):
            attempt = 0
        finalized_at = payload.get("finalized_at")
        return FinalizeMessage(
            call_id=call_id,
            tenant_id=tenant_id,
            attempt=attempt,
            finalized_at=str(finalized_at) if finalized_at else None,
        )


# --------------------------------------------------------------- the finalize service


@dataclass
class FinalizeService:
    """Everything a finalize needs that is not per-call.

    The ASR adapter, the analyser, the judge, the cost policy and the connection
    pools are built once. The per-tenant collaborators — sink, segment index — are
    built per message, because they carry the tenant whose RLS context every one of
    their statements runs under, and a shared one would be a tenant leak waiting for
    a refactor.
    """

    db: Database
    blob: object
    asr: object
    analyzer: CallAnalyzer | None
    judge: ComplianceJudge | None
    cost_policy: CostPolicy
    codec: SegmentCodec = field(default_factory=SegmentCodec)

    def __post_init__(self) -> None:
        self.repo = PostgresCallRepository(self.db)

    def finalize(self, message: FinalizeMessage) -> Outcome:
        try:
            ctx = self.repo.call_context(message.tenant_id, message.call_id)
            budget = self.repo.budget(message.tenant_id)
            rules = self.repo.rule_engine(message.tenant_id)
        except CallNotFound as exc:
            # Under RLS a call belonging to another tenant is indistinguishable from
            # a call that does not exist, which is the intended behaviour and also
            # means retrying cannot help.
            raise Unprocessable(str(exc)) from exc
        except ValueError as exc:
            # A call id that is not a ULID cannot be looked up in any number of
            # attempts either.
            raise Unprocessable(str(exc)) from exc

        finalizer = Finalizer(
            asr=self.asr,
            analyzer=self.analyzer,
            rules=rules,
            judge=self.judge,
            segments=StoredSegmentSource(
                index=PostgresSegmentIndex(self.db, message.tenant_id),
                blob=self.blob,
                codec=self.codec,
            ),
            sink=PostgresSink(self.db, message.tenant_id),
            cost_policy=self.cost_policy,
        )
        outcome = finalizer.finalize(ctx, budget)
        log.info("finalized", extra={"call_id": message.call_id,
                                     "tenant_id": message.tenant_id,
                                     "attempt": message.attempt,
                                     "status": outcome.status,
                                     "findings": len(outcome.findings),
                                     "cost_paise": outcome.cost_paise,
                                     "notes": "; ".join(outcome.notes)})
        if outcome.status == "failed":
            # Raised so the message is *not* acked: no audio yet is the normal case
            # for a finalize that arrives before the last segments have landed, and a
            # redelivery a minute later usually succeeds. The DLQ catches the calls
            # where it never does.
            raise RuntimeError(f"finalize produced no transcript for {message.call_id}")
        return outcome

    async def handle(self, payload: Mapping[str, object]) -> None:
        message = FinalizeMessage.from_payload(payload)
        # In a thread: see the module docstring. A blocking finalize on the event
        # loop stalls every other message's ack until AckWait expires.
        await asyncio.to_thread(self.finalize, message)


# ------------------------------------------------------------------------ assembly


def build_finalize_service(env: Mapping[str, str] | None = None, *,
                           db: Database | None = None,
                           blob: object | None = None) -> FinalizeService:
    env = dict(os.environ if env is None else env)

    asr_settings = settings_from_env(env)
    for note in warnings_for(asr_settings):
        # Degradations a deployment is allowed to choose, said out loud at startup
        # rather than discovered in a review of coarse evidence spans.
        log.warning("asr selection: %s", note)
    # Raises rather than falling back if the floor's language is one the chosen
    # provider cannot read. See providers/registry.py: a silent fallback hands a bank
    # a clean-looking transcript with no flags on it.
    asr = build_batch_asr(asr_settings)

    analysis_provider = _model_provider(env, "ANALYSIS", "analysis.json")
    judge_provider = _model_provider(env, "JUDGE", "judge.json")

    codec = SegmentCodec(
        raw_pcm=(env.get("SENTINEL_SEGMENT_CODEC", "opus").strip().lower() == "pcm16")
    )
    return FinalizeService(
        db=db or Database(DatabaseConfig.from_env(env)),
        blob=blob if blob is not None else blob_store_from_env(env),
        asr=asr,
        analyzer=CallAnalyzer(analysis_provider) if analysis_provider is not None else None,
        judge=ComplianceJudge(judge_provider) if judge_provider is not None else None,
        cost_policy=CostPolicy(pricing=pricing_from_env(env)),
        codec=codec,
    )


# ---------------------------------------------------------------------- entrypoints


def run_consumer(env: Mapping[str, str] | None = None) -> int:
    """Build the real pipeline and consume ``sentinel.call.finalize`` forever."""
    env = dict(os.environ if env is None else env)
    telemetry.configure(telemetry.TelemetryConfig.from_env(env))
    consumer_config = ConsumerConfig.from_env(env)

    service = build_finalize_service(env)
    service.db.open()
    service.db.assert_rls_enforced()
    if service.db.config.max_size < consumer_config.max_in_flight:
        # Every in-flight message runs a finalize in its own thread, and each one
        # holds a connection for the length of a statement. Fewer connections than
        # in-flight messages is a queue nobody can see.
        log.warning("database pool is smaller than the in-flight message limit",
                    extra={"pool": service.db.config.max_size,
                           "max_in_flight": consumer_config.max_in_flight})
    try:
        asyncio.run(run_loop(consumer_config, service.handle))
    except KeyboardInterrupt:  # pragma: no cover - operator interrupt
        log.info("consumer stopped")
    finally:
        service.db.close()
        telemetry.shutdown()
    return 0


def run_retention(env: Mapping[str, str] | None = None, *,
                  db: Database | None = None, blob: object | None = None) -> int:
    """The nightly purge.

    Dry run unless ``SENTINEL_RETENTION_COMMIT=1``. The periods come from the
    database per tenant (**OPEN-6**: the schema's 30 and 365 days are placeholders
    that have never been checked against a real requirement), and the job has never
    deleted anything in this repository's history — so the first real run is on a
    customer's evidence, and it should have to be asked for explicitly.
    """
    env = dict(os.environ if env is None else env)
    telemetry.configure(telemetry.TelemetryConfig.from_env(env))
    commit = (env.get("SENTINEL_RETENTION_COMMIT") or "").strip().lower() in {
        "1", "true", "yes", "on"}

    database = db or Database(DatabaseConfig.from_env(env))
    opened = db is None
    if opened:
        database.open()
        database.assert_rls_enforced()
    try:
        job = RetentionJob(
            store=PostgresRetentionStore(database),
            blob=blob if blob is not None else blob_store_from_env(env),
            batch_size=int(env.get("SENTINEL_RETENTION_BATCH_SIZE", "1000")),
            dry_run=not commit,
        )
        results = job.run()
        for result in results:
            log.info("retention", extra={"tenant_id": result.tenant_id,
                                         "dry_run": result.dry_run,
                                         "audio_segments": result.audio_segments,
                                         "transcripts": result.transcripts,
                                         "swept_objects": result.swept_objects,
                                         "swept_days": result.swept_days,
                                         "errors": len(result.errors)})
            if not result.dry_run:
                telemetry.record_retention(
                    objects=result.audio_segments + result.swept_objects,
                    rows=result.transcripts, table="transcripts",
                    tenant_id=result.tenant_id)
        if not commit:
            log.warning("retention ran as a dry run; set SENTINEL_RETENTION_COMMIT=1 "
                        "to delete")
        return 1 if any(r.errors for r in results) else 0
    finally:
        if opened:
            database.close()
        telemetry.shutdown()


def run_coverage(env: Mapping[str, str] | None = None, *, day: date | None = None,
                 db: Database | None = None) -> int:
    """The nightly CDR reconciliation, for one day.

    Defaults to yesterday, in each tenant's own timezone, because a job that runs
    just after midnight and reconciles "today" reconciles four minutes of it.

    A tenant whose export is missing is **skipped**, loudly, with its previous rows
    untouched. Writing zeros would be worse than writing nothing: ``Coverage.pct``
    reads no dialer calls as 100% coverage, and 100% is the number this whole feature
    exists to be able to prove.
    """
    env = dict(os.environ if env is None else env)
    telemetry.configure(telemetry.TelemetryConfig.from_env(env))
    source = cdr_source_from_env(env)
    if source is None:
        log.warning("no CDR adapter configured (SENTINEL_CDR_ADAPTER); coverage "
                    "reconciliation is OPEN-7 and needs a sample export first")
        return 0

    database = db or Database(DatabaseConfig.from_env(env))
    opened = db is None
    if opened:
        database.open()
        database.assert_rls_enforced()
    store = PostgresCoverageStore(database)
    failures = 0
    try:
        for tenant in list_tenants(database):
            target = day or _yesterday(tenant.timezone)
            try:
                cdr_calls = list(source.calls_for(tenant.tenant_id, target))
            except CdrUnavailable as exc:
                log.error("no CDR export; coverage not written",
                          extra={"tenant_id": tenant.tenant_id,
                                 "day": target.isoformat(),
                                 "error_type": type(exc).__name__})
                failures += 1
                continue
            captured = store.captured_calls(tenant.tenant_id, target, tenant.timezone)
            rows = reconcile(tenant.tenant_id, target, cdr_calls, captured,
                             agent_for=store.agent_map(tenant.tenant_id))
            store.write_coverage(rows)
            for row in rows:
                telemetry.record_coverage(row.pct, tenant_id=tenant.tenant_id)
            log.info("coverage written", extra={"tenant_id": tenant.tenant_id,
                                                "day": target.isoformat(),
                                                "agents": len(rows),
                                                "dialer_calls": sum(r.dialer_calls
                                                                    for r in rows)})
        return 1 if failures else 0
    finally:
        if opened:
            database.close()
        telemetry.shutdown()


def _yesterday(tz: str) -> date:
    from zoneinfo import ZoneInfo  # noqa: PLC0415 - stdlib, only this path needs it

    try:
        zone = ZoneInfo(tz)
    except Exception:  # noqa: BLE001
        zone = timezone.utc
    return (datetime.now(zone) - timedelta(days=1)).date()
