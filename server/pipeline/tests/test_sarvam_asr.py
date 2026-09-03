"""Tests for the Sarvam adapter.

Sarvam matters more than it did: it is the route for Tamil, which the default provider
cannot read at all, and the whole-floor exit if OPEN-4 comes back as strict India-only.
Both of those make it a path that has to work rather than a candidate sitting in a
drawer, which is what these tests are for.

A fake session stands in for ``requests``, so they run without the SDK and without
network.
"""

from __future__ import annotations

import pytest

from sentinel_pipeline.providers.sarvam import SarvamASR, _words

SAMPLE_RATE = 16_000


class FakeResponse:
    def __init__(self, payload: dict, *, status_error: Exception | None = None) -> None:
        self.payload = payload
        self.status_error = status_error

    def raise_for_status(self) -> None:
        if self.status_error is not None:
            raise self.status_error

    def json(self) -> dict:
        return self.payload


class FakeSession:
    def __init__(self, *responses: FakeResponse) -> None:
        self.responses = list(responses)
        self.calls: list[dict] = []

    def post(self, url, **kwargs):
        self.calls.append({"url": url, **kwargs})
        if not self.responses:
            raise AssertionError("the adapter made more requests than the test queued")
        return self.responses.pop(0)


def payload(*, transcript: str = "pandrah hazaar", words: list[str] | None = None,
            starts: list[float] | None = None, ends: list[float] | None = None,
            language: str = "hi-IN", probability: float | None = 0.94) -> dict:
    """A response in the shape the API documents: three parallel arrays."""
    body: dict = {
        "request_id": "req-1",
        "transcript": transcript,
        "language_code": language,
    }
    if probability is not None:
        body["language_probability"] = probability
    if words is not None:
        body["timestamps"] = {
            "words": words,
            "start_time_seconds": starts if starts is not None else [],
            "end_time_seconds": ends if ends is not None else [],
        }
    return body


def silence(seconds: float) -> bytes:
    return b"\x00" * int(seconds * SAMPLE_RATE * 2)


# --------------------------------------------------------------------- request shape


def test_uses_a_current_model_identifier():
    # saarika:* is gone from the API's model enum entirely, so the previous default
    # would have been rejected on every call rather than degrading quietly.
    asr = SarvamASR(api_key="k", session=FakeSession())
    assert asr.model == "saaras:v4"
    assert asr.version == "saaras:v4"


def test_sends_the_audio_as_wav_with_the_key_in_the_documented_header():
    session = FakeSession(FakeResponse(payload(words=["haan"], starts=[0.0], ends=[0.4])))
    SarvamASR(api_key="secret", session=session).transcribe(
        silence(5), sample_rate=SAMPLE_RATE, language_hint="ta-IN"
    )

    call = session.calls[0]
    assert call["url"] == "https://api.sarvam.ai/speech-to-text"
    assert call["headers"] == {"api-subscription-key": "secret"}
    name, handle, mime = call["files"]["file"]
    assert (name, mime) == ("call.wav", "audio/wav")
    body = handle.getvalue()
    assert body.startswith(b"RIFF")
    assert b"WAVEfmt " in body
    assert call["data"]["with_timestamps"] == "true"
    assert call["data"]["model"] == "saaras:v4"
    assert call["data"]["language_code"] == "ta-IN"


def test_no_language_hint_asks_for_detection_explicitly():
    # "unknown" is the API's auto-detect value. Omitting the field instead would leave
    # the choice to whatever the service defaults to, which is a different request.
    session = FakeSession(FakeResponse(payload(words=["haan"], starts=[0.0], ends=[0.4])))
    SarvamASR(api_key="k", session=session).transcribe(silence(5), sample_rate=SAMPLE_RATE)

    assert session.calls[0]["data"]["language_code"] == "unknown"


def test_mode_is_omitted_unless_asked_for():
    # The API documents mode as applying to saaras:v3 only. Sending a field the chosen
    # model ignores is how a silent behaviour change hides.
    session = FakeSession(FakeResponse(payload(words=[], starts=[], ends=[])))
    SarvamASR(api_key="k", session=session).transcribe(silence(5), sample_rate=SAMPLE_RATE)
    assert "mode" not in session.calls[0]["data"]

    session = FakeSession(FakeResponse(payload(words=[], starts=[], ends=[])))
    SarvamASR(api_key="k", session=session, mode="codemix").transcribe(
        silence(5), sample_rate=SAMPLE_RATE
    )
    assert session.calls[0]["data"]["mode"] == "codemix"


@pytest.mark.parametrize("rate", [8_000, 16_000, 44_100])
def test_the_wav_header_declares_the_rate_it_was_given(rate):
    session = FakeSession(FakeResponse(payload(words=[], starts=[], ends=[])))
    SarvamASR(api_key="k", session=session).transcribe(silence(1), sample_rate=rate)

    header = session.calls[0]["files"]["file"][1].getvalue()[:44]
    assert int.from_bytes(header[24:28], "little") == rate


# -------------------------------------------------------------------- parsing output


def test_parses_the_three_parallel_arrays_into_words():
    # The response carries words, start_time_seconds and end_time_seconds as separate
    # lists that line up by index — not a list of word objects.
    session = FakeSession(FakeResponse(payload(
        transcript="pandrah hazaar rupaye",
        words=["pandrah hazaar", "rupaye"],
        starts=[1.0, 1.9],
        ends=[1.9, 2.4],
    )))
    result = SarvamASR(api_key="k", session=session).transcribe(
        silence(10), sample_rate=SAMPLE_RATE
    )

    assert result.text == "pandrah hazaar rupaye"
    assert [(w.text, w.start_ms, w.end_ms) for w in result.words] == [
        ("pandrah hazaar", 1_000, 1_900),
        ("rupaye", 1_900, 2_400),
    ]
    assert result.provider == "sarvam"
    assert result.provider_version == "saaras:v4"
    assert result.language == "hi-IN"
    assert result.confidence == 0.94


def test_entries_span_phrases_rather_than_single_words():
    # Recorded rather than hidden: this is the reason Sarvam is the fallback and not
    # the default. An evidence span built from these is as wide as the phrase.
    session = FakeSession(FakeResponse(payload(
        words=["aap pandrah hazaar kal tak jama kar dijiye"], starts=[3.0], ends=[6.5],
    )))
    result = SarvamASR(api_key="k", session=session).transcribe(
        silence(10), sample_rate=SAMPLE_RATE
    )

    assert len(result.words) == 1
    assert result.words[0].end_ms - result.words[0].start_ms == 3_500


def test_missing_timestamps_object_yields_no_words_rather_than_an_error():
    session = FakeSession(FakeResponse(payload(transcript="haan ji")))
    result = SarvamASR(api_key="k", session=session).transcribe(
        silence(5), sample_rate=SAMPLE_RATE
    )

    assert result.text == "haan ji"
    assert result.words == []


def test_no_word_confidence_is_invented():
    # The API reports a language probability, not a per-word score. Synthesising one
    # would make an unverified span look reviewed.
    session = FakeSession(FakeResponse(payload(words=["haan"], starts=[0.0], ends=[0.3])))
    result = SarvamASR(api_key="k", session=session).transcribe(
        silence(5), sample_rate=SAMPLE_RATE
    )

    assert all(w.confidence is None for w in result.words)


def test_language_falls_back_to_the_hint_then_to_undetermined():
    session = FakeSession(FakeResponse({"transcript": "x"}))
    assert SarvamASR(api_key="k", session=session).transcribe(
        silence(1), sample_rate=SAMPLE_RATE, language_hint="mr-IN"
    ).language == "mr-IN"

    session = FakeSession(FakeResponse({"transcript": "x"}))
    # Never a guessed "hi": claiming a language the response did not report would put
    # a wrong language on a stored transcript.
    assert SarvamASR(api_key="k", session=session).transcribe(
        silence(1), sample_rate=SAMPLE_RATE
    ).language == "und"


def test_an_http_error_propagates():
    # The worker is what decides a failed channel is survivable; the adapter must not
    # absorb the failure and return an empty transcript that looks like a silent call.
    boom = RuntimeError("503")
    session = FakeSession(FakeResponse({}, status_error=boom))
    with pytest.raises(RuntimeError, match="503"):
        SarvamASR(api_key="k", session=session).transcribe(
            silence(5), sample_rate=SAMPLE_RATE
        )


# ------------------------------------------------------------------ _words directly


@pytest.mark.parametrize("timestamps", [None, {}, [], "nonsense", {"words": None}])
def test_words_tolerates_anything_that_is_not_the_documented_shape(timestamps):
    assert _words(timestamps) == []


def test_ragged_arrays_are_truncated_rather_than_guessed_at():
    # A Word whose timing came from a mismatched index would put a finding's evidence
    # span over the wrong part of the call, which is worse than the span being absent.
    assert [w.text for w in _words({
        "words": ["ek", "do", "teen"],
        "start_time_seconds": [0.0, 1.0],
        "end_time_seconds": [1.0, 2.0],
    })] == ["ek", "do"]

    assert _words({"words": ["ek"], "start_time_seconds": [0.0],
                   "end_time_seconds": []}) == []


def test_blank_entries_are_dropped_and_text_is_stripped():
    words = _words({
        "words": ["  haan  ", "", "   ", "ji"],
        "start_time_seconds": [0.0, 1.0, 2.0, 3.0],
        "end_time_seconds": [0.5, 1.5, 2.5, 3.5],
    })
    assert [(w.text, w.start_ms) for w in words] == [("haan", 0), ("ji", 3_000)]


def test_string_offsets_from_the_wire_are_accepted():
    # JSON numbers arrive as numbers, but a provider that ever sends them quoted must
    # not produce a TypeError in the middle of a call's transcript.
    words = _words({"words": ["haan"], "start_time_seconds": ["1.5"],
                    "end_time_seconds": ["2.25"]})
    assert (words[0].start_ms, words[0].end_ms) == (1_500, 2_250)
