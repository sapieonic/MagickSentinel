"""Consumer configuration and delivery semantics.

No broker anywhere in here. The two things worth testing about this file are the
connection options — a pure function of the environment — and the ack decisions,
which are the difference between a redelivered call and a lost one.

The authentication tests are security tests rather than plumbing tests. Every message
on this stream authorises ASR and two LLM calls against a named tenant's budget, so a
broker anyone can publish to is an unmetered spend amplifier as well as an
information leak.
"""

import asyncio
import ssl

import pytest

from sentinel_pipeline.consumer import (
    ACK_WAIT_SECONDS,
    MAX_DELIVER,
    SUBJECT_DLQ,
    ConsumerConfig,
    NatsConfigError,
    Unprocessable,
    _process,
)


# ------------------------------------------------------------------------- config


def test_the_defaults_are_the_local_development_broker():
    config = ConsumerConfig.from_env({})
    assert config.server_list == ("nats://127.0.0.1:4222",)
    assert config.durable == "finalize-workers"
    assert config.ack_wait_seconds == ACK_WAIT_SECONDS
    assert config.max_deliver == MAX_DELIVER
    assert config.tls is False and config.authenticated is False


def test_every_knob_comes_from_the_environment():
    config = ConsumerConfig.from_env({
        "SENTINEL_NATS_SERVERS": "tls://a.example:4222,tls://b.example:4222",
        "SENTINEL_NATS_DURABLE": "finalize-mumbai",
        "SENTINEL_NATS_MAX_IN_FLIGHT": "16",
        "SENTINEL_NATS_ACK_WAIT_SECONDS": "600",
        "SENTINEL_NATS_MAX_DELIVER": "6",
        "SENTINEL_NATS_CREDS": "/etc/sentinel/pipeline.creds",
        "SENTINEL_NATS_CA": "/etc/ssl/nats-ca.pem",
        "SENTINEL_NATS_TLS_HOSTNAME": "nats.internal",
        "SENTINEL_NATS_CONNECT_TIMEOUT": "20",
    })
    assert len(config.server_list) == 2
    assert config.durable == "finalize-mumbai" and config.max_in_flight == 16
    assert config.ack_wait_seconds == 600 and config.max_deliver == 6
    assert config.connect_timeout_s == 20


def test_a_tls_url_turns_tls_on_without_a_second_variable():
    config = ConsumerConfig(servers="tls://nats.example:4222",
                            creds_file="/etc/sentinel/pipeline.creds")
    assert config.tls is True


@pytest.mark.parametrize("env,expected_key,expected_value", [
    ({"SENTINEL_NATS_CREDS": "/c.creds"}, "user_credentials", "/c.creds"),
    ({"SENTINEL_NATS_NKEY_SEED": "/s.nk"}, "nkeys_seed", "/s.nk"),
    ({"SENTINEL_NATS_USER": "pipeline", "SENTINEL_NATS_PASSWORD": "s3cret"},
     "user", "pipeline"),
    ({"SENTINEL_NATS_TOKEN": "t0ken"}, "token", "t0ken"),
])
def test_each_credential_shape_reaches_the_client(env, expected_key, expected_value):
    config = ConsumerConfig.from_env({**env, "SENTINEL_NATS_SERVERS":
                                      "nats://broker.example:4222"})
    options = config.connect_options()
    assert options[expected_key] == expected_value
    assert options["servers"] == ["nats://broker.example:4222"]
    assert options["name"] == "sentinel-pipeline"


def test_credentials_are_preferred_in_the_order_nats_itself_prefers_them():
    config = ConsumerConfig(servers="nats://broker.example:4222",
                            creds_file="/c.creds", nkey_seed_file="/s.nk",
                            user="u", password="p", token="t")
    options = config.connect_options()
    assert options["user_credentials"] == "/c.creds"
    assert "nkeys_seed" not in options and "user" not in options and "token" not in options


def test_reconnection_is_forever_by_default_and_boundable():
    # Forever is right for production — a pipeline that gives up on the broker stops
    # producing compliance records while the calls keep arriving — but it also means
    # the first connect waits rather than exiting, so it has to be settable.
    assert ConsumerConfig.from_env({}).connect_options()["max_reconnect_attempts"] == -1
    bounded = ConsumerConfig.from_env({"SENTINEL_NATS_MAX_RECONNECT_ATTEMPTS": "3"})
    assert bounded.connect_options()["max_reconnect_attempts"] == 3


def test_a_remote_broker_with_no_credentials_is_refused():
    # An unauthenticated stream is a way to spend another tenant's model budget from
    # off-box, not merely a way to read four identifiers.
    with pytest.raises(NatsConfigError, match="no credentials"):
        ConsumerConfig.from_env({"SENTINEL_NATS_SERVERS": "nats://broker.example:4222"})


def test_a_loopback_broker_with_no_credentials_is_allowed_with_a_warning():
    # Development. The warning is in the log; the point of the test is that it runs.
    ConsumerConfig.from_env({"SENTINEL_NATS_SERVERS": "nats://127.0.0.1:4222"})
    ConsumerConfig.from_env({"SENTINEL_NATS_SERVERS": "nats://localhost:4222"})


def test_the_insecure_escape_hatch_has_to_be_asked_for_explicitly():
    config = ConsumerConfig.from_env({
        "SENTINEL_NATS_SERVERS": "nats://broker.example:4222",
        "SENTINEL_NATS_ALLOW_INSECURE": "1",
    })
    assert config.authenticated is False


def test_half_configured_credentials_are_refused_rather_than_half_used():
    with pytest.raises(NatsConfigError, match="together"):
        ConsumerConfig.from_env({"SENTINEL_NATS_USER": "u",
                                 "SENTINEL_NATS_SERVERS": "nats://127.0.0.1:4222"})
    with pytest.raises(NatsConfigError, match="mutual TLS"):
        ConsumerConfig.from_env({"SENTINEL_NATS_CLIENT_CERT": "/c.pem",
                                 "SENTINEL_NATS_SERVERS": "nats://127.0.0.1:4222"})


def test_tls_produces_a_verifying_context():
    config = ConsumerConfig(servers="tls://broker.example:4222",
                            token="t", tls_hostname="nats.internal")
    options = config.connect_options()
    context = options["tls"]
    assert isinstance(context, ssl.SSLContext)
    # There is deliberately no way to turn either of these off from the environment:
    # a consumer that accepts any certificate is a consumer whose stream can be fed
    # by anything on the customer's network.
    assert context.verify_mode is ssl.CERT_REQUIRED
    assert context.check_hostname is True
    assert options["tls_hostname"] == "nats.internal"


def test_no_servers_at_all_is_a_configuration_error():
    with pytest.raises(NatsConfigError):
        ConsumerConfig(servers="   ")


# ------------------------------------------------------------ delivery semantics


class FakeMsg:
    def __init__(self, data: bytes, delivered: int = 1):
        self.data = data
        self.subject = "sentinel.call.finalize"
        self.metadata = type("Meta", (), {"num_delivered": delivered})()
        self.acked = False
        self.naks: list[int] = []

    async def ack(self):
        self.acked = True

    async def nak(self, delay=None):
        self.naks.append(delay)


class FakeJs:
    def __init__(self):
        self.published: list[tuple[str, bytes]] = []

    async def publish(self, subject, data):
        self.published.append((subject, data))


GOOD = b'{"call_id":"01J8ZQ8H2Q7X9K3M4N5P6R7S8T","tenant_id":"t1","attempt":1,' \
       b'"finalized_at":"2026-09-01T10:19:44Z"}'


def run(coro):
    # asyncio.run rather than pytest-asyncio: the suite is meant to need only pytest
    # and jsonschema, and one event loop per test is no harder to read.
    return asyncio.run(coro)


def test_a_handled_message_is_acked_once():
    js, msg = FakeJs(), FakeMsg(GOOD)
    seen = []

    async def handle(payload):
        seen.append(payload)

    run(_process(js, msg, handle))
    assert msg.acked and not msg.naks and not js.published
    # The payload reaches the handler exactly as published: four identifiers, no
    # transcript, no audio, nothing borrower-related on the bus.
    assert set(seen[0]) == {"call_id", "tenant_id", "attempt", "finalized_at"}


def test_a_transient_failure_is_negative_acked_with_a_backoff():
    js, msg = FakeJs(), FakeMsg(GOOD, delivered=2)

    async def handle(payload):
        raise RuntimeError("provider unavailable")

    run(_process(js, msg, handle, max_deliver=4))
    assert not msg.acked and msg.naks == [10]
    assert not js.published, "a retryable failure must not be dead-lettered"


def test_the_last_attempt_dead_letters_and_acks():
    js, msg = FakeJs(), FakeMsg(GOOD, delivered=4)

    async def handle(payload):
        raise RuntimeError("provider unavailable")

    run(_process(js, msg, handle, max_deliver=4))
    assert msg.acked
    assert js.published == [(SUBJECT_DLQ, GOOD)]


def test_the_configured_max_deliver_governs_rather_than_the_module_default():
    # The config field and the retry decision have to agree, or a deployment that
    # raises max_deliver gets four attempts and a dead letter anyway.
    js, msg = FakeJs(), FakeMsg(GOOD, delivered=2)

    async def handle(payload):
        raise RuntimeError("boom")

    run(_process(js, msg, handle, max_deliver=2))
    assert js.published and msg.acked


def test_an_undecodable_message_goes_straight_to_the_dead_letter_subject():
    # It will never succeed on redelivery, so burning four attempts and twenty
    # minutes of AckWait first only delays the human.
    js, msg = FakeJs(), FakeMsg(b"not json at all")

    async def handle(payload):  # pragma: no cover - must not be called
        raise AssertionError("the handler should never see an undecodable message")

    run(_process(js, msg, handle))
    assert js.published == [(SUBJECT_DLQ, b"not json at all")] and msg.acked


def test_a_json_array_is_not_a_finalize_message():
    js, msg = FakeJs(), FakeMsg(b'["call_id"]')

    async def handle(payload):  # pragma: no cover
        raise AssertionError("the handler should never see a non-object payload")

    run(_process(js, msg, handle))
    assert js.published and msg.acked


def test_an_unprocessable_call_is_dead_lettered_on_the_first_attempt():
    # "This call does not exist for this tenant" is permanent — under RLS it is also
    # what another tenant's call looks like — so retrying cannot help.
    js, msg = FakeJs(), FakeMsg(GOOD, delivered=1)

    async def handle(payload):
        raise Unprocessable("call 01J8 is not present for this tenant")

    run(_process(js, msg, handle))
    assert js.published == [(SUBJECT_DLQ, GOOD)] and msg.acked and not msg.naks
