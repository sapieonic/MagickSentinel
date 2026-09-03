"""``ASRResult``, and the single conversion that turns it into stored evidence.

Everything a reviewer is ever shown about what was said comes out of
``to_channel_transcript``, so these tests are mostly about what survives that
hand-off, what deliberately does not, and exactly which words a span query is
allowed to quote. The span predicate is strict on both ends; a quote that silently
picked up the word before it would be a quote the recording does not support.
"""

from __future__ import annotations

from dataclasses import fields

from conftest import words as lay_out

from sentinel_pipeline.asr.base import ASRResult
from sentinel_pipeline.models import Channel, ChannelTranscript, Word
from sentinel_pipeline.providers import FakeASR

SAMPLE_RATE = 16_000


def w(text: str, start_ms: int, end_ms: int) -> Word:
    return Word(text=text, start_ms=start_ms, end_ms=end_ms)


def transcribed(text: str, channel: Channel = Channel.NEAR) -> ChannelTranscript:
    """A channel transcript built the way the pipeline builds one: through an ASR.

    Word timings come from the fake at 150 wpm, so the word stream has the shape a
    real adapter produces — adjacent words touching at a shared millisecond — rather
    than the comfortable gaps a hand-written fixture would leave.
    """
    result = FakeASR(text=text).transcribe(b"\x00" * 320, sample_rate=SAMPLE_RATE)
    return result.to_channel_transcript(channel)


# --------------------------------------------------------------------- defaults


def test_an_asr_result_defaults_to_hindi_with_no_words_and_an_unknown_provider():
    result = ASRResult(text="haan theek hai")

    assert result.text == "haan theek hai"
    assert result.words == []
    assert result.language == "hi"
    assert result.confidence is None
    # "unknown" rather than an empty string: a stored transcript whose provider was
    # never set has to be visibly unattributed, because quality trends across a
    # model change are computed by grouping on exactly these two fields.
    assert result.provider == "unknown"
    assert result.provider_version == "unknown"


def test_unreported_token_counts_default_to_zero_rather_than_none():
    # Zero is the truthful reading for a provider that bills per minute of audio:
    # Sarvam charges by the second and reports no tokens because there are none, so
    # 0 is the fact rather than a placeholder. It also keeps the cost arithmetic
    # integer-only end to end, and money in this system is an integer of paise —
    # a None here would put a null check in the one place spend is summed.
    result = ASRResult(text="")

    assert result.input_tokens == 0
    assert result.output_tokens == 0
    assert isinstance(result.input_tokens, int)
    assert isinstance(result.output_tokens, int)


def test_two_results_do_not_share_the_default_word_list():
    first = ASRResult(text="one")
    second = ASRResult(text="two")

    first.words.append(w("one", 0, 400))

    assert second.words == []


# ------------------------------------------------------------------ conversion


def test_the_far_channel_conversion_carries_every_field_across():
    spoken = [w("nahin", 0, 300), w("mera", 300, 700), w("account", 700, 1_200)]
    result = ASRResult(
        text="nahin mera account",
        words=spoken,
        language="hi-IN",
        confidence=0.82,
        provider="google-transcribe",
        provider_version="gemini-3.5-transcribe",
    )

    ct = result.to_channel_transcript(Channel.FAR)

    assert ct.channel is Channel.FAR
    assert ct.text == "nahin mera account"
    assert ct.words == spoken
    assert ct.language == "hi-IN"
    assert ct.confidence == 0.82
    # The pair travels together onto every stored channel: a transcript produced
    # before a model change and one produced after must stay distinguishable, or a
    # WER trend across the change means nothing.
    assert ct.provider == "google-transcribe"
    assert ct.provider_version == "gemini-3.5-transcribe"


def test_the_near_channel_conversion_carries_every_field_across():
    spoken = [w("police", 30_000, 30_400), w("case", 30_400, 30_900)]
    result = ASRResult(
        text="police case",
        words=spoken,
        language="en-IN",
        confidence=0.91,
        provider="sarvam",
        provider_version="saarika:v2",
    )

    ct = result.to_channel_transcript(Channel.NEAR)

    assert ct.channel is Channel.NEAR
    assert ct.text == "police case"
    assert ct.words == spoken
    assert ct.language == "en-IN"
    assert ct.confidence == 0.91
    assert ct.provider == "sarvam"
    assert ct.provider_version == "saarika:v2"


def test_the_channel_is_the_only_thing_the_conversion_decides():
    result = ASRResult(text="ok", provider="fake-asr", provider_version="1")

    far = result.to_channel_transcript(Channel.FAR)
    near = result.to_channel_transcript(Channel.NEAR)

    assert (far.channel, near.channel) == (Channel.FAR, Channel.NEAR)
    assert far.text == near.text == "ok"
    assert far.provider == near.provider == "fake-asr"


def test_token_counts_are_not_carried_onto_the_channel_transcript():
    # Pinning current behaviour, not endorsing it. Token counts are a fact about
    # one billed request, not about the words, so they stop at the conversion —
    # which means anything that wants to account for ASR spend has to read the
    # ASRResult before it becomes a transcript. ASR is the largest recurring cost
    # in this pipeline, so where that number is dropped is worth a failing test if
    # it ever moves.
    result = ASRResult(text="x", input_tokens=1_200, output_tokens=340)

    ct = result.to_channel_transcript(Channel.NEAR)

    assert not hasattr(ct, "input_tokens")
    assert not hasattr(ct, "output_tokens")
    assert {f.name for f in fields(ChannelTranscript)} == {
        "channel", "text", "words", "language", "provider", "provider_version",
        "confidence",
    }
    assert (result.input_tokens, result.output_tokens) == (1_200, 340)


def test_the_word_list_is_shared_with_the_channel_transcript_not_copied():
    # Current behaviour: the conversion passes the same list object, so the two
    # views of the words are one list. Pinned in both directions because the
    # cheap-looking fix (a copy) and the cheap-looking bug (a mutation upstream)
    # are the same edit, and the words are what a finding quotes.
    spoken = [w("hum", 0, 300)]
    result = ASRResult(text="hum", words=spoken)

    ct = result.to_channel_transcript(Channel.NEAR)

    assert ct.words is result.words
    assert ct.words is spoken

    ct.words.append(w("aayenge", 300, 900))
    assert [word.text for word in result.words] == ["hum", "aayenge"]

    result.words.append(w("ghar", 900, 1_300))
    assert [word.text for word in ct.words] == ["hum", "aayenge", "ghar"]


# ------------------------------------------------------------- speaker identity


def test_the_far_channel_is_the_borrower_and_the_near_channel_is_the_agent():
    # The two channels were captured separately, so this mapping is the reason
    # there is no diarization step anywhere in the pipeline. Almost every conduct
    # rule applies to the agent only; if these ever swapped, the engine would judge
    # the borrower's words as the agent's and flag calls that were fine while
    # missing the ones that were not.
    assert Channel.FAR.speaker == "borrower"
    assert Channel.NEAR.speaker == "agent"
    assert (Channel.FAR.value, Channel.NEAR.value) == (0, 1)


def test_a_converted_result_reports_the_speaker_of_the_channel_it_landed_on():
    result = ASRResult(text="aap kaun bol rahe hain")

    assert result.to_channel_transcript(Channel.FAR).channel.speaker == "borrower"
    assert result.to_channel_transcript(Channel.NEAR).channel.speaker == "agent"


# ------------------------------------------------------------------- span_text


def test_a_word_ending_exactly_at_the_span_start_is_not_quoted():
    # The predicate is strict on both ends. A word that finished at the instant the
    # span opened is not evidence for what happened inside it, and quoting it would
    # put a word in a reviewer's evidence box that the span does not cover.
    ct = ChannelTranscript(
        channel=Channel.NEAR,
        text="police case",
        words=[w("police", 0, 1_000), w("case", 1_000, 1_500)],
    )

    assert ct.span_text(1_000, 2_000) == "case"


def test_a_word_starting_exactly_at_the_span_end_is_not_quoted():
    ct = ChannelTranscript(
        channel=Channel.NEAR,
        text="police case",
        words=[w("police", 0, 1_000), w("case", 1_000, 1_500)],
    )

    assert ct.span_text(0, 1_000) == "police"


def test_a_word_fully_inside_the_span_is_quoted():
    ct = ChannelTranscript(
        channel=Channel.NEAR,
        text="warrant",
        words=[w("warrant", 5_200, 5_800)],
    )

    assert ct.span_text(5_000, 6_000) == "warrant"


def test_a_word_straddling_both_ends_of_the_span_is_quoted():
    # A drawn-out word that starts before the window and ends after it was still
    # being spoken throughout, so it belongs in the quote.
    ct = ChannelTranscript(
        channel=Channel.NEAR,
        text="giraftaaaaar",
        words=[w("giraftaaaaar", 1_000, 9_000)],
    )

    assert ct.span_text(4_000, 5_000) == "giraftaaaaar"


def test_adjacent_asr_words_touching_at_one_millisecond_do_not_bleed_into_a_span():
    # Real adapters emit words that touch: word n ends on the millisecond word n+1
    # begins. With a strict predicate a one-word window quotes exactly one word.
    ct = transcribed("we will file a police case")
    assert [word.text for word in ct.words] == ["we", "will", "file", "a", "police", "case"]
    assert ct.words[1].start_ms == ct.words[0].end_ms

    assert ct.span_text(400, 800) == "will"
    assert ct.span_text(1_600, 2_400) == "police case"


def test_a_span_ending_where_it_starts_quotes_nothing():
    ct = transcribed("police case")

    assert ct.span_text(400, 400) == ""


def test_a_channel_with_no_word_timings_falls_back_to_its_whole_text():
    # This is the Sarvam case, and the reason the fallback exists at all: its
    # timestamps are phrase-level and can be absent from a response entirely. With
    # no timings there is no honest way to narrow the quote, so a span that starts
    # at the beginning of the call gets the whole channel. Coarse evidence is worse
    # than precise evidence and far better than dropping the finding.
    ct = ChannelTranscript(
        channel=Channel.NEAR,
        text="aap ko jail bhej denge",
        words=[],
    )

    assert ct.span_text(0, 30_000) == "aap ko jail bhej denge"
    assert ct.span_text(-1_000, 5) == "aap ko jail bhej denge"


def test_a_channel_with_no_word_timings_quotes_nothing_for_a_later_window():
    # The other half of the same decision: asked about a window starting later in
    # the call, an untimed channel returns nothing rather than handing back text
    # from a part of the call the reviewer did not ask about. Inventing a quote
    # from the wrong minute is the failure that would destroy trust in the tool.
    ct = ChannelTranscript(
        channel=Channel.NEAR,
        text="aap ko jail bhej denge",
        words=[],
    )

    assert ct.span_text(1, 30_000) == ""
    assert ct.span_text(120_000, 150_000) == ""


# ----------------------------------------------------------------- words_within


def test_words_within_uses_the_same_strict_boundaries_as_span_text():
    spoken = [w("police", 0, 1_000), w("case", 1_000, 1_500)]
    ct = ChannelTranscript(channel=Channel.NEAR, text="police case", words=spoken)

    assert ct.words_within(1_000, 2_000) == [spoken[1]]
    assert ct.words_within(0, 1_000) == [spoken[0]]
    assert ct.words_within(0, 1_500) == spoken
    assert ct.words_within(1_500, 2_000) == []


def test_words_within_returns_a_straddling_word_and_an_enclosed_word():
    long_word = w("giraftaaaaar", 1_000, 9_000)
    inside = w("warrant", 4_100, 4_500)
    ct = ChannelTranscript(
        channel=Channel.NEAR,
        text="giraftaaaaar warrant",
        words=[long_word, inside],
    )

    assert ct.words_within(4_000, 5_000) == [long_word, inside]


def test_words_within_on_an_untimed_channel_is_empty_rather_than_a_guess():
    # No fallback here, unlike span_text: there is no word list to approximate, and
    # a rule that needs per-word timings must see that it has none.
    ct = ChannelTranscript(channel=Channel.NEAR, text="jail bhej denge", words=[])

    assert ct.words_within(0, 30_000) == []


def test_words_within_preserves_the_order_the_provider_returned():
    ct = transcribed("if you do not pay we will file a police case")

    covered = ct.words_within(0, 4_000)

    assert [word.text for word in covered] == [word.text for word in ct.words[:10]]


# ------------------------------------------------------------ zero-length words


def test_a_zero_length_word_is_reachable_only_strictly_inside_a_span():
    # These do occur: every adapter rounds a float offset to whole milliseconds
    # (`int(w.start * 1000)`), so a short token whose start and end round to the
    # same millisecond arrives with zero duration, and nothing rejects it. With a
    # strict predicate such a word is invisible to a span that merely touches it,
    # so a rule matching it must widen the window rather than butt against it.
    zero = w("fir", 4_000, 4_000)
    ct = ChannelTranscript(channel=Channel.NEAR, text="fir", words=[zero])

    assert ct.span_text(3_000, 5_000) == "fir"
    assert ct.words_within(3_999, 4_001) == [zero]

    assert ct.span_text(4_000, 5_000) == ""
    assert ct.span_text(3_000, 4_000) == ""
    assert ct.words_within(4_000, 5_000) == []
    assert ct.words_within(3_000, 4_000) == []


def test_a_zero_length_word_survives_the_conversion_untouched():
    # The conversion does not filter or repair the word list, so whatever an
    # adapter reports is what the rule engine sees.
    zero = w("fir", 4_000, 4_000)
    result = ASRResult(text="fir", words=[zero], provider="sarvam")

    ct = result.to_channel_transcript(Channel.FAR)

    assert ct.words == [zero]


# ------------------------------------------------------- result to evidence span


def test_an_asr_result_becomes_a_quotable_evidence_span():
    # The whole point of the dataclass: word timings in, a verbatim quote out. A
    # flag a reviewer cannot trace to the words it came from is not usable as
    # evidence with a bank, and this is the shortest path from one to the other.
    spoken = lay_out("if you do not pay we will file a police case", start_ms=30_000)
    result = ASRResult(
        text="if you do not pay we will file a police case",
        words=spoken,
        provider="google-transcribe",
        provider_version="gemini-3.5-transcribe",
    )

    ct = result.to_channel_transcript(Channel.NEAR)
    police = next(word for word in ct.words if word.text == "police")
    case = next(word for word in ct.words if word.text == "case")

    assert ct.span_text(police.start_ms, case.end_ms) == "police case"
    assert ct.words_within(police.start_ms, case.end_ms) == [police, case]
    assert police.start_ms >= 30_000, "word timings stay on the call's timeline"
