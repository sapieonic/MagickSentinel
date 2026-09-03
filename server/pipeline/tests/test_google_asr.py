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
import struct
from types import SimpleNamespace

import pytest

from sentinel_pipeline.models import Channel
from sentinel_pipeline.providers.google import (
    SUPPORTED_INDIC_LANGUAGES,
    GoogleTranscribeASR,
    GoogleTranscribeError,
    _field,
    _offset_ms,
    _parse,
)

SAMPLE_RATE = 16_000
BYTES_PER_SECOND = SAMPLE_RATE * 2


class UpstreamFailure(Exception):
    """Stands in for whatever the SDK raises when the API call itself fails."""


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


def annotation(text: str, start: object, end: object, *, kind: str = "word_info") -> dict:
    return {"type": kind, "text": text, "start_offset": start, "end_offset": end}


def block(*annotations: dict, text: str = "", kind: str | None = "text") -> dict:
    return {"type": kind, "text": text, "annotations": list(annotations)}


def response(*steps: dict, output_text: object = None, usage_key: str = "usage_metadata",
             usage: object = None) -> dict:
    """A response assembled block by block, for the parser's own edge cases."""
    out: dict = {"steps": list(steps)}
    if output_text is not None:
        out["output_text"] = output_text
    if usage is not None:
        out[usage_key] = usage
    return out


class FakeInteractions:
    def __init__(self, responses: list, events: list[str] | None = None) -> None:
        self.responses = responses
        self.requests: list[dict] = []
        self.events = events if events is not None else []

    def create(self, **kwargs) -> dict:
        self.requests.append(kwargs)
        self.events.append("create")
        if not self.responses:
            raise AssertionError("the adapter made more requests than the test queued")
        queued = self.responses.pop(0)
        if isinstance(queued, BaseException):
            raise queued
        return queued


class FakeFiles:
    def __init__(self, events: list[str] | None = None) -> None:
        self.uploaded: list[int] = []
        self.payloads: list[bytes] = []
        self.deleted: list[str] = []
        self.events = events if events is not None else []
        #: Set to ``None`` to model an upload result that reports no name.
        self.name: str | None = "files/abc"
        #: Set to ``False`` to model an upload result that reports no MIME type.
        self.report_mime = True
        #: Set to an exception to model the Files API refusing the deletion.
        self.delete_error: BaseException | None = None

    def upload(self, *, file, config) -> dict:
        payload = file.read()
        self.uploaded.append(len(payload))
        self.payloads.append(payload)
        self.events.append("upload")
        result: dict = {"uri": "files/abc"}
        if self.name is not None:
            result["name"] = self.name
        if self.report_mime:
            result["mime_type"] = config["mime_type"]
        return result

    def delete(self, *, name: str) -> None:
        self.events.append("delete")
        if self.delete_error is not None:
            raise self.delete_error
        self.deleted.append(name)


class FakeClient:
    def __init__(self, responses: list) -> None:
        self.events: list[str] = []
        self.interactions = FakeInteractions(responses, self.events)
        self.files = FakeFiles(self.events)


def silence(seconds: float) -> bytes:
    return b"\x00" * int(seconds * BYTES_PER_SECOND)


def one_word(client: FakeClient) -> dict:
    """The first transcription config the adapter sent."""
    return client.interactions.requests[0]["generation_config"]["transcription_config"]


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
    with pytest.raises(ValueError, match="Tamil is not among"):
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
    response_ = interaction([("pandrah", 1.0, 1.4)])
    del response_["steps"][0]["content"][0]["annotations"][0]["end_offset"]
    asr = GoogleTranscribeASR(client=FakeClient([response_]))

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


# --------------------------------------------------------------- duration parsing


@pytest.mark.parametrize(("value", "expected"), [
    ("0.100s", 100),
    ("12s", 12_000),
    ("0s", 0),
    (".5s", 500),
    ("  1.5s  ", 1_500),
    ("3600.5s", 3_600_500),
    ("7200s", 7_200_000),
    (12, 12_000),
    (0, 0),
    (0.1, 100),
    (1.4, 1_400),
    (3_600.0, 3_600_000),
])
def test_every_documented_duration_form_parses_to_whole_milliseconds(value, expected):
    assert _offset_ms(value) == expected


def test_an_hour_long_call_keeps_millisecond_resolution_at_its_end():
    # A one-hour call is inside the un-timestamped ceiling, and a span quoted from its
    # last minute has to be as precise as one quoted from its first.
    assert _offset_ms("3599.999s") == 3_599_999
    assert _offset_ms("3599.999") == 3_599_999


def test_a_missing_offset_is_an_error_rather_than_a_zero():
    with pytest.raises(GoogleTranscribeError, match="missing a timing offset"):
        _offset_ms(None)


@pytest.mark.parametrize("value", ["abc", "", "s", "1.2.3s", "0..1s", "1,5s", [], {}])
def test_an_unparseable_offset_is_refused_rather_than_guessed(value):
    # Salvaging a number out of junk would put a word somewhere nobody can defend.
    # Refusing the whole transcript is the auditable outcome.
    with pytest.raises(GoogleTranscribeError, match="unparseable timing offset"):
        _offset_ms(value)


def test_a_zero_offset_is_a_timing_and_not_a_missing_one():
    # A word at the very start of a chunk is the common case at every chunk boundary;
    # confusing its zero with "no timing" would fail every long call.
    assert _offset_ms("0s") == 0
    assert _offset_ms(0) == 0
    assert _offset_ms(0.0) == 0


@pytest.mark.parametrize(("value", "expected"), [
    ("0.1005s", 100),
    ("0.0015s", 2),
    ("0.0025s", 2),
    ("0.1015s", 102),
])
def test_sub_millisecond_offsets_round_the_way_python_rounds(value, expected):
    # Pinned deliberately: the half-millisecond case decides which word an evidence
    # span starts on, so the behaviour has to be visible in a diff rather than
    # discovered from a reviewer disputing a quote. round() is banker's rounding, so
    # 0.0015s and 0.0025s both land on 2 ms.
    assert _offset_ms(value) == expected


@pytest.mark.parametrize("value", ["-1.5s", -2, "-0.001s", -0.5])
def test_a_negative_offset_is_refused(value):
    # A word cannot start before the call did. Accepting one would put a finding's
    # evidence span at a timestamp that does not exist in the audio, which a reviewer
    # cannot play back and so cannot defend.
    with pytest.raises(GoogleTranscribeError, match="negative timing offset"):
        _offset_ms(value)


def test_a_zero_offset_is_not_mistaken_for_a_negative_one():
    assert _offset_ms("0s") == 0
    assert _offset_ms(0) == 0


# ----------------------------------------------------------------- field accessor


@pytest.mark.parametrize("obj", [
    {"uri": "files/abc"},
    SimpleNamespace(uri="files/abc"),
])
def test_a_field_reads_the_same_off_a_dict_and_off_an_sdk_object(obj):
    assert _field(obj, "uri") == "files/abc"


@pytest.mark.parametrize("obj", [{}, {"other": 1}, SimpleNamespace(), SimpleNamespace(other=1)])
def test_an_absent_field_reads_as_none(obj):
    assert _field(obj, "uri") is None


@pytest.mark.parametrize("value", [0, 0.0, "", [], {}, False])
def test_a_falsy_field_value_is_not_mistaken_for_an_absent_one(value):
    # ``start_offset: 0`` is the timing of the first word of every chunk. Reading it
    # as absence would turn a correct response into a failed transcript.
    assert _field({"start_offset": value}, "start_offset") is value
    assert _field(SimpleNamespace(start_offset=value), "start_offset") is value


# ------------------------------------------------------------------ response parsing


def test_every_step_and_content_block_contributes_its_words():
    parsed = _parse(
        response(
            {"content": [
                block(annotation("pandrah", "1.0s", "1.4s"), text="pandrah"),
                block(annotation("hazaar", "1.4s", "1.9s"), text="hazaar"),
            ]},
            {"content": [block(annotation("rupaye", "2.0s", "2.4s"), text="rupaye")]},
        ),
        offset_ms=0,
    )

    assert [w.text for w in parsed[1]] == ["pandrah", "hazaar", "rupaye"]
    assert parsed[0] == "pandrah hazaar rupaye"


def test_a_content_block_that_is_not_text_is_skipped_whole():
    parsed = _parse(
        response({"content": [
            block(annotation("ignored", "1.0s", "1.4s"), text="thumbnail", kind="image"),
            block(annotation("kal", "2.0s", "2.4s"), text="kal"),
        ]}),
        offset_ms=0,
    )

    assert [w.text for w in parsed[1]] == ["kal"]
    assert parsed[0] == "kal"


def test_a_content_block_with_no_declared_type_is_read_as_text():
    parsed = _parse(
        response({"content": [block(annotation("kal", "2.0s", "2.4s"), text="kal", kind=None)]}),
        offset_ms=0,
    )
    assert [w.text for w in parsed[1]] == ["kal"]


def test_annotations_that_are_not_word_info_are_skipped():
    # Citations and safety annotations share the annotation list with word_info. A
    # parser that took them all would invent words nobody said.
    parsed = _parse(
        response({"content": [block(
            annotation("https://example.test", "0s", "1s", kind="citation"),
            annotation("kal", "2.0s", "2.4s"),
            annotation("blocked", "3s", "4s", kind="safety_rating"),
            text="kal",
        )]}),
        offset_ms=0,
    )

    assert [w.text for w in parsed[1]] == ["kal"]


def test_a_word_annotation_with_no_text_is_skipped():
    parsed = _parse(
        response({"content": [block(
            annotation("", "0s", "1s"),
            annotation("kal", "2.0s", "2.4s"),
            text="kal",
        )]}),
        offset_ms=0,
    )
    assert [w.text for w in parsed[1]] == ["kal"]


def test_a_whitespace_only_word_annotation_is_dropped():
    # An empty-text Word is a phantom token: it shifts the rule engine's n-gram
    # windows and widens whatever span_text returns around it, so it must never reach
    # the word list.
    parsed = _parse(
        response({"content": [block(annotation("  ", "0.0s", "0.1s"), text=" ")]}),
        offset_ms=0,
    )
    assert parsed[1] == []


def test_a_bit_depth_that_is_not_whole_bytes_is_refused():
    # Floor division on the byte rate would truncate it and drift every later chunk's
    # offset, which lands in evidence spans.
    asr = GoogleTranscribeASR(client=FakeClient([]), bits_per_sample=4)
    with pytest.raises(ValueError, match="whole number of bytes"):
        asr.transcribe(b"\x00" * 100, sample_rate=16_000)


def test_word_text_is_stripped_of_the_padding_the_model_adds():
    # Rule matching is n-gram based over these tokens, so " police " and "police" must
    # not be two different words.
    parsed = _parse(
        response({"content": [block(annotation(" police ", "1.0s", "1.4s"), text=" police ")]}),
        offset_ms=0,
    )
    assert [w.text for w in parsed[1]] == ["police"]


def test_the_transcript_falls_back_to_the_per_step_text_when_output_text_is_absent():
    parsed = _parse(
        response(
            {"content": [block(text="pandrah hazaar")]},
            {"content": [block(text="rupaye baaki hai")]},
        ),
        offset_ms=0,
    )
    assert parsed[0] == "pandrah hazaar rupaye baaki hai"


def test_an_empty_output_text_falls_back_to_the_per_step_text():
    parsed = _parse(
        response({"content": [block(text="pandrah hazaar")]}, output_text=""),
        offset_ms=0,
    )
    assert parsed[0] == "pandrah hazaar"


def test_a_whitespace_only_output_text_falls_back_to_the_per_step_text():
    # Whitespace is truthy, so a truthiness test here would discard the transcript and
    # leave a result with spans but nothing to quote — the worst combination for a
    # compliance record, because the finding looks traceable until someone opens it.
    parsed = _parse(
        response(
            {"content": [block(annotation("kal", "1.0s", "1.4s"), text="kal")]},
            output_text="   ",
        ),
        offset_ms=0,
    )
    assert parsed[0] == "kal"
    assert [w.text for w in parsed[1]] == ["kal"]


def test_output_text_is_preferred_over_the_per_step_text_when_both_are_present():
    parsed = _parse(
        response({"content": [block(text="per step")]}, output_text=" assembled "),
        offset_ms=0,
    )
    assert parsed[0] == "assembled"


def test_a_response_with_no_steps_yields_no_words():
    assert _parse({"output_text": "pandrah"}, offset_ms=0) == ("pandrah", [], 0, 0)


def test_a_content_block_with_no_annotations_yields_no_words():
    parsed = _parse(response({"content": [{"type": "text", "text": "kal"}]}), offset_ms=0)
    assert parsed[1] == []
    assert parsed[0] == "kal"


def test_a_step_with_no_content_yields_no_words():
    assert _parse(response({"type": "model_output"}), offset_ms=0) == ("", [], 0, 0)


@pytest.mark.parametrize("usage_key", ["usage_metadata", "usage"])
def test_token_counts_are_read_from_either_usage_shape(usage_key):
    # Token counts are the only cost record for this provider, so a shape the parser
    # does not recognise bills a floor for nothing.
    parsed = _parse(
        response({"content": [block(text="kal")]}, usage_key=usage_key,
                 usage={"total_input_tokens": 1_500, "total_output_tokens": 90}),
        offset_ms=0,
    )
    assert parsed[2:] == (1_500, 90)


def test_missing_usage_is_zero_tokens_rather_than_a_failed_transcript():
    # Zero means "not reported". Losing the transcript over an absent cost field would
    # trade a compliance record for an accounting one.
    parsed = _parse(response({"content": [block(text="kal")]}), offset_ms=0)
    assert parsed[2:] == (0, 0)


def test_partial_usage_counts_the_half_that_arrived():
    parsed = _parse(
        response({"content": [block(text="kal")]}, usage={"total_input_tokens": 700}),
        offset_ms=0,
    )
    assert parsed[2:] == (700, 0)


@pytest.mark.parametrize(("raw", "expected"), [("1500", 1_500), (1_500.0, 1_500), (12.9, 12)])
def test_token_counts_are_coerced_to_integers(raw, expected):
    parsed = _parse(
        response({"content": [block(text="kal")]},
                 usage={"total_input_tokens": raw, "total_output_tokens": raw}),
        offset_ms=0,
    )
    assert parsed[2:] == (expected, expected)


def test_the_parser_shifts_both_ends_of_every_word_by_the_chunk_offset():
    # Shifting only start_ms would leave end_ms pointing into a previous chunk, and
    # span_text() would return the wrong words for a finding in a long call.
    parsed = _parse(
        response({"content": [block(annotation("kal", "1.0s", "1.4s"), text="kal")]}),
        offset_ms=60_000,
    )
    assert [(w.start_ms, w.end_ms) for w in parsed[1]] == [(61_000, 61_400)]


def test_an_sdk_style_object_response_parses_like_the_documented_json():
    # The installed SDK returns typed objects and the REST API returns JSON; both have
    # to reach the same transcript or the adapter is pinned to one of them.
    payload = SimpleNamespace(
        output_text="pandrah hazaar",
        steps=[SimpleNamespace(content=[SimpleNamespace(
            type="text",
            text="pandrah hazaar",
            annotations=[
                SimpleNamespace(type="word_info", text="pandrah",
                                start_offset="1.0s", end_offset="1.4s"),
                SimpleNamespace(type="word_info", text="hazaar",
                                start_offset="1.4s", end_offset="1.9s"),
            ],
        )])],
        usage_metadata=SimpleNamespace(total_input_tokens=42, total_output_tokens=7),
    )

    client = FakeClient([payload])
    result = GoogleTranscribeASR(client=client).transcribe(
        silence(5), sample_rate=SAMPLE_RATE, language_hint="hi-IN"
    )

    assert result.text == "pandrah hazaar"
    assert [(w.text, w.start_ms, w.end_ms) for w in result.words] == [
        ("pandrah", 1_000, 1_400),
        ("hazaar", 1_400, 1_900),
    ]
    assert (result.input_tokens, result.output_tokens) == (42, 7)


# ------------------------------------------------------------- chunking arithmetic


def test_audio_exactly_one_chunk_long_makes_exactly_one_request():
    client = FakeClient([interaction([("ek", 0.0, 0.3)])])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=30)

    asr.transcribe(silence(30), sample_rate=SAMPLE_RATE)

    assert len(client.interactions.requests) == 1


def test_audio_exactly_two_chunks_long_makes_exactly_two_requests():
    client = FakeClient([
        interaction([("ek", 0.0, 0.3)]),
        interaction([("do", 0.0, 0.3)]),
    ])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=30)

    result = asr.transcribe(silence(60), sample_rate=SAMPLE_RATE)

    assert len(client.interactions.requests) == 2
    assert [w.start_ms for w in result.words] == [0, 30_000]


def test_one_byte_past_a_chunk_boundary_costs_a_whole_extra_request():
    # A single trailing byte is still a request, and its words still have to be
    # offset: the request count is also what the provider bills.
    client = FakeClient([
        interaction([("ek", 0.0, 0.3)]),
        interaction([("do", 0.0, 0.0)]),
    ])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=30)

    result = asr.transcribe(silence(30) + b"\x01", sample_rate=SAMPLE_RATE)

    assert len(client.interactions.requests) == 2
    assert len(base64.b64decode(client.interactions.requests[1]["input"][0]["data"])) == 1
    assert [w.start_ms for w in result.words] == [0, 30_000]


def test_a_trailing_partial_sample_does_not_shift_the_call_timeline():
    # 45 s plus one stray byte: the odd byte count must not round the last chunk's
    # offset, because every finding in that chunk quotes from it.
    client = FakeClient([
        interaction([("ek", 0.0, 0.3)]),
        interaction([("do", 1.0, 1.3)]),
    ])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=30)

    result = asr.transcribe(silence(45) + b"\x01", sample_rate=SAMPLE_RATE)

    assert len(client.interactions.requests) == 2
    sent = [len(base64.b64decode(r["input"][0]["data"]))
            for r in client.interactions.requests]
    assert sent == [30 * BYTES_PER_SECOND, 15 * BYTES_PER_SECOND + 1]
    assert [(w.start_ms, w.end_ms) for w in result.words] == [(0, 300), (31_000, 31_300)]


def test_an_odd_bytes_per_second_still_lands_chunks_on_whole_seconds():
    # 8-bit audio at 8,001 Hz makes the chunk size an odd number of bytes, which is
    # where an offset computed from the chunk index instead of the byte position
    # drifts. Every chunk here still starts on an exact second.
    client = FakeClient([interaction([("ek", 0.0, 0.1)]) for _ in range(3)])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=1, bits_per_sample=8)

    result = asr.transcribe(b"\x00" * (3 * 8_001), sample_rate=8_001)

    assert len(client.interactions.requests) == 3
    assert [w.start_ms for w in result.words] == [0, 1_000, 2_000]


def test_a_very_small_chunk_length_keeps_every_word_in_call_order():
    client = FakeClient([interaction([(f"w{i}", 0.2, 0.4)]) for i in range(5)])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=1)

    result = asr.transcribe(silence(5), sample_rate=SAMPLE_RATE)

    assert len(client.interactions.requests) == 5
    starts = [w.start_ms for w in result.words]
    assert starts == [200, 1_200, 2_200, 3_200, 4_200]
    assert starts == sorted(starts)
    assert all(w.end_ms > w.start_ms for w in result.words)


@pytest.mark.parametrize(("sample_rate", "channels", "bits"), [
    (8_000, 1, 16),
    (16_000, 1, 16),
    (44_100, 1, 16),
    (16_000, 2, 16),
    (16_000, 1, 8),
    (44_100, 2, 16),
])
def test_chunk_size_and_offsets_follow_the_declared_audio_format(sample_rate, channels, bits):
    # The offsets in an evidence span are derived from this arithmetic, so a format
    # the adapter mis-measures moves every quote in a long call.
    bps = sample_rate * channels * bits // 8
    client = FakeClient([interaction([("ek", 0.5, 0.9)]) for _ in range(3)])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=1, channels=channels,
                              bits_per_sample=bits, inline_limit_bytes=64 * 1024 * 1024)

    result = asr.transcribe(b"\x00" * (2 * bps + 5), sample_rate=sample_rate)

    sent = [len(base64.b64decode(r["input"][0]["data"]))
            for r in client.interactions.requests]
    assert sent == [bps, bps, 5]
    assert [(w.start_ms, w.end_ms) for w in result.words] == [
        (500, 900), (1_500, 1_900), (2_500, 2_900)
    ]
    part = client.interactions.requests[0]["input"][0]
    assert (part["sample_rate"], part["channels"]) == (sample_rate, channels)


@pytest.mark.parametrize(("sample_rate", "channels", "bits"), [
    (0, 1, 16),
    (16_000, 0, 16),
    (16_000, 1, 0),
    (-16_000, 1, 16),
    (16_000, -1, 16),
])
def test_an_audio_format_with_no_usable_byte_rate_is_refused(sample_rate, channels, bits):
    # A zero byte rate would mean a zero-length chunk and an offset division by zero;
    # a negative one would run the timeline backwards.
    asr = GoogleTranscribeASR(client=FakeClient([]), channels=channels,
                              bits_per_sample=bits)

    with pytest.raises(ValueError, match="must be positive"):
        asr.transcribe(b"\x00" * 1_000, sample_rate=sample_rate)


# --------------------------------------------------------- construction validation


def test_a_thousand_custom_terms_are_allowed_and_one_more_is_not():
    terms = tuple(f"term-{i}" for i in range(1_000))
    GoogleTranscribeASR(client=FakeClient([]), word_timestamps=False,
                        custom_vocabulary=terms)

    with pytest.raises(ValueError, match="capped at 1,000 terms"):
        GoogleTranscribeASR(client=FakeClient([]), word_timestamps=False,
                            custom_vocabulary=terms + ("one-too-many",))


@pytest.mark.parametrize("max_chunk_s", [0, -1, -1_500])
def test_a_non_positive_chunk_length_is_refused(max_chunk_s):
    with pytest.raises(ValueError, match="must be between 1 and"):
        GoogleTranscribeASR(client=FakeClient([]), max_chunk_s=max_chunk_s)


@pytest.mark.parametrize(("max_chunk_s", "word_timestamps", "accepted"), [
    (1, True, True),
    (1_800, True, True),
    (1_801, True, False),
    (3_600, True, False),
    (1_800, False, True),
    (3_600, False, True),
    (3_601, False, False),
])
def test_the_documented_chunk_ceilings_are_inclusive(max_chunk_s, word_timestamps, accepted):
    # The ceiling halves once word timestamps are on, and the boundary value itself is
    # legal: a request at exactly the documented limit is one the API accepts.
    if accepted:
        asr = GoogleTranscribeASR(client=FakeClient([]), max_chunk_s=max_chunk_s,
                                  word_timestamps=word_timestamps)
        assert asr.max_chunk_s == max_chunk_s
    else:
        with pytest.raises(ValueError, match="must be between 1 and"):
            GoogleTranscribeASR(client=FakeClient([]), max_chunk_s=max_chunk_s,
                                word_timestamps=word_timestamps)


def test_configuration_is_validated_before_the_sdk_is_imported():
    # No client is passed, so reaching the import would need google-genai installed.
    # A misconfiguration has to be reported as a misconfiguration on any deployment.
    with pytest.raises(ValueError, match="must be between 1 and"):
        GoogleTranscribeASR(max_chunk_s=0)


def test_the_result_records_the_model_it_was_actually_produced_by():
    # Quality trends across a model change are only meaningful if results from before
    # and after are distinguishable.
    client = FakeClient([interaction([("haan", 0.0, 0.2)])])
    asr = GoogleTranscribeASR(client=client, model="gemini-3.5-transcribe-002")

    result = asr.transcribe(silence(5), sample_rate=SAMPLE_RATE)

    assert asr.version == "gemini-3.5-transcribe-002"
    assert result.provider == "google-transcribe"
    assert result.provider_version == "gemini-3.5-transcribe-002"
    assert client.interactions.requests[0]["model"] == "gemini-3.5-transcribe-002"


# ------------------------------------------------------------ language validation


@pytest.mark.parametrize("code", sorted(SUPPORTED_INDIC_LANGUAGES))
def test_every_supported_locale_is_accepted_on_both_validation_paths(code):
    GoogleTranscribeASR(client=FakeClient([]), language_hints=(code,))

    client = FakeClient([interaction([("haan", 0.0, 0.2)])])
    result = GoogleTranscribeASR(client=client).transcribe(
        silence(5), sample_rate=SAMPLE_RATE, language_hint=code
    )

    assert result.language == code
    assert one_word(client)["language_codes"] == [code]


@pytest.mark.parametrize("code", ["ur-IN", "sd-IN", "ks-IN", "xx-IN"])
def test_an_unsupported_indian_locale_is_refused_without_the_tamil_note(code):
    with pytest.raises(ValueError, match="not a supported gemini-3.5-transcribe locale") as err:
        GoogleTranscribeASR(client=FakeClient([]), language_hints=(code,))
    assert "Tamil" not in str(err.value)

    asr = GoogleTranscribeASR(client=FakeClient([]))
    with pytest.raises(ValueError, match=code):
        asr.transcribe(silence(5), sample_rate=SAMPLE_RATE, language_hint=code)


@pytest.mark.parametrize("code", ["en-US", "fr-FR", "ja-JP", "en-GB"])
def test_a_non_indian_locale_is_left_to_the_model_to_police(code):
    # The supported set here covers only the languages this product ships into;
    # rejecting the model's other 85 locales would be this adapter overreaching.
    client = FakeClient([interaction([("yes", 0.0, 0.2)])])
    result = GoogleTranscribeASR(client=client, language_hints=(code,)).transcribe(
        silence(5), sample_rate=SAMPLE_RATE
    )

    assert one_word(client)["language_codes"] == [code]
    assert result.language == code


@pytest.mark.parametrize(("hints", "expected"), [
    (("hi-IN", "ta-IN"), "Tamil is not among"),
    (("hi-IN", "mr-IN", "ur-IN"), "ur-IN"),
    (("en-US", "ta-IN"), "Tamil is not among"),
])
def test_every_language_hint_is_validated_not_only_the_first(hints, expected):
    with pytest.raises(ValueError, match=expected):
        GoogleTranscribeASR(client=FakeClient([]), language_hints=hints)


def test_all_constructor_hints_are_sent_and_the_first_is_the_reported_language():
    client = FakeClient([interaction([("haan", 0.0, 0.2)])])
    asr = GoogleTranscribeASR(client=client, language_hints=("hi-IN", "en-IN"))

    result = asr.transcribe(silence(5), sample_rate=SAMPLE_RATE)

    assert one_word(client)["language_codes"] == ["hi-IN", "en-IN"]
    assert result.language == "hi-IN"


def test_a_per_call_hint_replaces_the_constructed_default_entirely():
    # Provider selection is per tenant but language is per call; sending the
    # deployment default anyway would transcribe a Marathi call as Hindi.
    client = FakeClient([interaction([("haan", 0.0, 0.2)])])
    asr = GoogleTranscribeASR(client=client, language_hints=("hi-IN", "en-IN"))

    result = asr.transcribe(silence(5), sample_rate=SAMPLE_RATE, language_hint="mr-IN")

    assert one_word(client)["language_codes"] == ["mr-IN"]
    assert result.language == "mr-IN"


# ------------------------------------------------------------------- files api path


def test_the_uploaded_wav_header_declares_the_real_audio_format():
    # L16 carries no rate of its own. If the header lied about the rate or the channel
    # count, the model would transcribe the audio at the wrong speed and every word
    # timing derived from it would be wrong.
    client = FakeClient([interaction([("theek", 0.0, 0.4)])])
    asr = GoogleTranscribeASR(client=client, inline_limit_bytes=100, channels=2,
                              bits_per_sample=8)
    audio = b"\x7f" * 88_200  # one second at 44,100 Hz, 8-bit stereo

    asr.transcribe(audio, sample_rate=44_100)

    payload = client.files.payloads[0]
    assert payload[:4] == b"RIFF"
    assert payload[8:16] == b"WAVEfmt "
    assert struct.unpack_from("<I", payload, 4)[0] == 36 + len(audio)
    size, fmt, channels, rate, byte_rate, block_align, bits = struct.unpack_from(
        "<IHHIIHH", payload, 16
    )
    assert (size, fmt) == (16, 1)
    assert (channels, rate, bits) == (2, 44_100, 8)
    assert byte_rate == 44_100 * 2 * 8 // 8
    assert block_align == 2
    assert payload[36:40] == b"data"
    assert struct.unpack_from("<I", payload, 40)[0] == len(audio)
    assert payload[44:] == audio


def test_each_chunk_uploads_and_deletes_its_own_file():
    # Deletion has to happen per chunk rather than at the end of the call: a failure
    # part-way through a long call must not leave earlier chunks sitting in a
    # third-party store past the tenant's retention period.
    client = FakeClient([
        interaction([("ek", 0.0, 0.3)]),
        interaction([("do", 0.0, 0.3)]),
    ])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=1, inline_limit_bytes=1_024)

    asr.transcribe(silence(2), sample_rate=SAMPLE_RATE)

    assert client.events == ["upload", "create", "delete", "upload", "create", "delete"]
    assert client.files.uploaded == [BYTES_PER_SECOND + 44] * 2
    assert client.files.deleted == ["files/abc", "files/abc"]


def test_a_deletion_that_fails_does_not_lose_the_transcript():
    # The Files API expires the upload on its own within 48 hours, so a failed
    # deletion delays it rather than preventing it. Losing a completed transcript over
    # that would be the worse trade.
    client = FakeClient([interaction([("theek", 0.0, 0.4)])])
    client.files.delete_error = UpstreamFailure("permission denied")
    asr = GoogleTranscribeASR(client=client, inline_limit_bytes=1_024)

    result = asr.transcribe(silence(5), sample_rate=SAMPLE_RATE)

    assert result.text == "theek"
    assert client.events == ["upload", "create", "delete"]
    assert client.files.deleted == []


def test_an_upload_that_reports_no_name_skips_deletion_without_crashing():
    client = FakeClient([interaction([("theek", 0.0, 0.4)])])
    client.files.name = None
    asr = GoogleTranscribeASR(client=client, inline_limit_bytes=1_024)

    result = asr.transcribe(silence(5), sample_rate=SAMPLE_RATE)

    assert result.text == "theek"
    assert client.events == ["upload", "create"]


def test_an_upload_that_reports_no_mime_type_falls_back_to_raw_l16():
    client = FakeClient([interaction([("theek", 0.0, 0.4)])])
    client.files.report_mime = False
    asr = GoogleTranscribeASR(client=client, inline_limit_bytes=1_024)

    asr.transcribe(silence(5), sample_rate=SAMPLE_RATE)

    part = client.interactions.requests[0]["input"][0]
    assert part["mime_type"] == "audio/l16"
    assert part["uri"] == "files/abc"


def test_a_chunk_exactly_at_the_inline_limit_is_inlined():
    audio = silence(1)
    client = FakeClient([interaction([("ek", 0.0, 0.3)])])
    asr = GoogleTranscribeASR(client=client, inline_limit_bytes=len(audio))

    asr.transcribe(audio, sample_rate=SAMPLE_RATE)

    assert client.files.uploaded == []
    assert base64.b64decode(client.interactions.requests[0]["input"][0]["data"]) == audio


def test_a_chunk_one_byte_over_the_inline_limit_is_uploaded():
    audio = silence(1)
    client = FakeClient([interaction([("ek", 0.0, 0.3)])])
    asr = GoogleTranscribeASR(client=client, inline_limit_bytes=len(audio) - 1)

    asr.transcribe(audio, sample_rate=SAMPLE_RATE)

    assert client.files.uploaded == [len(audio) + 44]
    assert "data" not in client.interactions.requests[0]["input"][0]


def test_the_inline_path_never_touches_the_files_api():
    client = FakeClient([interaction([("ek", 0.0, 0.3)])])
    GoogleTranscribeASR(client=client).transcribe(silence(1), sample_rate=SAMPLE_RATE)

    assert client.events == ["create"]


# ------------------------------------------------------------ tokens and failures


def test_token_counts_accumulate_across_many_chunks():
    # ASR is the largest recurring cost in the pipeline; a chunked call that reports
    # only its last chunk would under-bill by however many chunks it took.
    client = FakeClient([
        interaction([("w", 0.0, 0.1)], in_tokens=100 * i, out_tokens=10 * i)
        for i in range(1, 11)
    ])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=1)

    result = asr.transcribe(silence(10), sample_rate=SAMPLE_RATE)

    assert len(client.interactions.requests) == 10
    assert (result.input_tokens, result.output_tokens) == (5_500, 550)


def test_chunks_that_report_no_usage_do_not_reset_the_running_total():
    client = FakeClient([
        interaction([("ek", 0.0, 0.1)], in_tokens=400, out_tokens=40),
        response({"content": [block(annotation("do", "0.0s", "0.1s"), text="do")]}),
        interaction([("teen", 0.0, 0.1)], in_tokens=600, out_tokens=60),
    ])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=1)

    result = asr.transcribe(silence(3), sample_rate=SAMPLE_RATE)

    assert (result.input_tokens, result.output_tokens) == (1_000, 100)
    assert [w.text for w in result.words] == ["ek", "do", "teen"]


def test_an_error_from_the_api_is_not_swallowed():
    # The worker decides what an ASR failure means for the call (it stops it), so the
    # adapter must not turn a failure into a partial transcript nobody flagged.
    client = FakeClient([
        interaction([("ek", 0.0, 0.3)]),
        UpstreamFailure("503 from upstream"),
    ])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=30)

    with pytest.raises(UpstreamFailure, match="503 from upstream"):
        asr.transcribe(silence(60), sample_rate=SAMPLE_RATE)


def test_a_failure_on_a_later_chunk_yields_no_transcript_at_all():
    client = FakeClient([
        interaction([("ek", 0.0, 0.3)]),
        UpstreamFailure("truncated"),
        interaction([("teen", 0.0, 0.3)]),
    ])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=1)

    with pytest.raises(UpstreamFailure):
        asr.transcribe(silence(3), sample_rate=SAMPLE_RATE)

    # The third chunk was never requested: a half-transcribed call must not look
    # complete to the compliance tier.
    assert len(client.interactions.requests) == 2


def test_chunks_that_transcribe_to_nothing_are_left_out_of_the_transcript():
    client = FakeClient([
        interaction([("ek", 0.0, 0.3)]),
        interaction([], text=""),
        interaction([("teen", 0.0, 0.3)]),
    ])
    asr = GoogleTranscribeASR(client=client, max_chunk_s=1)

    result = asr.transcribe(silence(3), sample_rate=SAMPLE_RATE)

    assert result.text == "ek teen"
