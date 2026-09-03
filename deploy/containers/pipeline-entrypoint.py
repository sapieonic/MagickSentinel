#!/usr/bin/env python3
"""Entry point for the sentinel-pipeline container.

WHY THIS FILE EXISTS AT ALL, and why it does not do more than it does.

`server/pipeline` has no `__main__`, no console script and no `main()`. That is not an
omission — it is where the component honestly is. `worker.py`'s `Finalizer` is a pure
function over injected `Protocol` interfaces (`SegmentSource`, `Sink`, `BatchASR`,
`Analyzer`, `Judge`) and *nothing in the repository implements the database-backed
ones*. `retention.py` and `coverage.py` are in the same position, documented as such in
docs/security.md and docs/open-decisions.md. So a container cannot be given a real
finalize loop today without this file inventing the concrete store the repository has
deliberately not written yet, and inventing it here — in deployment machinery, owned by
a different work stream, untested — would be the worst possible place for it.

So this entrypoint does the two things that *are* real and refuses to pretend about the
third:

  1. `selftest` (the default). Validates the ASR provider/language configuration and
     proves every dependency the pipeline will need is reachable and correctly
     credentialled: Postgres, NATS JetStream with the stream present, and the object
     store. Then it holds, refreshing a liveness file. This is what makes
     `deploy/compose.yaml` a stack you can stand up and believe.

     The ASR validation is not filler. It is the invariant AGENTS.md states most
     forcefully: the default batch provider has no Tamil at all, and a Tamil floor
     pointed at it "would not fail — it would transcribe Tamil audio as something else
     and hand a bank a clean-looking transcript with no flags on it".
     `registry.validate` raises on that, and calling it at container start is what
     turns a silent wrong answer into a container that will not boot.

  2. `consume`. Runs the real `consumer.run` JetStream loop, and requires
     `SENTINEL_PIPELINE_SINK` to name an import path providing the concrete
     `SegmentSource`/`Sink`. With no sink configured it refuses to start rather than
     acking messages it cannot process — because JetStream at-least-once plus a
     handler that swallows would burn `max_deliver` attempts and land every call on the
     DLQ subject, which reads as "the pipeline processed the backlog" and means "the
     backlog is gone".

No PII anywhere in this file's output. It logs counts, hostnames, subject names and
provider names; never a call id, an account reference, a transcript or a user uid.
"""

from __future__ import annotations

import asyncio
import importlib
import json
import logging
import os
import pathlib
import signal
import sys
import time

# Structured, single-line JSON on stdout. The container runtime is the log shipper and
# the OTel collector's redaction processors (deploy/observability/otel-collector.yaml)
# are the second line of defence; a multi-line traceback split across log records is
# unparseable by both.
logging.basicConfig(
    level=os.environ.get("SENTINEL_LOG_LEVEL", "INFO"),
    format='{"ts":"%(asctime)s","level":"%(levelname)s","logger":"%(name)s","msg":"%(message)s"}',
)
log = logging.getLogger("sentinel.pipeline.entrypoint")

# The liveness file the image's HEALTHCHECK stats. The pipeline is a JetStream consumer
# with no HTTP surface, so unlike the gateway there is no endpoint to probe — inventing
# one would mean adding an HTTP server to server/pipeline, which this work stream does
# not own. A file whose mtime the main loop refreshes is the honest substitute: it
# proves the loop is turning, which is exactly what a liveness probe should prove and
# no more.
#
# When the pipeline grows a real health endpoint, replace this and the HEALTHCHECK
# together; a liveness file that outlives the thing it stood in for is worse than none,
# because it keeps reporting healthy.
LIVENESS_PATH = pathlib.Path(os.environ.get("SENTINEL_LIVENESS_FILE", "/tmp/pipeline-alive"))
LIVENESS_INTERVAL_SECONDS = 10


def touch_liveness() -> None:
    LIVENESS_PATH.parent.mkdir(parents=True, exist_ok=True)
    LIVENESS_PATH.write_text(str(int(time.time())), encoding="utf-8")


# ---------------------------------------------------------------------------- config


def require_env(name: str, why: str) -> str:
    value = os.environ.get(name, "")
    if not value:
        raise SystemExit(f"{name} is required: {why}")
    return value


def check_asr_configuration() -> None:
    """The startup gate that matters most, run before any dependency is touched.

    Cheap, and it fails on the thing that would otherwise fail invisibly: a floor
    configured for a language the chosen provider cannot read produces confident
    transcripts of the wrong words, and every downstream compliance finding is then
    computed over fiction. There is no monitoring signal for that. There is only this
    check.
    """
    from sentinel_pipeline.providers import registry  # noqa: PLC0415

    settings = registry.settings_from_env()
    registry.validate(settings)

    log.info(
        "asr configuration accepted: provider=%s languages=%s routes=%s",
        settings.provider,
        ",".join(settings.languages) or "(none configured)",
        ",".join(f"{k}={v}" for k, v in sorted(settings.routes.items())) or "(none)",
    )
    if not settings.languages:
        # Not fatal — `validate` cannot object to an empty list — but worth saying
        # loudly, because an empty language list means the check above verified
        # nothing at all.
        log.warning(
            "SENTINEL_ASR_LANGUAGES is empty, so no language/provider coverage was "
            "actually verified. Set it to the floor's languages."
        )
    for note in registry.warnings_for(settings):
        # Degradations, not errors: coarse evidence spans are worse than precise ones,
        # not unusable, and a startup that refused to boot over one would be wrong.
        log.warning("asr degradation: %s", note)


# ------------------------------------------------------------------ dependency probes


async def check_postgres() -> None:
    import asyncpg  # noqa: PLC0415

    dsn = require_env(
        "SENTINEL_DATABASE_URL",
        "the pipeline reads calls and writes transcripts, analyses and findings",
    )
    conn = await asyncpg.connect(dsn)
    try:
        # Three things in one round trip, all of them things that have gone wrong:
        #   * connectivity and credentials;
        #   * `current_user`, because the pipeline must connect as sentinel_pipeline
        #     (NOBYPASSRLS). Connecting as the schema owner would silently bypass
        #     row-level security and make every tenant's rows visible to every worker.
        #   * the migration state, so a worker started against an unmigrated database
        #     fails here rather than on its first insert.
        row = await conn.fetchrow(
            "SELECT current_user AS who, "
            "       (SELECT count(*) FROM information_schema.tables "
            "         WHERE table_schema = 'public' AND table_name = 'calls') AS has_calls"
        )
        who, has_calls = row["who"], row["has_calls"]
        if not has_calls:
            raise SystemExit(
                "connected to Postgres but there is no `calls` table: the database has "
                "not been migrated. Run the migration runner "
                "(docker compose run --rm migrate)."
            )
        if who == "postgres" or who.endswith("_owner"):
            log.warning(
                "connected as %r. The pipeline should connect as sentinel_pipeline, "
                "which is NOBYPASSRLS; a superuser or the schema owner bypasses "
                "row-level security and sees every tenant.",
                who,
            )
        # Confirm RLS is actually being enforced for this role rather than assuming it
        # from the role name. `row_security` off, or a BYPASSRLS role, both produce a
        # working pipeline that leaks across tenants.
        bypass = await conn.fetchval(
            "SELECT rolbypassrls FROM pg_roles WHERE rolname = current_user"
        )
        if bypass:
            raise SystemExit(
                f"the role {who!r} has BYPASSRLS. Tenant isolation in this product "
                "lives in the database, not the application: a BYPASSRLS worker "
                "returns every tenant's rows for every query. Refusing to start."
            )
        log.info("postgres ok: user=%s rls_enforced=true", who)
    finally:
        await conn.close()


async def check_nats(create_stream: bool) -> None:
    import nats  # noqa: PLC0415
    from nats.js.api import RetentionPolicy, StorageType, StreamConfig  # noqa: PLC0415

    from sentinel_pipeline.consumer import (  # noqa: PLC0415
        STREAM,
        SUBJECT_DLQ,
        SUBJECT_FINALIZE,
    )

    servers = require_env("SENTINEL_NATS_URL", "the finalize queue is NATS JetStream")
    # Credentials are mandatory, not optional. There is no anonymous path here on
    # purpose: an unauthenticated JetStream on a floor network lets anyone who can
    # reach 4222 inject `sentinel.call.finalize` messages, which spends the tenant's
    # model budget, or drain the stream, which silently loses calls that the gateway
    # has already acked to the endpoint agent — so the agent has deleted its only copy
    # of the audio.
    user = require_env("SENTINEL_NATS_USER", "NATS authentication is required; see deploy/nats/nats.conf")
    password = require_env("SENTINEL_NATS_PASSWORD", "NATS authentication is required; see deploy/nats/nats.conf")

    nc = await nats.connect(servers=servers.split(","), user=user, password=password)
    try:
        js = nc.jetstream()
        try:
            info = await js.stream_info(STREAM)
            log.info(
                "jetstream ok: stream=%s messages=%d bytes=%d subjects=%s",
                STREAM,
                info.state.messages,
                info.state.bytes,
                ",".join(info.config.subjects or []),
            )
        except Exception:
            if not create_stream:
                raise SystemExit(
                    f"JetStream stream {STREAM!r} does not exist. Create it in the "
                    "deployment's provisioning step, or set "
                    "SENTINEL_PIPELINE_CREATE_STREAM=1 for a local stack."
                )
            # Local/dev convenience only, and gated behind an explicit flag: in a real
            # deployment the stream's retention and replica count are capacity
            # decisions, and a worker that creates its own stream on first start will
            # create it with whatever defaults were in the worker image.
            await js.add_stream(
                StreamConfig(
                    name=STREAM,
                    subjects=[SUBJECT_FINALIZE, SUBJECT_DLQ],
                    retention=RetentionPolicy.LIMITS,
                    storage=StorageType.FILE,
                    max_age=7 * 24 * 3600,
                )
            )
            log.info("jetstream: created stream %s for local use", STREAM)
    finally:
        await nc.drain()


async def check_object_store() -> None:
    """The audio object store.

    Note what this does NOT assert: a region. OPEN-4 (data residency) is unresolved.
    The working assumption recorded in docs/open-decisions.md is India-only
    (`ap-south-1`), asserted in contracts/openapi.yaml and enforced by nothing, and it
    is not this file's place to turn an assumption into a constraint. What it does do
    is log the endpoint and region it was handed, so that "where is borrower audio
    actually being written" is answerable from a container's first ten log lines
    instead of from someone's memory of a Terraform run.
    """
    import boto3  # noqa: PLC0415
    from botocore.config import Config  # noqa: PLC0415

    endpoint = os.environ.get("SENTINEL_S3_ENDPOINT", "")
    bucket = os.environ.get("SENTINEL_S3_BUCKET", "")
    region = os.environ.get("SENTINEL_S3_REGION", "")
    if not bucket:
        # The gateway has no S3 adapter yet (docs/security.md, requirement 5) and
        # refuses to start without SENTINEL_BLOB_DIR, so a stack running on the
        # filesystem backend legitimately has no bucket. Skipping is correct; silently
        # pretending the check passed is not.
        log.warning(
            "SENTINEL_S3_BUCKET is unset, so the object store was not checked. The "
            "gateway currently has only filesystem and in-memory blob backends, so "
            "this is expected until the S3 adapter lands."
        )
        return

    session = boto3.session.Session()
    kwargs = {"config": Config(signature_version="s3v4", s3={"addressing_style": "path"})}
    if endpoint:
        kwargs["endpoint_url"] = endpoint
    if region:
        kwargs["region_name"] = region
    s3 = session.client("s3", **kwargs)
    s3.head_bucket(Bucket=bucket)
    log.info(
        "object store ok: bucket=%s endpoint=%s region=%s "
        "(residency is OPEN-4 and is not enforced here)",
        bucket,
        endpoint or "(aws default)",
        region or "(unset)",
    )


# ------------------------------------------------------------------------- the modes


async def run_selftest() -> int:
    check_asr_configuration()
    await check_postgres()
    await check_nats(create_stream=os.environ.get("SENTINEL_PIPELINE_CREATE_STREAM") == "1")
    await check_object_store()

    log.info(
        "selftest passed. This container is NOT processing calls: mode=selftest. "
        "There is no concrete SegmentSource/Sink in this repository yet "
        "(server/pipeline/worker.py defines them as Protocols), so a finalize loop "
        "would have nothing to write to. Set SENTINEL_PIPELINE_MODE=consume and "
        "SENTINEL_PIPELINE_SINK once one exists."
    )

    # Hold, so the container's dependency verdict stays inspectable and compose does
    # not treat a passing selftest as a crash loop. The liveness file is refreshed so
    # the HEALTHCHECK measures the loop rather than the process table.
    stopping = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stopping.set)
    while not stopping.is_set():
        touch_liveness()
        try:
            await asyncio.wait_for(stopping.wait(), timeout=LIVENESS_INTERVAL_SECONDS)
        except asyncio.TimeoutError:
            pass
    log.info("selftest mode stopping on signal")
    return 0


async def run_consumer() -> int:
    from sentinel_pipeline.consumer import ConsumerConfig, run  # noqa: PLC0415

    check_asr_configuration()
    await check_postgres()
    await check_nats(create_stream=False)
    await check_object_store()

    sink_path = os.environ.get("SENTINEL_PIPELINE_SINK", "")
    if not sink_path:
        # Fail closed. The tempting alternative — consume, log "not implemented", and
        # ack — is the worst available behaviour: JetStream would consider every call
        # delivered, the gateway has already acked the segments to the endpoint agent,
        # and the agent deletes a segment only after that ack. The audio would be gone
        # and the pipeline's queue would look healthy and empty.
        raise SystemExit(
            "SENTINEL_PIPELINE_MODE=consume requires SENTINEL_PIPELINE_SINK, an import "
            "path to a module exposing `build()` that returns the concrete "
            "SegmentSource/Sink pair from server/pipeline/sentinel_pipeline/worker.py. "
            "Nothing in this repository implements them yet. Refusing to consume a "
            "queue this container cannot process: an acked call whose audio the "
            "endpoint has already deleted is unrecoverable."
        )

    module_name, _, attr = sink_path.partition(":")
    module = importlib.import_module(module_name)
    factory = getattr(module, attr or "build")
    handler = factory()
    log.info("finalize handler loaded from %s", sink_path)

    config = ConsumerConfig(
        servers=os.environ["SENTINEL_NATS_URL"],
        durable=os.environ.get("SENTINEL_PIPELINE_DURABLE", "finalize-workers"),
        max_in_flight=int(os.environ.get("SENTINEL_PIPELINE_MAX_IN_FLIGHT", "8")),
    )

    async def wrapped(message: dict) -> None:
        touch_liveness()
        await handler(message)

    touch_liveness()
    await run(config, wrapped)
    return 0


def main() -> int:
    mode = os.environ.get("SENTINEL_PIPELINE_MODE", "selftest")
    log.info("sentinel-pipeline starting: mode=%s python=%s", mode, sys.version.split()[0])
    try:
        if mode == "selftest":
            return asyncio.run(run_selftest())
        if mode == "consume":
            return asyncio.run(run_consumer())
        raise SystemExit(
            f"SENTINEL_PIPELINE_MODE={mode!r} is not a mode. Use 'selftest' or 'consume'."
        )
    except SystemExit as exc:
        # One structured line rather than a traceback: the message is the diagnosis and
        # a traceback through asyncio.run buries it under twelve frames of event loop.
        log.error("%s", exc)
        print(json.dumps({"startup": "refused", "reason": str(exc)}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
