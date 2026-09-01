import sys
from datetime import datetime, timezone
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from sentinel_pipeline.compliance.engine import load_default_rule_set  # noqa: E402
from sentinel_pipeline.models import (  # noqa: E402
    CallContext,
    Channel,
    ChannelTranscript,
    Transcript,
    Word,
)


def words(text: str, start_ms: int = 0, wpm: int = 150) -> list[Word]:
    """Lay a phrase out on a timeline at a plausible speaking rate.

    Real timings matter for these tests: the rules that depend on *when* something
    was said (identification in the first 30 s, disclosure after a denial) would pass
    vacuously against zero-length spans.
    """
    ms_per_word = int(60_000 / wpm)
    out = []
    t = start_ms
    for token in text.split():
        out.append(Word(text=token, start_ms=t, end_ms=t + ms_per_word))
        t += ms_per_word
    return out


def channel(ch: Channel, *segments: tuple[int, str]) -> ChannelTranscript:
    """Build a channel transcript from (start_ms, text) segments."""
    all_words: list[Word] = []
    for start_ms, text in segments:
        all_words.extend(words(text, start_ms))
    return ChannelTranscript(
        channel=ch,
        text=" ".join(text for _, text in segments),
        words=all_words,
        language="en",
        provider="fixture",
        provider_version="1",
    )


def call(
    *,
    near: ChannelTranscript | None = None,
    far: ChannelTranscript | None = None,
    started_at: datetime | None = None,
    duration_ms: int = 300_000,
    account_ref: str | None = "LN-1",
    prior_contacts_24h: int = 0,
    interruptions: int | None = 2,
) -> Transcript:
    ctx = CallContext(
        call_id="01J8ZQ8H2Q7X9K3M4N5P6R7S8T",
        tenant_id="tenant-1",
        user_uid="agent-a",
        started_at=started_at or datetime(2026, 9, 1, 5, 30, tzinfo=timezone.utc),  # 11:00 IST
        duration_ms=duration_ms,
        account_ref=account_ref,
        prior_contacts_24h=prior_contacts_24h,
        interruptions=interruptions,
    )
    channels = {}
    if near is not None:
        channels[Channel.NEAR] = near
    if far is not None:
        channels[Channel.FAR] = far
    return Transcript(context=ctx, channels=channels)


# A compliant opening the rules should never flag, reused across tests so a change
# that starts flagging good calls fails loudly.
COMPLIANT_OPENING = (
    "Good morning, my name is Ravi and I am calling from Acme Recovery Services "
    "on behalf of the bank regarding your loan account which is overdue."
)


@pytest.fixture(scope="session")
def default_rules():
    return load_default_rule_set()


@pytest.fixture
def make_call():
    return call


@pytest.fixture
def make_channel():
    return channel
