"""NATS JetStream consumer: the long-running half of the pipeline.

Kept deliberately thin. Everything that can be wrong about *what* the pipeline does
lives in :mod:`sentinel_pipeline.worker` and is tested without a broker; this file
only handles delivery semantics.

Two properties the subject layout and ack policy have to give us:

* **At-least-once with idempotent effects.** A redelivered ``call.finalize`` must
  re-run harmlessly. It does: transcripts, analyses and flags are all written with
  the call id as the key, so a second run overwrites rather than duplicates
  (:mod:`sentinel_pipeline.persistence` spells out which columns a re-run owns and
  which belong to a human).
* **A slow call must not block the stream.** Analysis of a forty-minute call can take
  a minute; ``AckWait`` is set well above that and the message is acked only once the
  work is durable, so a worker that dies mid-analysis redelivers rather than losing
  the call.

## The message

The gateway publishes exactly this and nothing else:

    {"call_id": "<ulid>", "tenant_id": "<uuid>", "attempt": <int>,
     "finalized_at": "<RFC3339>"}

No transcript, no audio, no borrower data on the bus. Everything else is looked up
from Postgres under that tenant's row-level-security context, which is what keeps the
message itself uninteresting to anyone who can read the stream, and what makes a
redelivery cheap to reason about: the message is a pointer, not a payload.

A message that does not parse, or that is missing ``call_id`` or ``tenant_id``, can
never succeed on redelivery. It goes straight to the dead-letter subject rather than
burning four attempts and twenty minutes of ``AckWait`` first — as does a handler
that raises :class:`Unprocessable`, which is how "this call does not exist for this
tenant" is distinguished from "the model provider is down".

## Authentication

The stream carries instructions to spend money: every ``call.finalize`` message
causes ASR and, usually, two LLM calls against the named tenant's budget. An
unauthenticated broker therefore is not merely an information risk, it is an
unmetered spend amplifier for anyone who can reach the port. So the consumer refuses
to connect to a non-loopback server with no credentials unless an operator has said
in as many words that they mean it (``SENTINEL_NATS_ALLOW_INSECURE=1``), and TLS is
configuration rather than code.
"""

from __future__ import annotations

import asyncio
import logging
import os
from dataclasses import dataclass, field
from typing import Awaitable, Callable, Mapping

from . import telemetry

log = logging.getLogger(__name__)

STREAM = "SENTINEL"
SUBJECT_FINALIZE = "sentinel.call.finalize"
SUBJECT_DLQ = "sentinel.call.dlq"

# Comfortably above the slowest realistic analysis, so a working consumer is never
# redelivered a call it is still processing.
ACK_WAIT_SECONDS = 300
MAX_DELIVER = 4


class Unprocessable(Exception):
    """The handler cannot ever succeed with this message.

    Raised for a call that does not exist for the named tenant, or a payload the
    handler cannot make sense of. Retrying is pointless, so the message is
    dead-lettered on the first delivery instead of after ``max_deliver``.
    """


class NatsConfigError(RuntimeError):
    """A broker configuration that cannot be honoured as written."""


@dataclass
class ConsumerConfig:
    """Connection, subscription and credentials.

    Credentials, in the order nats-py itself prefers them:

    ``SENTINEL_NATS_CREDS``     path to a ``.creds`` file (NATS 2 JWT + seed). The
                                production shape: per-service identity, revocable at
                                the operator without redeploying anything.
    ``SENTINEL_NATS_NKEY_SEED`` path to an nkey seed file.
    ``SENTINEL_NATS_USER`` / ``SENTINEL_NATS_PASSWORD``
    ``SENTINEL_NATS_TOKEN``

    TLS:

    ``SENTINEL_NATS_TLS``          ``1`` to require TLS. Implied by a ``tls://`` URL.
    ``SENTINEL_NATS_CA``           CA bundle to verify the broker against.
    ``SENTINEL_NATS_CLIENT_CERT`` / ``SENTINEL_NATS_CLIENT_KEY``  for mTLS.
    ``SENTINEL_NATS_TLS_HOSTNAME`` name to verify when connecting via an address that
                                   is not the certificate's name.
    """

    servers: str = "nats://127.0.0.1:4222"
    durable: str = "finalize-workers"
    max_in_flight: int = 8
    ack_wait_seconds: int = ACK_WAIT_SECONDS
    max_deliver: int = MAX_DELIVER

    creds_file: str | None = None
    nkey_seed_file: str | None = None
    user: str | None = None
    password: str | None = None
    token: str | None = None

    tls: bool = False
    tls_ca_file: str | None = None
    tls_cert_file: str | None = None
    tls_key_file: str | None = None
    tls_hostname: str | None = None

    #: Named so a broker operator can tell which service is connected.
    client_name: str = "sentinel-pipeline"
    connect_timeout_s: int = 10
    #: Reconnect forever by default. A pipeline that gives up on the broker stops
    #: producing compliance records while the calls keep arriving, so waiting is the
    #: better failure. Note that this also applies to the *initial* connect: the
    #: consumer blocks until the broker answers rather than exiting. Under an
    #: orchestrator that would rather see a crash loop than a quiet wait, set
    #: SENTINEL_NATS_MAX_RECONNECT_ATTEMPTS to a finite number.
    max_reconnect_attempts: int = -1
    #: Explicit permission to talk to a non-loopback broker with no credentials.
    allow_insecure: bool = False

    _server_list: tuple[str, ...] = field(default=(), init=False, repr=False)

    def __post_init__(self) -> None:
        self._server_list = tuple(s.strip() for s in self.servers.split(",") if s.strip())
        if not self._server_list:
            raise NatsConfigError("no NATS servers configured")
        if any(s.startswith("tls://") for s in self._server_list):
            self.tls = True

    @property
    def server_list(self) -> tuple[str, ...]:
        return self._server_list

    @property
    def authenticated(self) -> bool:
        return bool(self.creds_file or self.nkey_seed_file or self.token
                    or (self.user and self.password))

    @staticmethod
    def from_env(env: Mapping[str, str] | None = None) -> "ConsumerConfig":
        env = dict(os.environ if env is None else env)
        config = ConsumerConfig(
            servers=env.get("SENTINEL_NATS_SERVERS") or "nats://127.0.0.1:4222",
            durable=env.get("SENTINEL_NATS_DURABLE") or "finalize-workers",
            max_in_flight=int(env.get("SENTINEL_NATS_MAX_IN_FLIGHT", "8")),
            ack_wait_seconds=int(env.get("SENTINEL_NATS_ACK_WAIT_SECONDS",
                                         str(ACK_WAIT_SECONDS))),
            max_deliver=int(env.get("SENTINEL_NATS_MAX_DELIVER", str(MAX_DELIVER))),
            creds_file=env.get("SENTINEL_NATS_CREDS") or None,
            nkey_seed_file=env.get("SENTINEL_NATS_NKEY_SEED") or None,
            user=env.get("SENTINEL_NATS_USER") or None,
            password=env.get("SENTINEL_NATS_PASSWORD") or None,
            token=env.get("SENTINEL_NATS_TOKEN") or None,
            tls=_flag(env.get("SENTINEL_NATS_TLS")),
            tls_ca_file=env.get("SENTINEL_NATS_CA") or None,
            tls_cert_file=env.get("SENTINEL_NATS_CLIENT_CERT") or None,
            tls_key_file=env.get("SENTINEL_NATS_CLIENT_KEY") or None,
            tls_hostname=env.get("SENTINEL_NATS_TLS_HOSTNAME") or None,
            connect_timeout_s=int(env.get("SENTINEL_NATS_CONNECT_TIMEOUT", "10")),
            max_reconnect_attempts=int(
                env.get("SENTINEL_NATS_MAX_RECONNECT_ATTEMPTS", "-1")),
            allow_insecure=_flag(env.get("SENTINEL_NATS_ALLOW_INSECURE")),
        )
        config.validate()
        return config

    def validate(self) -> None:
        """Refuse the configurations that are wrong rather than merely unusual."""
        if (self.tls_cert_file is None) != (self.tls_key_file is None):
            raise NatsConfigError(
                "SENTINEL_NATS_CLIENT_CERT and SENTINEL_NATS_CLIENT_KEY must be set "
                "together for mutual TLS"
            )
        if (self.user is None) != (self.password is None):
            raise NatsConfigError(
                "SENTINEL_NATS_USER and SENTINEL_NATS_PASSWORD must be set together"
            )
        if self.authenticated or self.allow_insecure:
            return
        remote = [s for s in self._server_list if not _is_loopback(s)]
        if remote:
            raise NatsConfigError(
                "refusing to connect to a remote NATS server with no credentials: "
                "every message on this stream authorises model spend against a named "
                "tenant. Set SENTINEL_NATS_CREDS (or NKEY_SEED, or USER/PASSWORD), or "
                "SENTINEL_NATS_ALLOW_INSECURE=1 if the broker is genuinely trusted."
            )
        log.warning("connecting to a local NATS server with no credentials")

    def connect_options(self) -> dict[str, object]:
        """Keyword arguments for ``nats.connect``.

        A pure function of the configuration, deliberately: it is the part of the
        connection worth testing, and it can be asserted on without a broker.
        """
        options: dict[str, object] = {
            "servers": list(self._server_list),
            "name": self.client_name,
            "connect_timeout": self.connect_timeout_s,
            "max_reconnect_attempts": self.max_reconnect_attempts,
        }
        if self.creds_file:
            options["user_credentials"] = self.creds_file
        elif self.nkey_seed_file:
            options["nkeys_seed"] = self.nkey_seed_file
        elif self.user and self.password:
            options["user"] = self.user
            options["password"] = self.password
        elif self.token:
            options["token"] = self.token
        if self.tls:
            options["tls"] = self.tls_context()
            if self.tls_hostname:
                options["tls_hostname"] = self.tls_hostname
        return options

    def tls_context(self) -> object:
        """A verifying TLS context.

        Hostname checking and certificate verification are left on. There is no
        ``SENTINEL_NATS_TLS_INSECURE`` and there should not be one: the broker is
        reached over the customer's network, and a consumer that accepts any
        certificate is a consumer whose stream can be fed by anything on that path.
        """
        import ssl  # noqa: PLC0415 - stdlib, but only the TLS path needs it

        context = ssl.create_default_context(purpose=ssl.Purpose.SERVER_AUTH,
                                             cafile=self.tls_ca_file)
        if self.tls_cert_file and self.tls_key_file:
            context.load_cert_chain(certfile=self.tls_cert_file,
                                    keyfile=self.tls_key_file)
        return context


def _flag(value: str | None) -> bool:
    return (value or "").strip().lower() in {"1", "true", "yes", "on"}


def _is_loopback(server: str) -> bool:
    host = server.split("://", 1)[-1].split("/", 1)[0]
    host = host.rsplit("@", 1)[-1].rsplit(":", 1)[0].strip("[]")
    return host in {"127.0.0.1", "localhost", "::1", ""}


async def run(config: ConsumerConfig, handle: Callable[[dict], Awaitable[None]]) -> None:
    """Consume finalize messages until cancelled.

    ``handle`` is given the decoded message and must raise to signal failure; a raised
    exception leaves the message unacked so JetStream redelivers it, and after
    ``max_deliver`` attempts it lands on the dead-letter subject for a human.
    :class:`Unprocessable` short-circuits that: it dead-letters immediately.
    """
    import nats  # noqa: PLC0415 - lazy so unit tests need no broker
    from nats.js.api import ConsumerConfig as JsConsumerConfig
    from nats.js.api import DeliverPolicy

    config.validate()
    # Logged before the attempt, because with the default reconnect policy this call
    # waits for the broker indefinitely: without a line here, a consumer pointed at
    # the wrong endpoint is silent rather than obviously stuck.
    log.info("connecting to nats", extra={"servers": ",".join(config.server_list),
                                          "tls": config.tls,
                                          "authenticated": config.authenticated})
    nc = await nats.connect(**config.connect_options())
    js = nc.jetstream()
    log.info("consumer connected", extra={"durable": config.durable,
                                          "tls": config.tls,
                                          "authenticated": config.authenticated})

    subscription = await js.pull_subscribe(
        SUBJECT_FINALIZE,
        durable=config.durable,
        config=JsConsumerConfig(
            ack_wait=config.ack_wait_seconds,
            max_deliver=config.max_deliver,
            deliver_policy=DeliverPolicy.ALL,
            max_ack_pending=config.max_in_flight,
        ),
    )

    try:
        while True:
            try:
                messages = await subscription.fetch(batch=config.max_in_flight, timeout=5)
            except asyncio.TimeoutError:
                continue
            await asyncio.gather(*(_process(js, msg, handle, config.max_deliver)
                                   for msg in messages))
    finally:
        await nc.drain()


async def _process(js, msg, handle: Callable[[dict], Awaitable[None]],
                   max_deliver: int = MAX_DELIVER) -> None:
    import json

    try:
        payload = json.loads(msg.data)
        if not isinstance(payload, dict):
            raise ValueError("finalize message is not an object")
    except ValueError:
        # A message we cannot even parse will never succeed on redelivery, so it goes
        # straight to the dead-letter subject rather than burning four attempts.
        log.error("undecodable finalize message", extra={"subject": msg.subject})
        telemetry.record_dlq("undecodable")
        await js.publish(SUBJECT_DLQ, msg.data)
        await msg.ack()
        return

    call_id = payload.get("call_id", "unknown")
    try:
        await handle(payload)
    except Unprocessable as exc:
        # Permanent by construction: the call is not there, or the message is not
        # about anything. One attempt, then a human.
        log.error("finalize message is unprocessable",
                  extra={"call_id": call_id, "error_type": type(exc).__name__})
        telemetry.record_dlq("unprocessable")
        await js.publish(SUBJECT_DLQ, msg.data)
        await msg.ack()
        return
    except Exception as exc:  # noqa: BLE001 - the handler's failures are opaque here
        # No call content in the log: an exception message can carry a transcript
        # fragment from a provider that echoes its input.
        log.error("finalize failed", extra={"call_id": call_id,
                                            "error_type": type(exc).__name__,
                                            "delivered": msg.metadata.num_delivered})
        if msg.metadata.num_delivered >= max_deliver:
            telemetry.record_dlq("exhausted")
            await js.publish(SUBJECT_DLQ, msg.data)
            await msg.ack()
        else:
            # Negative-ack with a delay rather than letting AckWait expire: a
            # provider outage should back off, not retry four times in a minute.
            await msg.nak(delay=min(60, 5 * msg.metadata.num_delivered))
        return
    await msg.ack()
