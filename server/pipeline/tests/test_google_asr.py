"""Tests for the Gemini 3.5 Transcribe adapter.

A fake client stands in for the SDK, so these run without ``google-genai`` installed
and without network. The fake records what was sent, because the parts that matter
here are exactly the parts a live call would not tell you about until it was too
late: that timestamps were requested, that "smart" formatting was not, that
diarization was never asked for, and that a long call's word timings land on the
call's timeline rather than each chunk's.
"""

from __future__ import annotations

import base64

import pytest

from sentinel_pipeline.models import Channel
from sentinel_pipeline.providers.google import (
    GoogleTranscribeASR,
    GoogleTranscribeError,
)

SAMPLE_RATE = 16_000
BYTES_PER_SECOND = SAMPLE_RATE * 2


def interaction(words: list[tuple[str, float, float]], *, text: str | None = None,
                in_tokens: int = 100, out_tokens: int = 20) -> dict:
    """A response in the shape the Interactions API documents."""
    return {
        "output_text": text if text is not None else " ".join(w for w, _, _ in words),
        "steps": [
            {
                "type": "model_output",
                "content": [
                    {
                        "type": "text",
                        "text": " ".join(w for w, _, _ in words),
                        "annotations": [
                            {
                                "type": "word_info",
                                "text": w,
                                "start_offset": f"{start}s",
                                "end_offset": f"{end}s",
                            }
                            for w, start, end in words
                        ],
                    }
                ],
            }
        ],
        "usage_metadata": {
            "total_input_tokens": in_tokens,
            "total_output_tokens": out_tokens,
        },
    }


class FakeInteractions:
    def __init__(self, responses: list[dict]) -> None:
        self.responses = responses
        self.requests: list[dict] = []

    def create(self, **kwargs) -> dict:
        self.requests.append(kwargs)
        if not self.responses:
            raise AssertionError("the adapter made more requests than the test queued")
        return self.responses.pop(0)


class FakeFiles:
    def __init__(self) -> None:
        self.uploaded: list[int] = []
        self.deleted: list[str] = []

    def upload(self, *, file, config) -> dict:
        payload = file.read()
        self.uploaded.append(len(payload))
        return {"uri": "files/abc", "name": "files/abc",
                "mime_type": config["mime_type"]}

    def delete(self, *, name: str) -> None:
        self.deleted.append(name)


class FakeClient:
    def __init__(self, responses: list[dict]) -> None:
        self.interactions = FakeInteractions(responses)
        self.files = FakeFiles()


def silence(seconds: float) -> bytes:
    return b"\x00" * int(seconds * BYTES_PER_SECOND)


def test_transcribes_a_call_with_word_timings():
    client = FakeClient([interaction([("pandrah", 1.0, 1.4), ("hazaar", 1.4, 1.9)])])
    asr = GoogleTranscribeASR(client=client)

    result = asr.transcribe(silence(30), sample_rate=SAMPLE_RATE, language_hint="hi-IN")

    assert result.text == "pandrah hazaar"
    assert [(w.text, w.start_ms, w.end_ms) for w in result.words] == [
        ("pandrah", 1_000, 1_400),
        ("hazaar", 1_400, 1_900),
    ]
    assert result.provider == "google-transcribe"
    assert result.provider_version == "gemini-3.5-transcribe"
    assert result.language == "hi-IN"
    assert (result.input_tokens, result.output_tokens) == (100, 20)
    # No per-word confidence is reported, and none is invented: a synthesised score
    # would make an unverified span look reviewed.
    assert all(w.confidence is None for w in result.words)


def test_requests_verbatim_word_timestamps_and_never_diarization():
    client = FakeClient([interaction([("haan", 0.0, 0.3)])])
    GoogleTranscribeASR(client=client).transcribe(
        silence(5), sample_rate=SAMPLE_RATE, language_hint="mr-IN"
    )

    config = client.interactions.requests[0]["generation_config"]["transcription_config"]
    assert config["language_codes"] == ["mr-IN"]
    assert config["mode"] == {"type": "verbatim", "timestamp_granularities": ["word"]}
    # The channels were captured separately, so the speaker is known exactly. Asking
    # the model to guess it again would be strictly worse than the answer in hand.
    assert "diarization_mode" not in config["mode"]
    assert "custom_vocabulary" not in config


def test_inlines_small_audio_as_raw_pcm_with_its_rate():
    client = FakeClient([interaction([("ji", 0.0, 0.2)])])
    audio = silence(10)
    GoogleTranscribeASR(client=client).transcribe(audio, sample_rate=SAMPLE_RATE)

    part = client.interactions.requests[0]["input"][0]
    assert part["mime_type"] == "audio/l16"
    assert part["sample_rate"] == SAMPLE_RATE
    assert part["channels"] == 1
    assert base64.b64decode(part["data"]) == audio
    assert client.files.uploaded == []


def test_large_audio_goes_through_the_files_api_and_is_deleted_afterwards():
    client = FakeClient([interaction([("theek", 0.0, 0.4)])])
    asr = GoogleTranscribeASR(client=client, inline_limit_bytes=1_024)

    asr.transcribe(silence(10), sample_rate=SAMPLE_RATE)

    part = client.interactions.requests[0]["input"][0]
    assert part["uri"] == "files/abc"
    # Wrapped in WAV on this path: audio/l16 carries no sample rate of its own, and a
    # file referenced by URI cannot declare one alongside the bytes.
    assert part["mime_type"] == "audio/wav"
    assert client.files.uploaded == [len(silence(10)) + 44]
    # Call audio is subject to the tenant's retention period; it must not sit in a
    # third-party file store until an automatic expiry nobody chose.
    assert client.files.deleted == ["files/abc"]


def test_uploads_are_deleted_even_when_the_transcription_call_fails():
    client = FakeClient([])  # the first create() raises
    asr = GoogleTranscribeASR(client=client, inline_limit_bytes=1_024)

    with pytest.raises(AssertionError):
        asr.transcribe(silence(10), sample_rate=SAMPLE_RATE)

    assert client.files.deleted == ["files/abc"]


def test_retaining_uploads_can_be_turned_off_explicitly():
    client = FakeClient([interaction([("haan", 0.0, 0.2)])])
    asr = GoogleTranscribeASR(client=client, inline_limit_bytes=1_024,
                              delete_uploads=False)

    asr.transcribe(silence(10), sample_rate=SAMPLE_RATE)

    assert client.files.deleted == []


def test_long_calls_are_chunked_onto_one_timeline():
    # Two chunks of one minute each. The second chunk's model output starts from zero,
    # as every request does, and has to be shifted onto the call's timeline: a finding
    # whose span points 60 seconds too early is not evidence of anything.
    client = FakeClient([
        interaction([("pehla", 0.5, 1.0)], in_tokens=1_000, out_tokens=200),
        interaction([("doosra", 2.0, 2.5)], in_tokens=900, out_tokens=180),
    ])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=60)

    result = asr.transcribe(silence(90), sample_rate=SAMPLE_RATE)

    assert len(client.interactions.requests) == 2
    assert result.text == "pehla doosra"
    assert [(w.text, w.start_ms) for w in result.words] == [
        ("pehla", 500),
        ("doosra", 62_000),
    ]
    # Token counts accumulate across chunks, or a chunked call would under-report its
    # cost by however many chunks it took.
    assert (result.input_tokens, result.output_tokens) == (1_900, 380)


def test_final_short_chunk_keeps_an_exact_offset():
    # 45 s split at 30 s: the tail is half a chunk, so an offset derived from the
    # chunk index rather than the byte position would be wrong.
    client = FakeClient([
        interaction([("ek", 0.0, 0.3)]),
        interaction([("do", 1.0, 1.3)]),
    ])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=30)

    result = asr.transcribe(silence(45), sample_rate=SAMPLE_RATE)

    assert [w.start_ms for w in result.words] == [0, 31_000]


def test_empty_audio_makes_no_request():
    client = FakeClient([])
    result = GoogleTranscribeASR(client=client).transcribe(b"", sample_rate=SAMPLE_RATE)

    assert client.interactions.requests == []
    assert result.text == ""
    assert result.words == []


def test_custom_vocabulary_and_timestamps_are_refused_at_construction():
    # The API rejects the combination. Failing here rather than per call means a
    # deployment cannot lose its evidence spans to a 400 nobody reads.
    with pytest.raises(ValueError, match="custom_vocabulary"):
        GoogleTranscribeASR(client=FakeClient([]), custom_vocabulary=("Acme Recovery",))


def test_custom_vocabulary_is_sent_when_timestamps_are_given_up_deliberately():
    client = FakeClient([interaction([], text="pandrah hazaar")])
    asr = GoogleTranscribeASR(client=client, word_timestamps=False,
                              custom_vocabulary=("Acme Recovery", "EMI"))

    result = asr.transcribe(silence(5), sample_rate=SAMPLE_RATE)

    config = client.interactions.requests[0]["generation_config"]["transcription_config"]
    assert config["custom_vocabulary"] == ["Acme Recovery", "EMI"]
    assert "mode" not in config
    assert result.text == "pandrah hazaar"
    assert result.words == []


def test_tamil_is_refused_with_the_reason():
    # ta-IN is not in the model's supported locales. An unsupported hint degrades
    # rather than failing, and Hindi output for a Tamil call reaches a bank as a
    # missed flag, so this has to be an error and it has to say why.
    with pytest.raises(ValueError, match="Tamil is not among"):
        GoogleTranscribeASR(client=FakeClient([]), language_hints=("ta-IN",))

    asr = GoogleTranscribeASR(client=FakeClient([]))
    with pytest.raises(ValueError, match="ta-IN"):
        asr.transcribe(silence(5), sample_rate=SAMPLE_RATE, language_hint="ta-IN")


@pytest.mark.parametrize("code", ["hi-IN", "mr-IN", "te-IN", "en-IN"])
def test_the_languages_this_product_ships_into_are_accepted(code):
    client = FakeClient([interaction([("haan", 0.0, 0.2)])])
    result = GoogleTranscribeASR(client=client).transcribe(
        silence(5), sample_rate=SAMPLE_RATE, language_hint=code
    )
    assert result.language == code


def test_no_language_hint_leaves_detection_to_the_model():
    client = FakeClient([interaction([("haan", 0.0, 0.2)])])
    result = GoogleTranscribeASR(client=client).transcribe(
        silence(5), sample_rate=SAMPLE_RATE
    )

    config = client.interactions.requests[0]["generation_config"]["transcription_config"]
    assert config["language_codes"] == []
    # The API does not report what it settled on, so claiming a language would be a
    # guess recorded as a fact.
    assert result.language == "und"


def test_chunk_length_above_the_documented_ceiling_is_refused():
    with pytest.raises(ValueError, match="1800 seconds"):
        GoogleTranscribeASR(client=FakeClient([]), max_chunk_s=2_400)
    # Without timestamps the ceiling is an hour, so the same value is fine.
    GoogleTranscribeASR(client=FakeClient([]), max_chunk_s=2_400, word_timestamps=False)


def test_a_word_without_a_timing_is_an_error_rather_than_a_zero():
    response = interaction([("pandrah", 1.0, 1.4)])
    del response["steps"][0]["content"][0]["annotations"][0]["end_offset"]
    asr = GoogleTranscribeASR(client=FakeClient([response]))

    # Defaulting to zero would put the word at the start of the call and quietly
    # widen every span that covers it.
    with pytest.raises(GoogleTranscribeError, match="missing a timing offset"):
        asr.transcribe(silence(5), sample_rate=SAMPLE_RATE)


def test_result_converts_to_a_channel_transcript_that_can_be_quoted():
    client = FakeClient([
        interaction([("hum", 1.0, 1.3), ("police", 1.3, 1.8), ("case", 1.8, 2.2)])
    ])
    result = GoogleTranscribeASR(client=client).transcribe(
        silence(10), sample_rate=SAMPLE_RATE, language_hint="hi-IN"
    )

    ct = result.to_channel_transcript(Channel.NEAR)
    assert ct.channel is Channel.NEAR
    assert ct.provider_version == "gemini-3.5-transcribe"
    # The span primitive the rule engine builds evidence from has to work off these
    # timings, which is the whole reason this model was chosen over chirp_3.
    assert ct.span_text(1_250, 1_900) == "hum police case"
    assert ct.span_text(1_000, 1_200) == "hum"
