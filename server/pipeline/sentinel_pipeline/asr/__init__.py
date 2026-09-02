"""Speech recognition, in two paths.

**Streaming** feeds the live widget only. It is never persisted and never shown in
the portal: a rough live transcript displayed as a final output makes the product
look broken, and a supervisor who reads one will not trust the finished transcript
either.

**Batch** runs after ``call.end`` over the whole call, and is the transcript
everything downstream uses — analysis, compliance, search, evidence packs.

ASR quality is the top technical risk on this project. The input is code-mixed
Hinglish, Telugu, Tamil and Marathi collections speech over compressed telephony
audio, dense with amounts and dates that have to be exact. A promise to pay of
₹15,000 misheard as ₹50,000 destroys trust in the whole product, so
:mod:`sentinel_pipeline.asr.evaluate` tracks a numeric-entity error rate separately
from WER: overall WER can look fine while the amounts are wrong.

Because the two channels were captured separately there is **no diarization step**,
and there must not be one.
"""

from .base import ASRResult, BatchASR, StreamingASR
from .evaluate import EvaluationResult, numeric_entity_error_rate, word_error_rate

__all__ = [
    "ASRResult",
    "BatchASR",
    "StreamingASR",
    "EvaluationResult",
    "word_error_rate",
    "numeric_entity_error_rate",
]
