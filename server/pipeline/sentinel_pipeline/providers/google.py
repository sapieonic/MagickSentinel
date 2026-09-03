"""Google Gemini 3.5 Transcribe batch ASR adapter.

Like Sarvam and IndicWhisper, this is a *candidate* the Phase 3 evaluation has to
measure over 200 hand-labelled real calls per language. Nothing in this file should be
read as a selection; ``docs/asr-provider-selection.md`` records why it is the Google
model worth measuring and what it cannot do.

Two things about this model decide how the adapter is written.

**Word timestamps are the whole point of choosing it.** Google's other Indic-capable
option, ``chirp_3``, lists word-level timestamps under the features it does *not*
support. Every compliance finding carries a span
(:meth:`sentinel_pipeline.models.ChannelTranscript.span_text`), and a finding a
reviewer cannot trace to specific words is not usable as evidence with a bank — so a
model without per-word timings is not a candidate for this product at all, whatever
its WER.

**Word timestamps and custom vocabulary are mutually exclusive.** The API rejects a
request carrying both, so the choice is made once at construction and is loud rather
than silent: see :class:`GoogleTranscribeASR`.

Speaker diarization is deliberately never requested. The two channels were captured
separately, so the speaker is known exactly rather than inferred, and asking a model
to guess it again would be strictly worse than the answer already in hand.
"""

from __future__ import annotations

import base64
import logging
from dataclasses import dataclass

from ..models import Word
from .base import ASRResult

log = logging.getLogger(__name__)

#: The India-relevant subset of the model's 85+ locales, as documented at
#: https://ai.google.dev/gemini-api/docs/transcribe. Checked at construction because
#: an unsupported hint does not fail — it degrades, and Hindi output for a Tamil call
#: is the kind of quiet quality failure that reaches a bank as a missed flag.
#:
#: **Tamil is absent and that is not an oversight.** ``ta-IN`` is not in the model's
#: supported list, so a Tamil floor needs a different provider for that language.
SUPPORTED_INDIC_LANGUAGES = frozenset(
    {
        "as-IN",  # Assamese
        "bn-IN",  # Bengali
        "en-IN",  # Indian English
        "gu-IN",  # Gujarati
        "hi-IN",  # Hindi
        "kn-IN",  # Kannada
        "ml-IN",  # Malayalam
        "mr-IN",  # Marathi
        "or-IN",  # Oriya
        "pa-IN",  # Punjabi
        "te-IN",  # Telugu
    }
)

# Documented ceilings, in seconds: one hour per request, dropping to thirty minutes
# once word timestamps or diarization are on.
_MAX_AUDIO_S = 3_600
_MAX_AUDIO_WITH_TIMESTAMPS_S = 1_800


class GoogleTranscribeError(RuntimeError):
    """A response that arrived but cannot be turned into a transcript."""


@dataclass
class GoogleTranscribeASR:
    """Batch transcription through ``gemini-3.5-transcribe``.

    ``custom_vocabulary`` and ``word_timestamps`` cannot both be set: the API rejects
    such a request, and finding that out per call would mean discovering it on live
    audio. Constructing the adapter is where the trade-off is made, so a deployment
    that biases towards bank names and product terms has explicitly given up the
    evidence spans, rather than losing them to a 400 nobody reads.

    The SDK is imported inside ``__post_init__`` so a deployment on a different
    provider does not have to install it. Passing ``client`` skips the import
    entirely, which is how the tests run without the SDK present.
    """

    api_key: str | None = None
    model: str = "gemini-3.5-transcribe"
    #: BCP-47 hints used when the caller does not pass a per-call
    #: ``language_hint``. Empty means the model detects the language itself and
    #: handles code-switching, which is the interesting setting for Hinglish.
    language_hints: tuple[str, ...] = ()
    #: Up to 1,000 domain terms. Google's own guidance is that ~100 works best.
    custom_vocabulary: tuple[str, ...] = ()
    word_timestamps: bool = True
    #: Requests are split at this many seconds so a long call still produces a
    #: transcript. Below the documented ceiling on purpose, to leave room for the
    #: model counting duration slightly differently than we do.
    max_chunk_s: int = 1_500
    #: Above this, the chunk goes through the Files API instead of being inlined.
    inline_limit_bytes: int = 8 * 1024 * 1024
    bits_per_sample: int = 16
    channels: int = 1
    #: Best-effort deletion of anything uploaded to the Files API. On by default:
    #: call audio is subject to the tenant's retention period (OPEN-6), and leaving
    #: it in a third-party file store until an automatic 48-hour expiry is a
    #: retention decision nobody made.
    delete_uploads: bool = True
    client: object | None = None

    name: str = "google-transcribe"

    def __post_init__(self) -> None:
        if self.custom_vocabulary and self.word_timestamps:
            raise ValueError(
                "gemini-3.5-transcribe rejects custom_vocabulary together with "
                "word-level timestamps. Compliance findings need the timestamps, so "
                "leave custom_vocabulary empty unless you are deliberately trading "
                "evidence spans for recognition of domain terms."
            )
        if len(self.custom_vocabulary) > 1_000:
            raise ValueError("custom_vocabulary is capped at 1,000 terms by the API")
        for code in self.language_hints:
            _check_language(code)
        ceiling = _MAX_AUDIO_WITH_TIMESTAMPS_S if self.word_timestamps else _MAX_AUDIO_S
        if not 0 < self.max_chunk_s <= ceiling:
            raise ValueError(
                f"max_chunk_s must be between 1 and {ceiling} seconds for this "
                f"configuration; got {self.max_chunk_s}"
            )

        if self.client is None:
            from google import genai  # noqa: PLC0415 - lazily imported; see the module docstring

            self.client = (
                genai.Client(api_key=self.api_key) if self.api_key else genai.Client()
            )
        self.version = self.model

    # ------------------------------------------------------------------ BatchASR

    def transcribe(self, audio: bytes, *, sample_rate: int,
                   language_hint: str | None = None) -> ASRResult:
        if language_hint:
            _check_language(language_hint)
            codes = [language_hint]
        else:
            codes = list(self.language_hints)

        bytes_per_second = sample_rate * self.channels * self.bits_per_sample // 8
        if bytes_per_second <= 0:
            raise ValueError("sample_rate, channels and bits_per_sample must be positive")
        chunk_bytes = self.max_chunk_s * bytes_per_second

        texts: list[str] = []
        words: list[Word] = []
        input_tokens = 0
        output_tokens = 0

        for offset in range(0, max(len(audio), 1), chunk_bytes):
            chunk = audio[offset : offset + chunk_bytes]
            if not chunk:
                break
            # Exact rather than chunk_index * max_chunk_s: a final short chunk and an
            # audio length that is not a whole number of samples both have to land on
            # the same timeline as the rest of the call, because these offsets end up
            # in a finding's evidence span.
            offset_ms = offset * 1_000 // bytes_per_second
            interaction = self._call(chunk, sample_rate=sample_rate, codes=codes)
            text, chunk_words, in_tok, out_tok = _parse(interaction, offset_ms=offset_ms)
            if text:
                texts.append(text)
            words.extend(chunk_words)
            input_tokens += in_tok
            output_tokens += out_tok

        return ASRResult(
            text=" ".join(texts).strip(),
            words=words,
            # The API does not report the language it settled on, so the hint is the
            # only honest answer; with detection on, the caller knows no more than
            # that the model chose for itself.
            language=(codes[0] if codes else "und"),
            provider=self.name,
            provider_version=self.version,
            input_tokens=input_tokens,
            output_tokens=output_tokens,
        )

    # --------------------------------------------------------------------- request

    def _call(self, chunk: bytes, *, sample_rate: int, codes: list[str]) -> object:
        transcription: dict[str, object] = {"language_codes": codes}
        if self.custom_vocabulary:
            transcription["custom_vocabulary"] = list(self.custom_vocabulary)
        if self.word_timestamps:
            transcription["mode"] = {
                # Verbatim, never "smart": filler removal and re-formatting rewrite
                # the words a finding quotes, and "smart" is incompatible with
                # timestamps anyway.
                "type": "verbatim",
                "timestamp_granularities": ["word"],
            }

        uploaded = None
        try:
            if len(chunk) > self.inline_limit_bytes:
                uploaded = self._upload(chunk, sample_rate=sample_rate)
                part: dict[str, object] = {
                    "type": "audio",
                    "uri": _field(uploaded, "uri"),
                    "mime_type": _field(uploaded, "mime_type") or "audio/l16",
                }
            else:
                # Raw PCM as audio/l16 with the rate declared alongside it. No WAV
                # header: the container exists only to carry a sample rate the
                # request can state directly, and copying every chunk to prepend 44
                # bytes is wasted work at 60,000 minutes a day.
                part = {
                    "type": "audio",
                    "data": base64.b64encode(chunk).decode("ascii"),
                    "mime_type": "audio/l16",
                    "sample_rate": sample_rate,
                    "channels": self.channels,
                }
            return self.client.interactions.create(
                model=self.model,
                input=[part],
                generation_config={"transcription_config": transcription},
            )
        finally:
            if uploaded is not None and self.delete_uploads:
                self._delete(uploaded)

    def _upload(self, chunk: bytes, *, sample_rate: int) -> object:
        import io  # noqa: PLC0415 - only reached on the large-chunk path

        # L16 carries no rate of its own, so a file that will be referenced by URI
        # rather than inlined gets a WAV container to keep the rate attached to it.
        from .sarvam import _wav_header  # noqa: PLC0415

        buf = io.BytesIO(_wav_header(len(chunk), sample_rate, channels=self.channels,
                                     bits=self.bits_per_sample) + chunk)
        return self.client.files.upload(file=buf, config={"mime_type": "audio/wav"})

    def _delete(self, uploaded: object) -> None:
        name = _field(uploaded, "name")
        if not name:
            return
        try:
            self.client.files.delete(name=name)
        except Exception as exc:  # noqa: BLE001 - deletion is best effort
            # The file name identifies one call's audio, so it stays out of the log
            # line. The Files API expires the upload on its own within 48 hours, so a
            # failure here delays the deletion rather than preventing it.
            log.warning("could not delete uploaded audio",
                        extra={"error_type": type(exc).__name__})


# ------------------------------------------------------------------------ parsing


def _check_language(code: str) -> None:
    if code in SUPPORTED_INDIC_LANGUAGES:
        return
    if not code.endswith("-IN"):
        # Non-Indian locales are the model's business, not ours to police; the
        # supported set here covers only the languages this product ships into.
        return
    hint = ""
    if code == "ta-IN":
        hint = (" Tamil is not among the model's supported locales at all — a Tamil "
                "floor needs a different provider for that language.")
    raise ValueError(f"{code} is not a supported gemini-3.5-transcribe locale.{hint}")


def _field(obj: object, name: str) -> object:
    """Read ``name`` off an SDK object or a plain dict.

    The SDK returns typed objects and the REST API returns JSON; tests use dicts.
    One accessor keeps the parser working against all three rather than pinning the
    adapter to whichever the installed SDK version happens to produce.
    """
    if isinstance(obj, dict):
        return obj.get(name)
    return getattr(obj, name, None)


def _offset_ms(value: object) -> int:
    """Parse a protobuf Duration in its JSON form — ``"0.100s"`` — to milliseconds."""
    if value is None:
        raise GoogleTranscribeError("word annotation is missing a timing offset")
    if isinstance(value, (int, float)):
        return round(float(value) * 1_000)
    text = str(value).strip().removesuffix("s")
    try:
        return round(float(text) * 1_000)
    except ValueError as exc:
        raise GoogleTranscribeError(f"unparseable timing offset {value!r}") from exc


def _parse(interaction: object, *, offset_ms: int) -> tuple[str, list[Word], int, int]:
    words: list[Word] = []
    texts: list[str] = []

    for step in (_field(interaction, "steps") or []):
        for content in (_field(step, "content") or []):
            if _field(content, "type") not in (None, "text"):
                continue
            text = _field(content, "text")
            if text:
                texts.append(str(text))
            for annotation in (_field(content, "annotations") or []):
                if _field(annotation, "type") != "word_info":
                    continue
                token = _field(annotation, "text")
                if not token:
                    continue
                words.append(
                    Word(
                        text=str(token).strip(),
                        start_ms=offset_ms + _offset_ms(_field(annotation, "start_offset")),
                        end_ms=offset_ms + _offset_ms(_field(annotation, "end_offset")),
                        # The model returns no per-word confidence. Recording None is
                        # the truthful answer; a synthesised 1.0 would make a
                        # low-quality span look reviewed.
                        confidence=None,
                    )
                )

    # output_text is the assembled transcript and is preferred when present; the
    # per-step text is the fallback for a response shape that does not carry it.
    text = _field(interaction, "output_text")
    transcript = str(text) if text else " ".join(texts)

    usage = _field(interaction, "usage_metadata") or _field(interaction, "usage") or {}
    return (
        transcript.strip(),
        words,
        int(_field(usage, "total_input_tokens") or 0),
        int(_field(usage, "total_output_tokens") or 0),
    )
