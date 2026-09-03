"""Sarvam AI batch ASR adapter.

Sarvam is one of the candidates the Phase 3 evaluation has to measure — with
IndicWhisper, Gemini 3.5 Transcribe and the incumbent — over 200 hand-labelled real
calls per language. It is not the default until that measurement exists, and nothing
in this file should be read as a selection. ``docs/asr-provider-selection.md`` records
where each candidate stands.

One gap in this provider is worth knowing before reading its output as evidence: the
API returns timings per sentence or phrase rather than per word, so ``ASRResult.words``
built from it is coarser than the spans ``ChannelTranscript.span_text`` is meant to
produce.

The SDK is imported inside ``__init__`` so a deployment that uses a different
provider does not have to install it.
"""

from __future__ import annotations

import io
from dataclasses import dataclass

from ..models import Word
from .base import ASRResult


@dataclass
class SarvamASR:
    api_key: str
    #: ``saarika:*`` is gone from the API's model enum; the current identifiers are
    #: ``saaras:v3`` (the API default) and ``saaras:v4``.
    model: str = "saaras:v4"
    endpoint: str = "https://api.sarvam.ai/speech-to-text"
    #: ``transcribe``, ``verbatim``, ``codemix`` and friends. Left unset by default
    #: because the API documents ``mode`` as applying to ``saaras:v3`` only, and
    #: sending a field a model ignores is how a silent behaviour change hides.
    mode: str | None = None
    timeout_s: int = 120
    #: Injected in tests. Real use imports ``requests`` in ``__post_init__``.
    session: object | None = None

    name: str = "sarvam"

    def __post_init__(self) -> None:
        if self.session is None:
            import requests  # noqa: PLC0415 - lazily imported; see the module docstring

            self.session = requests
        self.version = self.model

    def transcribe(self, audio: bytes, *, sample_rate: int,
                   language_hint: str | None = None) -> ASRResult:
        # The API takes a WAV container, not raw PCM.
        files = {"file": ("call.wav", io.BytesIO(_wav_header(len(audio), sample_rate) + audio),
                          "audio/wav")}
        data = {"model": self.model, "with_timestamps": "true"}
        if self.mode:
            data["mode"] = self.mode
        # "unknown" is the API's explicit auto-detect value. Omitting the field
        # entirely leaves the default to the service, which is a different thing to
        # ask for than detection.
        data["language_code"] = language_hint or "unknown"
        resp = self.session.post(
            self.endpoint, headers={"api-subscription-key": self.api_key},
            files=files, data=data, timeout=self.timeout_s,
        )
        resp.raise_for_status()
        payload = resp.json()
        return ASRResult(
            text=payload.get("transcript", ""),
            words=_words(payload.get("timestamps")),
            language=payload.get("language_code") or language_hint or "und",
            confidence=payload.get("language_probability"),
            provider=self.name,
            provider_version=self.version,
        )


def _words(timestamps: object) -> list[Word]:
    """Turn Sarvam's three parallel arrays into ``Word`` entries.

    The response carries ``words``, ``start_time_seconds`` and ``end_time_seconds`` as
    separate lists that line up by index, not a list of word objects — and each entry
    spans a phrase rather than a single token, which is the gap recorded in the module
    docstring.

    Anything ragged is dropped rather than guessed at. A ``Word`` whose timing came
    from a mismatched index would put a finding's evidence span over the wrong part of
    the call, which is worse than the span being absent.
    """
    if not isinstance(timestamps, dict):
        return []
    texts = timestamps.get("words") or []
    starts = timestamps.get("start_time_seconds") or []
    ends = timestamps.get("end_time_seconds") or []
    return [
        Word(text=str(text).strip(), start_ms=int(float(start) * 1000),
             end_ms=int(float(end) * 1000))
        for text, start, end in zip(texts, starts, ends)
        if str(text).strip()
    ]


def _wav_header(data_len: int, sample_rate: int, channels: int = 1, bits: int = 16) -> bytes:
    """Minimal RIFF header for 16-bit PCM.

    Written by hand rather than through ``wave`` because the audio arrives as a byte
    string already, and round-tripping it through a file-like object to add 44 bytes
    is wasted copying at 60,000 minutes a day.
    """
    import struct

    byte_rate = sample_rate * channels * bits // 8
    block_align = channels * bits // 8
    return (
        b"RIFF" + struct.pack("<I", 36 + data_len) + b"WAVEfmt "
        + struct.pack("<IHHIIHH", 16, 1, channels, sample_rate, byte_rate, block_align, bits)
        + b"data" + struct.pack("<I", data_len)
    )
