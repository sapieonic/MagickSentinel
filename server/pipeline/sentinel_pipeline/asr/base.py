"""ASR provider interfaces.

Providers live in :mod:`sentinel_pipeline.providers` and no provider SDK is imported
outside its adapter. Everything a provider returns carries the provider name and
version, because when a model changes, results from before and after have to stay
distinguishable or quality trends become meaningless.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Iterable, Protocol

from ..models import Channel, ChannelTranscript, Word


@dataclass
class ASRResult:
    text: str
    words: list[Word] = field(default_factory=list)
    language: str = "hi"
    confidence: float | None = None
    provider: str = "unknown"
    provider_version: str = "unknown"

    def to_channel_transcript(self, channel: Channel) -> ChannelTranscript:
        return ChannelTranscript(
            channel=channel,
            text=self.text,
            words=self.words,
            language=self.language,
            provider=self.provider,
            provider_version=self.provider_version,
            confidence=self.confidence,
        )


class BatchASR(Protocol):
    """Transcribes a complete channel of a finished call."""

    name: str
    version: str

    def transcribe(self, audio: bytes, *, sample_rate: int, language_hint: str | None = None) -> ASRResult:
        ...


class StreamingASR(Protocol):
    """Low-latency partial transcription for the live widget only.

    Output is never persisted and never reaches the portal.
    """

    name: str
    version: str

    def stream(self, chunks: Iterable[bytes], *, sample_rate: int) -> Iterable[str]:
        ...
