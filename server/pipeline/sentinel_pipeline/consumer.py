"""NATS JetStream consumer: the long-running half of the pipeline.

Kept deliberately thin. Everything that can be wrong about *what* the pipeline does
lives in :mod:`sentinel_pipeline.worker` and is tested without a broker; this file
only handles delivery semantics.

Two properties the subject layout and ack policy have to give us:

* **At-least-once with idempotent effects.** A redelivered ``call.finalize`` must
  re-run harmlessly. It does: transcripts, analyses and flags are all written with
  the call id as the key, so a second run overwrites rather than duplicates.
* **A slow call must not block the stream.** Analysis of a forty-minute call can take
  a minute; ``AckWait`` is set well above that and the message is acked only once the
  work is durable, so a worker that dies mid-analysis redelivers rather than losing
  the call.
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass
from typing import Awaitable, Callable

log = logging.getLogger(__name__)

STREAM = "SENTINEL"
SUBJECT_FINALIZE = "sentinel.call.finalize"
SUBJECT_DLQ = "sentinel.call.dlq"

# Comfortably above the slowest realistic analysis, so a working consumer is never
# redelivered a call it is still processing.
ACK_WAIT_SECONDS = 300
MAX_DELIVER = 4


@dataclass
class ConsumerConfig:
    servers: str = "nats://127.0.0.1:4222"
    durable: str = "finalize-workers"
    max_in_flight: int = 8
    ack_wait_seconds: int = ACK_WAIT_SECONDS
    max_deliver: int = MAX_DELIVER


async def run(config: ConsumerConfig, handle: Callable[[dict], Awaitable[None]]) -> None:
    """Consume finalize messages until cancelled.

    ``handle`` is given the decoded message and must raise to signal failure; a raised
    exception leaves the message unacked so JetStream redelivers it, and after
    ``max_deliver`` attempts it lands on the dead-letter subject for a human.
    """
    import nats  # noqa: PLC0415 - lazy so unit tests need no broker
    from nats.js.api import ConsumerConfig as JsConsumerConfig
    from nats.js.api import DeliverPolicy

    nc = await nats.connect(servers=config.servers.split(","))
    js = nc.jetstream()

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
            await asyncio.gather(*(_process(js, msg, handle) for msg in messages))
    finally:
        await nc.drain()


async def _process(js, msg, handle: Callable[[dict], Awaitable[None]]) -> None:
    import json

    try:
        payload = json.loads(msg.data)
    except ValueError:
        # A message we cannot even parse will never succeed on redelivery, so it goes
        # straight to the dead-letter subject rather than burning four attempts.
        log.error("undecodable finalize message", extra={"subject": msg.subject})
        await js.publish(SUBJECT_DLQ, msg.data)
        await msg.ack()
        return

    call_id = payload.get("call_id", "unknown")
    try:
        await handle(payload)
    except Exception as exc:  # noqa: BLE001 - the handler's failures are opaque here
        # No call content in the log: an exception message can carry a transcript
        # fragment from a provider that echoes its input.
        log.error("finalize failed", extra={"call_id": call_id,
                                            "error_type": type(exc).__name__,
                                            "delivered": msg.metadata.num_delivered})
        if msg.metadata.num_delivered >= MAX_DELIVER:
            await js.publish(SUBJECT_DLQ, msg.data)
            await msg.ack()
        else:
            # Negative-ack with a delay rather than letting AckWait expire: a
            # provider outage should back off, not retry four times in a minute.
            await msg.nak(delay=min(60, 5 * msg.metadata.num_delivered))
        return
    await msg.ack()
