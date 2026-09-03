"""Reading a call's stored audio back out of object storage.

This is the concrete half of :class:`sentinel_pipeline.worker.SegmentSource`, and it
carries the one filter that the whole product's credibility rests on.

**Foreign audio is stored and must never be transcribed.**
``contracts/wire.md`` §4.2 defines the ``foreign`` flag: on a tier B capture, the
loopback stream carries *everything* the agent's speakers play, and the client sets
the flag on any segment whose energy arrived while the softphone's audio session was
``Inactive``. The gateway stores those segments anyway — being able to prove what was
discarded is worth more than the storage — and records
``media_segments.foreign_audio = true``. ``worker.py`` deliberately pushed the
filtering down here so there is exactly one place to get it wrong.

Getting it wrong is not a quality regression, it is a fabricated compliance record.
The audio behind a foreign segment is music, a video, a colleague's call bleeding
through the room. Transcribe it and the pipeline runs RBI conduct rules over song
lyrics and files the hits against an agent who never said any of it, on a call that
may not have existed. The flag is checked twice below — once in SQL, once in Python —
because a future edit to either alone must not be able to turn it off.

The stored bytes are Opus, framed as ``contracts/wire.md`` §4.1 specifies: one
1-second segment is ``[u16 len][opus bytes] × 50``, with a zero length meaning the
capture dropped that 20 ms frame and the decoder must insert silence rather than
close the gap. Closing it would shift every timestamp after it on one channel only,
and the two channels' timelines are what every evidence span is measured against.
The ASR adapters take raw 16 kHz mono PCM16 (they add their own WAV header, see
``providers/sarvam.py``), so decoding happens here.
"""

from __future__ import annotations

import logging
import struct
from dataclasses import dataclass, field
from typing import Iterable, Protocol, Sequence

from .models import Channel

log = logging.getLogger(__name__)

#: Wire constants. These are contract, not tuning knobs — ``wire.md`` §4.1 and §7.
SAMPLE_RATE_HZ = 16_000
FRAME_MS = 20
FRAMES_PER_SEGMENT = 50
#: 20 ms of 16 kHz mono PCM16 = 320 samples = 640 bytes.
SILENCE_FRAME = b"\x00" * (SAMPLE_RATE_HZ // 1000 * FRAME_MS * 2)


class MalformedSegment(ValueError):
    """A stored segment does not match the framing in ``contracts/wire.md`` §4.1."""


def unpack_frames(payload: bytes) -> list[bytes | None]:
    """Split a segment payload into its Opus packets.

    ``None`` marks a dropped frame (length 0): the client synthesised nothing and
    flagged ``silence_inserted`` on the record, and the decoder owes 20 ms of silence
    so the channel stays aligned with the other one.

    Truncation raises rather than returning a short segment. A half-read object is a
    storage fault, and silently transcribing the first 300 ms of a second would move
    every later timestamp on that channel.
    """
    frames: list[bytes | None] = []
    offset = 0
    end = len(payload)
    while offset < end:
        if offset + 2 > end:
            raise MalformedSegment(
                f"segment ends mid-length at byte {offset} of {end}"
            )
        (length,) = struct.unpack_from("<H", payload, offset)
        offset += 2
        if offset + length > end:
            raise MalformedSegment(
                f"frame claims {length} bytes but only {end - offset} remain"
            )
        frames.append(payload[offset:offset + length] if length else None)
        offset += length
    return frames


class FrameDecoder(Protocol):
    """Decodes one Opus packet to 20 ms of 16 kHz mono PCM16."""

    def decode(self, packet: bytes) -> bytes: ...


@dataclass
class OpusFrameDecoder:
    """libopus, through ``opuslib``, imported lazily.

    Lazy for the same reason every provider SDK is: a development deployment reading
    PCM fixtures out of ``SENTINEL_BLOB_DIR`` must not need libopus present, and the
    unit tests must not need it at all.

    One decoder instance per channel and never shared: an Opus decoder is stateful
    (it carries the previous frame for packet-loss concealment), so feeding it two
    channels interleaved would corrupt both.
    """

    sample_rate: int = SAMPLE_RATE_HZ
    channels: int = 1
    _decoder: object = field(default=None, init=False, repr=False)

    def __post_init__(self) -> None:
        import opuslib  # noqa: PLC0415 - lazy, see the class docstring

        self._decoder = opuslib.Decoder(self.sample_rate, self.channels)

    def decode(self, packet: bytes) -> bytes:
        frame_size = self.sample_rate // 1000 * FRAME_MS
        return self._decoder.decode(packet, frame_size)


@dataclass
class PassthroughFrameDecoder:
    """Treats each packet's bytes as PCM16 already.

    For development fixtures and for tests: it makes the framing, the silence
    insertion and the foreign-audio filter testable without libopus, which is where
    the bugs that matter actually live.
    """

    def decode(self, packet: bytes) -> bytes:
        return packet


@dataclass
class SegmentCodec:
    """Turns stored segment objects into the PCM the ASR adapters expect."""

    decoder_factory: object = OpusFrameDecoder
    #: Set when the objects hold raw PCM16 rather than framed Opus, which is what a
    #: local fixture generator produces. Selected by SENTINEL_SEGMENT_CODEC=pcm16.
    raw_pcm: bool = False

    def new_decoder(self) -> FrameDecoder:
        return self.decoder_factory()

    def decode_segments(self, payloads: Iterable[bytes]) -> bytes:
        """Decode a channel's segments in sequence order into one PCM buffer.

        One decoder for the whole channel, created here rather than per segment:
        Opus's packet-loss concealment and its internal filter state span packets,
        and a fresh decoder per second would put a discontinuity every 50 frames.
        """
        if self.raw_pcm:
            return b"".join(payloads)
        decoder = self.new_decoder()
        out: list[bytes] = []
        for payload in payloads:
            for packet in unpack_frames(payload):
                out.append(SILENCE_FRAME if packet is None else decoder.decode(packet))
        return b"".join(out)


# --------------------------------------------------------------- foreign filtering


@dataclass(frozen=True)
class SegmentRow:
    """One row of ``media_segments``, as far as reading audio is concerned."""

    seq: int
    s3_key: str
    foreign_audio: bool


def transcribable(rows: Iterable[SegmentRow]) -> list[SegmentRow]:
    """Drop the segments that must never reach a transcriber, in sequence order.

    The single place the ``foreign`` rule from ``contracts/wire.md`` §4.2 is applied.
    The SQL in :mod:`sentinel_pipeline.persistence` already excludes these rows; this
    re-checks the flag on whatever came back, so that removing the ``WHERE`` clause —
    or adding a second query that forgets it — cannot silently start transcribing an
    agent's music as a borrower's words.

    Sorted by ``seq`` rather than trusting the caller's order, because the ASR sees
    one concatenated buffer and a segment out of order is a sentence out of order
    with correct-looking timestamps attached.
    """
    kept: list[SegmentRow] = []
    dropped = 0
    for row in sorted(rows, key=lambda r: r.seq):
        if row.foreign_audio:
            dropped += 1
            continue
        kept.append(row)
    if dropped:
        # Counted, not listed: the count is the operationally interesting part (a
        # tier B floor with a mis-detected softphone shows up as a huge one) and the
        # keys carry the tenant and call id.
        log.info("skipped foreign segments", extra={"segments": dropped})
    return kept


class SegmentIndex(Protocol):
    """Which objects hold one channel of one call, foreign segments already gone."""

    def segments_for(self, call_id: str, channel: Channel) -> Sequence[SegmentRow]: ...


@dataclass
class StoredSegmentSource:
    """The production :class:`sentinel_pipeline.worker.SegmentSource`.

    Composed of an index (Postgres, in production) and an object store, because the
    two failure modes are different and want different handling: a missing *row* means
    the channel was never captured, while a missing *object* means audio the database
    still believes in has gone, which is worth a log line even though the call can
    still be transcribed from what is left.
    """

    index: SegmentIndex
    blob: object              # BlobStore
    codec: SegmentCodec = field(default_factory=SegmentCodec)

    def channel_audio(self, call_id: str, channel: Channel) -> bytes | None:
        rows = transcribable(self.index.segments_for(call_id, channel))
        if not rows:
            # None rather than b"": worker.py distinguishes "no audio on this
            # channel" (survivable, one side of the call) from "no audio at all"
            # (the call is marked failed). An empty bytes object would read as the
            # former to a truthiness check and as the latter to a length check.
            return None

        payloads: list[bytes] = []
        missing = 0
        for row in rows:
            body = self.blob.get(row.s3_key)
            if body is None:
                missing += 1
                continue
            payloads.append(body)
        if missing:
            log.warning("segments missing from object storage",
                        extra={"segments": missing, "channel": int(channel)})
        if not payloads:
            return None
        return self.codec.decode_segments(payloads) or None
