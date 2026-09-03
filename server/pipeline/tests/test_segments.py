"""Reading stored audio, and the one filter that must never be wrong.

Half of this file is about the ``foreign`` flag. ``contracts/wire.md`` §4.2 says a
foreign segment is stored and never transcribed, ``worker.py``'s ``SegmentSource``
docstring says the filtering happens in the implementation so there is one place to
get it wrong, and this is that place — so it is tested from both directions: that
foreign segments never reach the decoder, and that non-foreign ones always do.

The rest is the segment framing from §4.1, including the dropped-frame case. A
dropped frame must become 20 ms of silence rather than being closed up: closing it
shifts every later timestamp on one channel relative to the other, and the two
timelines are what every compliance finding's evidence span is measured against.
"""

import struct

import pytest

from sentinel_pipeline.blobstore import MemoryBlobStore, segment_key
from sentinel_pipeline.models import Channel
from sentinel_pipeline.segments import (
    FRAMES_PER_SEGMENT,
    SILENCE_FRAME,
    MalformedSegment,
    PassthroughFrameDecoder,
    SegmentCodec,
    SegmentRow,
    StoredSegmentSource,
    transcribable,
    unpack_frames,
)


def frame(payload: bytes) -> bytes:
    return struct.pack("<H", len(payload)) + payload


def segment(*payloads: bytes) -> bytes:
    return b"".join(frame(p) for p in payloads)


# ------------------------------------------------------------------- the framing


def test_a_segment_unpacks_into_its_packets():
    raw = segment(b"aaa", b"bb", b"c")
    assert unpack_frames(raw) == [b"aaa", b"bb", b"c"]


def test_a_zero_length_frame_is_a_dropped_frame_not_an_empty_packet():
    # The distinction is the whole point: None means "the client lost this 20 ms and
    # the decoder owes silence", where b"" would be handed to libopus as a packet.
    assert unpack_frames(segment(b"aa", b"", b"bb")) == [b"aa", None, b"bb"]


def test_a_dropped_frame_becomes_exactly_twenty_milliseconds_of_silence():
    codec = SegmentCodec(decoder_factory=PassthroughFrameDecoder)
    pcm = codec.decode_segments([segment(b"\x01\x02", b"", b"\x03\x04")])
    assert pcm == b"\x01\x02" + SILENCE_FRAME + b"\x03\x04"
    # 320 samples of 16-bit mono at 16 kHz.
    assert len(SILENCE_FRAME) == 640


def test_a_truncated_segment_raises_rather_than_decoding_what_arrived():
    # A short read is a storage fault. Transcribing the first fragment of a second
    # would move every timestamp after it on that channel.
    with pytest.raises(MalformedSegment):
        unpack_frames(struct.pack("<H", 10) + b"only-4")
    with pytest.raises(MalformedSegment):
        unpack_frames(segment(b"ok") + b"\x05")


def test_a_full_one_second_segment_is_fifty_frames():
    raw = segment(*[b"\x00" * 60] * FRAMES_PER_SEGMENT)
    assert len(unpack_frames(raw)) == 50


def test_segments_are_decoded_with_one_decoder_for_the_whole_channel():
    # Opus carries filter and packet-loss-concealment state across packets, so a
    # fresh decoder per second would put a discontinuity every 50 frames.
    created = []

    class Counting(PassthroughFrameDecoder):
        def __init__(self):
            created.append(self)

    codec = SegmentCodec(decoder_factory=Counting)
    codec.decode_segments([segment(b"a"), segment(b"b"), segment(b"c")])
    assert len(created) == 1


def test_raw_pcm_mode_skips_the_framing_entirely():
    # SENTINEL_SEGMENT_CODEC=pcm16, for development fixtures that were never Opus.
    codec = SegmentCodec(raw_pcm=True)
    assert codec.decode_segments([b"\x01\x02", b"\x03"]) == b"\x01\x02\x03"


# ------------------------------------------------------ the foreign-audio filter


def rows(*specs):
    return [SegmentRow(seq=seq, s3_key=key, foreign_audio=foreign)
            for seq, key, foreign in specs]


def test_foreign_segments_are_dropped():
    kept = transcribable(rows((0, "k0", False), (1, "k1", True), (2, "k2", False)))
    assert [r.s3_key for r in kept] == ["k0", "k2"]


def test_a_channel_of_nothing_but_foreign_audio_yields_no_audio_at_all():
    # A tier B agent whose softphone was never detected produces exactly this: a
    # loopback channel full of whatever their speakers were playing. The honest
    # answer is "no audio on this channel", which worker.py already handles.
    assert transcribable(rows((0, "k0", True), (1, "k1", True))) == []


def test_segments_are_ordered_by_sequence_and_not_by_arrival():
    kept = transcribable(rows((2, "k2", False), (0, "k0", False), (1, "k1", False)))
    assert [r.s3_key for r in kept] == ["k0", "k1", "k2"]


def test_the_source_never_fetches_a_foreign_segment():
    # Defence in depth: the SQL already excludes these rows. If a future query edit
    # drops the WHERE clause, this is what stops an agent's music being transcribed
    # as a borrower's words and flagged against the agent.
    class Index:
        def segments_for(self, call_id, channel):
            return rows((0, "clean", False), (1, "music", True))

    blob = MemoryBlobStore({"clean": b"\x01\x02", "music": b"\xff\xff"})
    source = StoredSegmentSource(index=Index(), blob=blob, codec=SegmentCodec(raw_pcm=True))
    assert source.channel_audio("01J8", Channel.FAR) == b"\x01\x02"


# ------------------------------------------------------------------ the source


class FixedIndex:
    def __init__(self, rows_):
        self.rows = rows_
        self.asked = []

    def segments_for(self, call_id, channel):
        self.asked.append((call_id, int(channel)))
        return self.rows


def test_the_source_concatenates_a_channel_in_sequence_order():
    keys = [segment_key("t", "2026-09-01", "c", 0, i) for i in range(3)]
    blob = MemoryBlobStore({k: bytes([i + 1]) for i, k in enumerate(keys)})
    index = FixedIndex(rows(*[(i, k, False) for i, k in enumerate(keys)]))
    source = StoredSegmentSource(index=index, blob=blob, codec=SegmentCodec(raw_pcm=True))

    assert source.channel_audio("01J8", Channel.NEAR) == b"\x01\x02\x03"
    assert index.asked == [("01J8", 1)]


def test_no_rows_means_none_rather_than_empty_bytes():
    # worker.py distinguishes "no audio on this channel" from "no audio at all", and
    # b"" reads as the first to a truthiness check and the second to a length check.
    source = StoredSegmentSource(index=FixedIndex([]), blob=MemoryBlobStore(),
                                 codec=SegmentCodec(raw_pcm=True))
    assert source.channel_audio("01J8", Channel.FAR) is None


def test_an_object_missing_from_storage_does_not_lose_the_rest_of_the_call():
    # The gateway writes the object before the row, and retention deletes in that
    # order too, so a row whose object has gone is a real state. The remaining audio
    # is still worth transcribing.
    blob = MemoryBlobStore({"k0": b"\x01", "k2": b"\x03"})
    index = FixedIndex(rows((0, "k0", False), (1, "k1", False), (2, "k2", False)))
    source = StoredSegmentSource(index=index, blob=blob, codec=SegmentCodec(raw_pcm=True))
    assert source.channel_audio("01J8", Channel.FAR) == b"\x01\x03"


def test_every_object_missing_reads_as_no_audio():
    index = FixedIndex(rows((0, "gone", False)))
    source = StoredSegmentSource(index=index, blob=MemoryBlobStore(),
                                 codec=SegmentCodec(raw_pcm=True))
    assert source.channel_audio("01J8", Channel.FAR) is None
