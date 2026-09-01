"""Re-export of the shared ASR result type, so adapters import from one place."""

from ..asr.base import ASRResult

__all__ = ["ASRResult"]
