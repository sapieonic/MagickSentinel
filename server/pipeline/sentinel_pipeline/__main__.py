"""``python -m sentinel_pipeline`` — the three ways this service runs.

Also installed as the ``sentinel-pipeline`` console script (see ``pyproject.toml``),
which is what a systemd unit or a container entrypoint should call.

    sentinel-pipeline consume                 # the JetStream consumer
    sentinel-pipeline retention [--commit]    # nightly purge; dry run by default
    sentinel-pipeline coverage [--day DATE]   # nightly CDR reconciliation
    sentinel-pipeline check                   # validate configuration and exit

``check`` exists because every failure this service can have at startup — a language
the chosen ASR provider cannot read, a database role that bypasses row-level
security, a NATS server with no credentials, an object store that is not configured —
is one somebody would rather discover from a deploy step than from a stream of failed
calls. It builds everything the consumer builds and then exits.

The exit code is the contract with the scheduler: non-zero when a tenant's purge hit
errors or a tenant's CDR export was missing, so a cron that ignores stdout still
notices.
"""

from __future__ import annotations

import argparse
import logging
import os
import sys
from datetime import date

from .cdr import CdrUnavailable
from .consumer import NatsConfigError
from .db import DatabaseConfigError
from .providers.registry import ProviderConfigError
from .service import (
    build_finalize_service,
    configure_logging,
    run_consumer,
    run_coverage,
    run_retention,
)

log = logging.getLogger("sentinel_pipeline")


def _check(env: dict[str, str]) -> int:
    """Build everything, connect to nothing that can be avoided, then exit."""
    from .cdr import cdr_source_from_env  # noqa: PLC0415
    from .consumer import ConsumerConfig  # noqa: PLC0415

    consumer = ConsumerConfig.from_env(env)
    service = build_finalize_service(env)
    service.db.open()
    try:
        service.db.assert_rls_enforced()
    finally:
        service.db.close()
    log.info("configuration ok", extra={
        "nats_servers": ",".join(consumer.server_list),
        "nats_tls": consumer.tls,
        "nats_authenticated": consumer.authenticated,
        "asr": str(getattr(service.asr, "name", "unknown")),
        "analysis": service.analyzer is not None,
        "judge": service.judge is not None,
        "priced_models": len(service.cost_policy.pricing),
        "cdr_adapter": cdr_source_from_env(env) is not None,
    })
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="sentinel-pipeline",
                                     description=__doc__.split("\n", 1)[0])
    parser.add_argument("--log-level", default=None)
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("consume", help="run the JetStream finalize consumer")

    retention = sub.add_parser("retention", help="nightly retention purge (dry run "
                                                 "unless --commit)")
    retention.add_argument(
        "--commit", action="store_true",
        help="actually delete. Equivalent to SENTINEL_RETENTION_COMMIT=1; without "
             "either, the job only reports what it would remove.")

    coverage = sub.add_parser("coverage", help="nightly CDR reconciliation")
    coverage.add_argument("--day", default=None,
                          help="ISO date to reconcile; defaults to yesterday in each "
                               "tenant's own timezone")

    sub.add_parser("check", help="validate configuration and exit")

    args = parser.parse_args(argv)
    configure_logging(args.log_level)
    env = dict(os.environ)

    try:
        if args.command == "consume":
            return run_consumer(env)
        if args.command == "retention":
            if args.commit:
                # The flag and the variable are the same switch, so a scheduler can
                # use either and neither is silently overridden by the other.
                env["SENTINEL_RETENTION_COMMIT"] = "1"
            return run_retention(env)
        if args.command == "coverage":
            return run_coverage(env, day=date.fromisoformat(args.day) if args.day else None)
        if args.command == "check":
            return _check(env)
    except (DatabaseConfigError, NatsConfigError, ProviderConfigError,
            CdrUnavailable) as exc:
        # Configuration errors are for a human reading a deploy log, not a stack
        # trace: they name the variable that is wrong.
        log.error("configuration error", extra={"detail": str(exc)})
        return 2
    return 2  # pragma: no cover - argparse rejects anything else


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
