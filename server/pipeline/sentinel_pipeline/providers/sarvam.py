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
    model: str = "saarika:v2"
    endpoint: str = "https://api.sarvam.ai/speech-to-text"
    timeout_s: int = 120

    name: str = "sarvam"

    def __post_init__(self) -> None:
        import requests  # noqa: PLC0415 - lazily imported; see the module docstring

        self._requests = requests
        self.version = self.model

    def transcribe(self, audio: bytes, *, sample_rate: int,
                   language_hint: str | None = None) -> ASRResult:
        # The API takes a WAV container, not raw PCM.
        files = {"file": ("call.wav", io.BytesIO(_wav_header(len(audio), sample_rate) + audio),
                          "audio/wav")}
        data = {"model": self.model, "with_timestamps": "true"}
        if language_hint:
            data["language_code"] = language_hint
        resp = self._requests.post(
            self.endpoint, headers={"api-subscription-key": self.api_key},
            files=files, data=data, timeout=self.timeout_s,
        )
        resp.raise_for_status()
        payload = resp.json()
        words = [
            Word(text=w["word"], start_ms=int(w["start"] * 1000), end_ms=int(w["end"] * 1000))
            for w in payload.get("timestamps", {}).get("words", [])
        ]
        return ASRResult(
            text=payload.get("transcript", ""),
            words=words,
            language=payload.get("language_code", language_hint or "hi"),
            provider=self.name,
            provider_version=self.version,
        )


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
