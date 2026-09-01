"""faster-whisper adapter, for IndicWhisper and Whisper checkpoints.

Runs locally, which matters for the data-residency question (OPEN-4): a customer that
will not let audio leave India can run this on Indian infrastructure without a
third-party API in the path at all.
"""

from __future__ import annotations

import tempfile
from dataclasses import dataclass, field

from ..models import Word
from .base import ASRResult
from .sarvam import _wav_header


@dataclass
class WhisperASR:
    model_path: str = "large-v3"
    device: str = "cpu"
    compute_type: str = "int8"
    beam_size: int = 5
    name: str = "whisper"
    _model: object = field(default=None, init=False, repr=False)

    def __post_init__(self) -> None:
        from faster_whisper import WhisperModel  # noqa: PLC0415 - lazy, see providers/__init__

        self._model = WhisperModel(self.model_path, device=self.device,
                                   compute_type=self.compute_type)
        self.version = self.model_path

    def transcribe(self, audio: bytes, *, sample_rate: int,
                   language_hint: str | None = None) -> ASRResult:
        with tempfile.NamedTemporaryFile(suffix=".wav") as fh:
            fh.write(_wav_header(len(audio), sample_rate))
            fh.write(audio)
            fh.flush()
            segments, info = self._model.transcribe(
                fh.name, language=language_hint, beam_size=self.beam_size,
                word_timestamps=True,
                # Telephony audio is band-limited and noisy; without VAD filtering
                # Whisper hallucinates fluent sentences over silence, which on a
                # collections call can invent an amount that was never said.
                vad_filter=True,
            )
            words: list[Word] = []
            texts: list[str] = []
            for seg in segments:
                texts.append(seg.text.strip())
                for w in (seg.words or []):
                    words.append(Word(text=w.word.strip(),
                                      start_ms=int(w.start * 1000),
                                      end_ms=int(w.end * 1000),
                                      confidence=getattr(w, "probability", None)))
        return ASRResult(
            text=" ".join(texts).strip(),
            words=words,
            language=getattr(info, "language", language_hint or "hi"),
            confidence=getattr(info, "language_probability", None),
            provider=self.name,
            provider_version=self.version,
        )
