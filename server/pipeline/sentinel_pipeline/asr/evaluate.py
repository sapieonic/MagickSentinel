"""ASR evaluation metrics.

Two numbers, tracked separately and from the first evaluation rather than after the
first customer complaint:

* **WER** — the usual word error rate, which tells you whether the model can follow
  the conversation at all.
* **Numeric-entity error rate** — how often an amount, a date or an account number
  came out wrong. This is the one that decides whether the product is usable.
  Overall WER can sit at a respectable 18% while every third amount is wrong,
  because numbers are a tiny fraction of the tokens and carry all of the meaning.

The required evaluation set before Phase 3 exit is 200 hand-labelled real calls per
language, measured against Sarvam, IndicWhisper and the incumbent.
"""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass

# Devanagari and Latin digits both appear in Indic ASR output.
_DIGITS = "0-9०-९"

# Amounts, dates and bare numbers. Spelled-out numbers are normalised separately.
_NUMERIC = re.compile(
    rf"""(?xi)
    (?: (?:rs|inr|₹)\s*[{_DIGITS},]+(?:\.[{_DIGITS}]+)? )   # ₹15,000
  | (?: [{_DIGITS}]{{1,2}}[/-][{_DIGITS}]{{1,2}}[/-][{_DIGITS}]{{2,4}} )  # 15/09/2026
  | (?: [{_DIGITS}][{_DIGITS},]*(?:\.[{_DIGITS}]+)? )       # 15000, 15,000.50
    """
)

# Collections speech says amounts aloud far more often than it says digits, so the
# scale words have to normalise or every spoken amount reads as a mismatch.
_SCALE = {
    "hazaar": "thousand", "hazar": "thousand", "hajar": "thousand",
    "lakh": "lakh", "lac": "lakh", "lakhs": "lakh",
    "crore": "crore", "crores": "crore",
    "thousand": "thousand", "hundred": "hundred",
}


def normalise_token(token: str) -> str:
    folded = unicodedata.normalize("NFKC", token).casefold()
    folded = folded.strip(".,!?;:।")
    # Strip the thousands separators an ASR may or may not insert.
    if re.fullmatch(rf"[{_DIGITS},]+", folded):
        folded = folded.replace(",", "")
    return _SCALE.get(folded, folded)


def tokenise(text: str) -> list[str]:
    return [t for t in (normalise_token(t) for t in text.split()) if t]


def _levenshtein(a: list[str], b: list[str]) -> int:
    if not a:
        return len(b)
    if not b:
        return len(a)
    previous = list(range(len(b) + 1))
    for i, ai in enumerate(a, start=1):
        current = [i]
        for j, bj in enumerate(b, start=1):
            current.append(min(
                previous[j] + 1,          # deletion
                current[j - 1] + 1,       # insertion
                previous[j - 1] + (ai != bj),  # substitution
            ))
        previous = current
    return previous[-1]


def word_error_rate(reference: str, hypothesis: str) -> float:
    """Standard WER: edit distance over tokens, divided by reference length."""
    ref, hyp = tokenise(reference), tokenise(hypothesis)
    if not ref:
        return 0.0 if not hyp else 1.0
    return _levenshtein(ref, hyp) / len(ref)


def numeric_entities(text: str) -> list[str]:
    """Every numeric entity in a string, normalised for comparison."""
    out = []
    for m in _NUMERIC.finditer(text):
        token = m.group(0)
        token = re.sub(r"(?i)^(rs|inr|₹)\s*", "", token).replace(",", "")
        out.append(unicodedata.normalize("NFKC", token).casefold())
    return out


def numeric_entity_error_rate(reference: str, hypothesis: str) -> float:
    """Fraction of the reference's numeric entities the hypothesis got wrong.

    Multiset comparison, not set: a call that says "fifteen thousand" twice and is
    transcribed with one of them wrong is half wrong, not correct.
    """
    ref = numeric_entities(reference)
    if not ref:
        return 0.0
    hyp = list(numeric_entities(hypothesis))
    missed = 0
    for entity in ref:
        if entity in hyp:
            hyp.remove(entity)
        else:
            missed += 1
    return missed / len(ref)


@dataclass
class EvaluationResult:
    language: str
    provider: str
    provider_version: str
    calls: int
    wer: float
    numeric_error_rate: float

    def summary(self) -> str:
        return (
            f"{self.provider} {self.provider_version} / {self.language}: "
            f"WER {self.wer:.1%}, numeric errors {self.numeric_error_rate:.1%} "
            f"over {self.calls} calls"
        )


def evaluate(pairs: list[tuple[str, str]], *, language: str, provider: str,
             provider_version: str) -> EvaluationResult:
    """Score a provider over a labelled set of (reference, hypothesis) pairs.

    Both metrics are weighted by reference length rather than averaged per call, so
    one short call cannot swing the number.
    """
    total_ref_tokens = 0
    total_errors = 0
    total_ref_entities = 0
    total_entity_errors = 0
    for reference, hypothesis in pairs:
        ref = tokenise(reference)
        total_ref_tokens += len(ref)
        total_errors += _levenshtein(ref, tokenise(hypothesis))
        entities = numeric_entities(reference)
        total_ref_entities += len(entities)
        total_entity_errors += round(numeric_entity_error_rate(reference, hypothesis) * len(entities))
    return EvaluationResult(
        language=language,
        provider=provider,
        provider_version=provider_version,
        calls=len(pairs),
        wer=(total_errors / total_ref_tokens) if total_ref_tokens else 0.0,
        numeric_error_rate=(
            total_entity_errors / total_ref_entities if total_ref_entities else 0.0
        ),
    )
