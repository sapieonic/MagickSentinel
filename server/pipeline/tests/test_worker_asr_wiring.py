"""How the ASR slot is wired into ``Finalizer``, and what happens when it fails.

``Finalizer`` takes an injected ``BatchASR``, so every adapter — the fake, the
language router, the real Gemini adapter with a stand-in SDK client — plugs into the
same hole. These tests drive the slot from the outside: what the provider is asked
for, what reaches storage, which failures stop the call, and which ones must not.

The last test is the one worth keeping alive: a documented API response shape goes in
one end and a quotable evidence span comes out the other, with no network anywhere.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field

import pytest
from conftest import COMPLIANT_OPENING

from sentinel_pipeline.analysis import CallAnalyzer
from sentinel_pipeline.asr.base import ASRResult
from sentinel_pipeline.compliance.engine import RuleEngine, load_default_rule_set
from sentinel_pipeline.cost import CostPolicy, ModelPricing, TenantBudget
from sentinel_pipeline.models import Analysis, Channel, Finding, Transcript, Word
from sentinel_pipeline.providers import FakeAnalysisProvider, FakeASR
from sentinel_pipeline.providers.google import GoogleTranscribeASR
from sentinel_pipeline.providers.registry import LanguageRoutedASR
from sentinel_pipeline.worker import Finalizer

SAMPLE_RATE = 16_000

CLEAN = COMPLIANT_OPENING + " Can you pay Rs 15,000 by the fifteenth? Thank you."
THREATENING = COMPLIANT_OPENING + " If you do not pay we will file a police case."

PRICING = {"fake-analysis": ModelPricing("fake-analysis", 25_000, 125_000)}


# ------------------------------------------------------------------------- fakes


@dataclass
class FakeSegments:
    """Stored audio, per channel. ``None`` is a channel that has none."""

    have_far: bool = True
    have_near: bool = True

    def channel_audio(self, call_id: str, channel_: Channel) -> bytes | None:
        if channel_ is Channel.FAR and not self.have_far:
            return None
        if channel_ is Channel.NEAR and not self.have_near:
            return None
        return b"\x00" * 320


@dataclass
class RecordingSink:
    transcripts: dict = field(default_factory=dict)
    analyses: dict = field(default_factory=dict)
    findings: dict = field(default_factory=dict)
    statuses: list = field(default_factory=list)

    def save_transcript(self, call_id: str, transcript: Transcript) -> None:
        self.transcripts[call_id] = transcript

    def save_analysis(self, call_id: str, analysis: Analysis, cost_paise: int) -> None:
        self.analyses[call_id] = (analysis, cost_paise)

    def save_findings(self, call_id: str, rule_set_version: int,
                      findings: list[Finding]) -> None:
        self.findings[call_id] = (rule_set_version, findings)

    def set_status(self, call_id: str, status: str) -> None:
        self.statuses.append(status)


@dataclass
class RecordingASR:
    """Transcribes anything, and remembers exactly what it was asked for."""

    name: str = "recording-asr"
    version: str = "1"
    requests: list[dict] = field(default_factory=list)

    def transcribe(self, audio: bytes, *, sample_rate: int,
                   language_hint: str | None = None) -> ASRResult:
        self.requests.append({
            "audio_len": len(audio),
            "sample_rate": sample_rate,
            "language_hint": language_hint,
        })
        return ASRResult(
            text="theek hai",
            words=[Word("theek", 0, 400), Word("hai", 400, 800)],
            provider=self.name,
            provider_version=self.version,
        )


@dataclass
class ScriptedASR:
    """Returns or raises once per call, in the order ``Finalizer`` transcribes.

    Far first, then near — the order is pinned by a test below, because every other
    scripted test in this file depends on it.
    """

    script: list = field(default_factory=list)
    name: str = "scripted-asr"
    version: str = "1"

    def transcribe(self, audio: bytes, *, sample_rate: int,
                   language_hint: str | None = None) -> ASRResult:
        step = self.script.pop(0)
        if isinstance(step, Exception):
            raise step
        return step


class ExplodingAnalysis:
    name, version = "boom-analysis", "1"

    def complete(self, prompt, *, max_output_tokens):
        raise RuntimeError("provider unavailable")


def spoken(text: str, *, start_ms: int = 0, provider: str = "scripted-asr") -> ASRResult:
    ms_per_word = 400
    words = []
    t = start_ms
    for token in text.split():
        words.append(Word(text=token, start_ms=t, end_ms=t + ms_per_word))
        t += ms_per_word
    return ASRResult(text=text, words=words, provider=provider, provider_version="1")


def build(*, asr=None, analysis_provider=None, segments=None,
          policy=None) -> tuple[Finalizer, RecordingSink]:
    sink = RecordingSink()
    analyzer = CallAnalyzer(analysis_provider) if analysis_provider is not None else None
    return (
        Finalizer(
            asr=asr or FakeASR(text=CLEAN),
            analyzer=analyzer,
            rules=RuleEngine(load_default_rule_set()),
            judge=None,
            segments=segments or FakeSegments(),
            sink=sink,
            cost_policy=policy or CostPolicy(pricing=PRICING),
        ),
        sink,
    )


@pytest.fixture
def ctx(make_call):
    def context(duration_ms: int = 300_000):
        return make_call(duration_ms=duration_ms).context

    return context


# ------------------------------------------------------------- the happy path


def test_both_channels_are_transcribed_and_each_records_the_provider_that_ran(ctx):
    asr = FakeASR(name="google-transcribe", version="gemini-3.5-transcribe", text=CLEAN)
    f, sink = build(asr=asr, analysis_provider=FakeAnalysisProvider())

    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert outcome.status == "complete"
    assert set(outcome.transcript.channels) == {Channel.FAR, Channel.NEAR}
    for channel in (Channel.FAR, Channel.NEAR):
        stored = outcome.transcript.get(channel)
        # Both fields on every channel: a transcript from before a model change and
        # one from after have to stay distinguishable, per channel, forever.
        assert stored.provider == "google-transcribe"
        assert stored.provider_version == "gemini-3.5-transcribe"
        assert stored.channel is channel
    assert sink.transcripts[outcome.call_id] is outcome.transcript


def test_the_borrower_channel_is_transcribed_before_the_agent_channel(ctx):
    asr = ScriptedASR(script=[spoken("haan bolo"), spoken("main Ravi bol raha hoon")])
    f, _ = build(asr=asr)

    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert outcome.transcript.far.text == "haan bolo"
    assert outcome.transcript.near.text == "main Ravi bol raha hoon"


def test_the_stored_words_survive_the_trip_into_the_transcript(ctx):
    asr = RecordingASR()
    f, _ = build(asr=asr)

    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    near = outcome.transcript.near
    assert [word.text for word in near.words] == ["theek", "hai"]
    assert near.span_text(400, 800) == "hai"


# ------------------------------------------------------- what the provider is told


def test_the_language_hint_is_not_passed_to_the_provider(ctx):
    # Pinning current behaviour: `_transcribe` calls
    # `transcribe(audio, sample_rate=16_000)` and never forwards a language hint,
    # because nothing threads the tenant's configured language down to here. The
    # consequence is that a multi-language floor depends entirely on the provider's
    # own detection, and a `LanguageRoutedASR` — which routes on this exact
    # argument and on nothing else — can never leave its default route from inside
    # `Finalizer`. See the routing tests below.
    asr = RecordingASR()
    f, _ = build(asr=asr)

    f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert len(asr.requests) == 2
    assert [r["language_hint"] for r in asr.requests] == [None, None]
    assert {r["sample_rate"] for r in asr.requests} == {SAMPLE_RATE}
    assert {r["audio_len"] for r in asr.requests} == {320}


# --------------------------------------------------------------- missing audio


def test_a_missing_borrower_channel_still_leaves_the_agent_channel_transcribed(ctx):
    # A headset unplugged mid-call, or tier B suppression removing the far side.
    # Survivable: the conduct rules that matter apply to the agent, so a call with
    # only the near channel is still worth monitoring.
    f, _ = build(asr=FakeASR(text=THREATENING),
                 segments=FakeSegments(have_far=False))

    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert outcome.status == "complete"
    assert set(outcome.transcript.channels) == {Channel.NEAR}
    assert any("no audio on the borrower channel" in n for n in outcome.notes)
    assert "false_legal_threat" in {x.rule_id for x in outcome.findings}


def test_a_missing_agent_channel_still_leaves_the_borrower_channel_transcribed(ctx):
    f, _ = build(asr=FakeASR(text=CLEAN), segments=FakeSegments(have_near=False))

    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert outcome.status == "complete"
    assert set(outcome.transcript.channels) == {Channel.FAR}
    assert any("no audio on the agent channel" in n for n in outcome.notes)


def test_both_channels_missing_fails_the_call_and_saves_nothing(ctx):
    f, sink = build(asr=FakeASR(text=CLEAN),
                    segments=FakeSegments(have_far=False, have_near=False))

    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert outcome.transcript is None
    assert outcome.status == "failed"
    assert sink.statuses == ["failed"]
    # Nothing partial reaches storage: a call recorded as complete with no
    # transcript would look monitored on the coverage report and never be revisited.
    assert sink.transcripts == {}
    assert sink.findings == {}
    assert len(outcome.notes) == 2


# --------------------------------------------------------------- provider errors


def test_an_asr_that_raises_on_one_channel_notes_it_and_keeps_the_other(ctx):
    asr = ScriptedASR(script=[RuntimeError("upstream 503"), spoken(THREATENING)])
    f, _ = build(asr=asr, analysis_provider=FakeAnalysisProvider())

    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert outcome.status == "complete"
    assert set(outcome.transcript.channels) == {Channel.NEAR}
    assert any("asr failed on the borrower channel" in n for n in outcome.notes)
    # A one-sided provider failure must not cost the call its compliance coverage.
    assert "false_legal_threat" in {x.rule_id for x in outcome.findings}


def test_an_asr_that_raises_on_both_channels_fails_the_call(ctx):
    asr = ScriptedASR(script=[RuntimeError("upstream 503"), RuntimeError("upstream 503")])
    f, sink = build(asr=asr, analysis_provider=FakeAnalysisProvider())

    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert outcome.status == "failed"
    assert sink.statuses == ["failed"]
    assert outcome.transcript is None
    assert outcome.notes == ["asr failed on the borrower channel",
                            "asr failed on the agent channel"]


def test_asr_failure_stops_the_call_before_the_analyser_is_asked_anything(ctx):
    # First step of the deliberate degradation order: with no transcript there is
    # nothing to analyse or judge, so the call is failed rather than half-processed.
    provider = FakeAnalysisProvider()
    asr = ScriptedASR(script=[RuntimeError("boom"), RuntimeError("boom")])
    f, sink = build(asr=asr, analysis_provider=provider)

    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert outcome.status == "failed"
    assert provider.calls == 0
    assert sink.analyses == {}


# ------------------------------------------------------- the degradation order


def test_a_successful_transcription_survives_an_analysis_failure_and_still_flags(ctx):
    # The invariant AGENTS.md calls out, and the reason ASR, analysis and
    # compliance are one function rather than three subscribers. Losing a summary
    # is an inconvenience; losing compliance coverage is a breach of what was sold.
    # So: ASR failure stops the call, an analysis failure after a good transcript
    # must not.
    f, sink = build(asr=FakeASR(text=THREATENING),
                    analysis_provider=ExplodingAnalysis())
    budget = TenantBudget("t", 10_000_000)

    outcome = f.finalize(ctx(), budget)

    assert outcome.status == "complete"
    assert outcome.analysis is None
    assert "analysis provider failed" in outcome.notes
    assert sink.analyses == {}

    assert outcome.transcript is not None
    assert sink.transcripts[outcome.call_id] is outcome.transcript
    version, findings = sink.findings[outcome.call_id]
    assert version == 1, "a flag must be traceable to the rule text that raised it"
    assert "false_legal_threat" in {x.rule_id for x in findings}
    flagged = next(x for x in findings if x.rule_id == "false_legal_threat")
    assert flagged.evidence_text and flagged.span_start_ms is not None
    assert sink.statuses[-1] == "complete"
    assert budget.spent_paise == 0, "a failed analysis is not billed"


# ---------------------------------------------------------------------- logging


def test_the_asr_failure_log_line_never_repeats_the_provider_error_text(ctx, caplog):
    # The adapter's exception can echo the request back, transcript fragment and
    # all, so the handler logs `error_type` and never `str(exc)`. Structured fields
    # only, no PII on any tier — a log sink is not inside the retention boundary.
    echoed = "rejected input: borrower Ramesh, loan account 4321 is overdue"
    call_ctx = ctx()
    asr = ScriptedASR(script=[RuntimeError(echoed), RuntimeError(echoed)])
    f, _ = build(asr=asr)

    with caplog.at_level(logging.ERROR, logger="sentinel_pipeline.worker"):
        outcome = f.finalize(call_ctx, TenantBudget("t", 10_000_000))

    assert outcome.status == "failed"
    records = [r for r in caplog.records if r.getMessage() == "asr failed"]
    assert len(records) == 2
    for record in records:
        assert record.exc_info is None, "a traceback would carry the message anyway"
        assert record.error_type == "RuntimeError"
        assert record.call_id == call_ctx.call_id
        assert record.channel in (0, 1)
        # Nothing anywhere on the record — message, args or extras — quotes the
        # provider's text.
        assert all(echoed not in str(value) for value in record.__dict__.values())
    assert echoed not in caplog.text


# ---------------------------------------------------------------- routed ASR


def test_a_language_routed_asr_records_the_provider_that_actually_ran(ctx):
    # The router is a wiring detail and must not appear in the audit trail: a
    # transcript has to name the model that produced it, or two calls on the same
    # floor become incomparable while both look authoritative.
    default = FakeASR(name="google-transcribe", version="gemini-3.5-transcribe",
                      text=THREATENING)
    tamil = FakeASR(name="sarvam", version="saaras:v4", text=THREATENING)
    router = LanguageRoutedASR(default=default, routes={"ta-IN": tamil})
    f, _ = build(asr=router, analysis_provider=FakeAnalysisProvider())

    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert outcome.status == "complete"
    for channel in (Channel.FAR, Channel.NEAR):
        stored = outcome.transcript.get(channel)
        assert stored.provider == "google-transcribe"
        assert stored.provider_version == "gemini-3.5-transcribe"
        assert stored.provider != router.name
    assert router.name == "language-routed"
    assert router.version == "default=google-transcribe,ta-IN=sarvam"
    assert "false_legal_threat" in {x.rule_id for x in outcome.findings}


def test_a_routed_language_is_unreachable_from_inside_the_finalizer(ctx):
    # Two halves of one gap, pinned together. The router works: given a hint it
    # picks the provider configured for that language. `Finalizer` never gives it
    # one, so a Tamil route — the reason the router exists, since the default model
    # has no Tamil at all — cannot be reached through the pipeline today.
    default = FakeASR(name="google-transcribe", text=CLEAN)
    tamil = FakeASR(name="sarvam", text=CLEAN)
    router = LanguageRoutedASR(default=default, routes={"ta-IN": tamil})

    routed = router.transcribe(b"\x00" * 320, sample_rate=SAMPLE_RATE,
                               language_hint="ta-IN")
    assert routed.provider == "sarvam"

    f, _ = build(asr=router)
    outcome = f.finalize(ctx(), TenantBudget("t", 10_000_000))

    assert {c.provider for c in outcome.transcript.channels.values()} == {
        "google-transcribe"
    }


# ------------------------------------------------------- the real Google adapter


def word_info(text: str, start_s: float, end_s: float) -> dict:
    """One ``word_info`` annotation, with a protobuf Duration in its JSON form."""
    return {
        "type": "word_info",
        "text": text,
        "start_offset": f"{start_s}s",
        "end_offset": f"{end_s}s",
    }


def google_interaction(sentence: str, *, seconds_per_word: float = 0.4,
                       in_tokens: int = 900, out_tokens: int = 60) -> dict:
    """A response in the shape ``gemini-3.5-transcribe`` documents.

    Word offsets are laid out at a plausible speaking rate so the timings that end
    up in an evidence span are the ones a real call would produce.
    """
    annotations = []
    t = 0.0
    for token in sentence.split():
        annotations.append(word_info(token, round(t, 2), round(t + 0.3, 2)))
        t = round(t + seconds_per_word, 2)
    return {
        "output_text": sentence,
        "steps": [
            {
                "type": "model_output",
                "content": [
                    {"type": "text", "text": sentence, "annotations": annotations},
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


class NoFiles:
    """The Files API, which a 320-byte chunk has no business touching."""

    def upload(self, **kwargs):
        raise AssertionError("audio this small must be inlined, not uploaded")

    def delete(self, **kwargs):
        raise AssertionError("nothing was uploaded, so nothing can be deleted")


class FakeGenAIClient:
    """Stands in for ``google.genai.Client``, so this runs with no SDK and no network."""

    def __init__(self, responses: list[dict]) -> None:
        self.interactions = FakeInteractions(responses)
        self.files = NoFiles()


FAR_SPEECH = "haan bataiye main sun raha hoon"
NEAR_SPEECH = (
    "my name is Ravi calling from Acme Recovery Services on behalf of the bank "
    "regarding your loan account if you do not pay we will file a police case"
)


def test_a_google_transcription_becomes_a_span_a_reviewer_can_quote(ctx):
    # The highest-value path in this file: a documented Interactions response goes
    # in, and what comes out is a compliance finding whose evidence a reviewer can
    # trace back to specific words. Word timestamps are the entire reason this
    # model was shortlisted, so every link in the chain — Duration strings to
    # milliseconds, ASRResult to ChannelTranscript, word timings to `span_text` —
    # is exercised here in one go rather than assumed.
    client = FakeGenAIClient([
        google_interaction(FAR_SPEECH),
        google_interaction(NEAR_SPEECH),
    ])
    asr = GoogleTranscribeASR(client=client)
    f, sink = build(asr=asr, analysis_provider=FakeAnalysisProvider())
    budget = TenantBudget("t", 10_000_000)

    outcome = f.finalize(ctx(), budget)

    assert outcome.status == "complete"
    assert asr.version == "gemini-3.5-transcribe"

    far = outcome.transcript.far
    near = outcome.transcript.near
    assert far.text == FAR_SPEECH
    assert near.text == NEAR_SPEECH
    for stored in (far, near):
        assert stored.provider == "google-transcribe"
        assert stored.provider_version == "gemini-3.5-transcribe"

    tokens = NEAR_SPEECH.split()
    assert [word.text for word in near.words] == tokens
    # "0.4s" is 400 ms, and the whole call was one chunk, so the nth word starts at
    # n * 400 on the call's timeline.
    index = tokens.index("police")
    assert near.words[index].start_ms == index * 400
    assert near.words[index].end_ms == index * 400 + 300
    # The model reports no per-word confidence; a synthesised 1.0 would make a
    # low-quality span look reviewed.
    assert all(word.confidence is None for word in near.words)

    flagged = next(x for x in outcome.findings if x.rule_id == "false_legal_threat")
    assert flagged.tier == 1
    assert flagged.span_start_ms == index * 400
    assert flagged.span_end_ms == (index + 1) * 400 + 300
    # The span the finding carries resolves back to the exact words, and the stored
    # evidence quote is the readable window around them.
    assert near.span_text(flagged.span_start_ms, flagged.span_end_ms) == "police case"
    assert "police case" in flagged.evidence_text
    assert "we will file" in flagged.evidence_text

    version, findings = sink.findings[outcome.call_id]
    assert version == 1
    assert findings == outcome.findings


def test_asr_token_counts_never_reach_the_tenant_budget(ctx):
    # Pinning current behaviour, and it is a money question rather than a nicety:
    # `gemini-3.5-transcribe` bills per token, the adapter reports those tokens on
    # the ASRResult, and `Finalizer` spends nothing against them — the only spend
    # recorded is the analysis. ASR is the largest recurring cost in this pipeline,
    # so if ASR ever starts being billed here, this test should be the thing that
    # notices.
    client = FakeGenAIClient([
        google_interaction(FAR_SPEECH, in_tokens=50_000, out_tokens=5_000),
        google_interaction(NEAR_SPEECH, in_tokens=50_000, out_tokens=5_000),
    ])
    asr = GoogleTranscribeASR(client=client)

    reported = asr.transcribe(b"\x00" * 320, sample_rate=SAMPLE_RATE)
    assert (reported.input_tokens, reported.output_tokens) == (50_000, 5_000)

    client.interactions.responses.append(google_interaction(NEAR_SPEECH))
    f, sink = build(asr=asr, analysis_provider=FakeAnalysisProvider())
    budget = TenantBudget("t", 10_000_000)

    outcome = f.finalize(ctx(), budget)

    assert outcome.status == "complete"
    analysis, cost_paise = sink.analyses[outcome.call_id]
    assert budget.spent_paise == cost_paise
    assert analysis.input_tokens > 0
