"""Compliance evaluation, in two tiers.

Tier 1 is deterministic and runs on 100% of calls: term lists, patterns and
structural checks over the transcript and call metadata. Cheap, fast, and close to
zero false negatives on the obvious violations.

Tier 2 is an LLM judge that runs only on calls tier 1 flagged plus a small random
sample. It catches misrepresentation and coercive framing that pattern matching
misses, and it must return the transcript span it relied on.
"""

from .engine import RuleEngine, RuleSet, load_rule_set
from .judge import ComplianceJudge, JudgeVerdict, should_judge

__all__ = [
    "RuleEngine",
    "RuleSet",
    "load_rule_set",
    "ComplianceJudge",
    "JudgeVerdict",
    "should_judge",
]
