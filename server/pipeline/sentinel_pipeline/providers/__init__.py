"""Provider adapters.

No provider SDK is imported outside this package, and each adapter imports its own
SDK lazily inside the constructor so a deployment that uses Sarvam does not have to
install Anthropic's client to start.

Every adapter reports a ``name`` and a ``version`` that are stored alongside whatever
they produced. When a model or a prompt changes, results from before and after have
to remain distinguishable, or quality trends over time mean nothing.

:mod:`sentinel_pipeline.providers.registry` is the one place that decides *which*
batch ASR provider gets built. The default is ``gemini-3.5-transcribe``; see
``docs/asr-provider-selection.md`` for why, and for what it cannot do.
"""

from .fake import FakeAnalysisProvider, FakeASR, FakeJudgeProvider
from .registry import (
    DEFAULT_BATCH_ASR,
    ASRSettings,
    Capabilities,
    LanguageRoutedASR,
    ProviderConfigError,
    build_batch_asr,
    settings_from_env,
    validate,
    warnings_for,
)

__all__ = [
    "DEFAULT_BATCH_ASR",
    "ASRSettings",
    "Capabilities",
    "FakeASR",
    "FakeAnalysisProvider",
    "FakeJudgeProvider",
    "LanguageRoutedASR",
    "ProviderConfigError",
    "build_batch_asr",
    "settings_from_env",
    "validate",
    "warnings_for",
]
