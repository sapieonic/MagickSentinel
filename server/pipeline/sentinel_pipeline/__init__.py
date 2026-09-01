"""Sentinel analysis pipeline.

Three swappable provider slots sit behind interfaces — ASR, analysis, compliance
judge — and no provider SDK is imported outside its adapter in
:mod:`sentinel_pipeline.providers`. Every stored artifact records the provider and
version that produced it: when a prompt or model changes, results from before and
after must stay distinguishable, or quality trends become meaningless.
"""

__version__ = "0.1.0"
