"""Provider adapters.

No provider SDK is imported outside this package, and each adapter imports its own
SDK lazily inside the constructor so a deployment that uses Sarvam does not have to
install Anthropic's client to start.

Every adapter reports a ``name`` and a ``version`` that are stored alongside whatever
they produced. When a model or a prompt changes, results from before and after have
to remain distinguishable, or quality trends over time mean nothing.
"""

from .fake import FakeAnalysisProvider, FakeJudgeProvider, FakeASR

__all__ = ["FakeAnalysisProvider", "FakeJudgeProvider", "FakeASR"]
