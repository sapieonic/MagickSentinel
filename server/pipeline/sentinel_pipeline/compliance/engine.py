"""Tier-1 deterministic compliance rules.

Runs on every call. The rule definitions are data — a versioned JSON document per
tenant, matching ``contracts/schemas/rule_set.json`` — so changing a term list is a
configuration change with an audit trail, not a deploy.

Design notes worth keeping in mind when adding a rule:

* **Every finding carries a span.** A flag a reviewer cannot trace to specific words
  is not usable as evidence with a bank, so a rule that cannot point at the words it
  fired on should not fire.
* **The channel matters.** Almost every conduct rule applies to the agent, which is
  the near channel. Matching a borrower's own swearing as `abusive_language` would
  flood the queue with noise and destroy trust in the tool within a week.
* **Prefer a false negative in tier 1 to a false positive.** Tier 2 exists to catch
  the subtle cases; tier 1 exists to be unarguable.
"""

from __future__ import annotations

import json
import re
import unicodedata
from dataclasses import dataclass, field
from datetime import time as dtime
from pathlib import Path
from typing import Callable, Iterable
from zoneinfo import ZoneInfo

from ..models import CallContext, Channel, ChannelTranscript, Finding, Severity, Transcript


@dataclass(frozen=True)
class Rule:
    rule_id: str
    enabled: bool
    severity: Severity
    judge: bool = False
    params: dict = field(default_factory=dict)


@dataclass
class RuleSet:
    version: int
    rules: list[Rule]
    call_hours_start: dtime = dtime(8, 0)
    call_hours_end: dtime = dtime(19, 0)
    timezone: str = "Asia/Kolkata"
    judge_sample_pct: float = 5.0

    def rule(self, rule_id: str) -> Rule | None:
        for r in self.rules:
            if r.rule_id == rule_id:
                return r
        return None

    def judge_rules(self) -> set[str]:
        return {r.rule_id for r in self.rules if r.enabled and r.judge}


def _parse_hhmm(value: str, fallback: dtime) -> dtime:
    try:
        hh, mm = value.split(":")
        return dtime(int(hh), int(mm))
    except (ValueError, AttributeError):
        return fallback


def load_rule_set(definition: dict, version: int) -> RuleSet:
    """Build a RuleSet from a stored ``rule_sets.definition`` document."""
    hours = definition.get("call_hours") or {}
    return RuleSet(
        version=version,
        rules=[
            Rule(
                rule_id=r["rule_id"],
                enabled=bool(r.get("enabled", True)),
                severity=Severity(r.get("severity", "medium")),
                judge=bool(r.get("judge", False)),
                params=r.get("params") or {},
            )
            for r in definition.get("rules", [])
        ],
        call_hours_start=_parse_hhmm(hours.get("start", "08:00"), dtime(8, 0)),
        call_hours_end=_parse_hhmm(hours.get("end", "19:00"), dtime(19, 0)),
        timezone=hours.get("timezone", "Asia/Kolkata"),
        judge_sample_pct=float(definition.get("judge_sample_pct", 5.0)),
    )


def load_default_rule_set(path: Path | None = None) -> RuleSet:
    """Load the shipped defaults out of the migration that seeds them.

    Reading the migration rather than keeping a second copy here means the rules the
    tests exercise are the rules a new tenant actually gets.
    """
    if path is None:
        path = (
            Path(__file__).resolve().parents[4]
            / "db"
            / "migrations"
            / "0004_default_rules.up.sql"
        )
    sql = path.read_text(encoding="utf-8")
    body = sql.split("$json$")[1]
    return load_rule_set(json.loads(body), version=1)


def normalise(text: str) -> str:
    """Fold text to a comparable form, without destroying non-Latin scripts.

    Strip by Unicode category rather than by ``\\w``: ``\\w`` excludes combining
    marks, so a punctuation strip written against it deletes every Devanagari matra
    and turns ``कमीने`` into ``कम न``. Every Indic script on the support list is
    written with combining marks, so that one character class silently disabled
    native-script matching for four of the five languages the rule set covers.

    Format characters (ZWJ, ZWNJ) are dropped rather than replaced with a space:
    they appear inside Devanagari words and substituting a space would split one
    word into two.
    """
    folded = unicodedata.normalize("NFKC", text).casefold()
    kept: list[str] = []
    for ch in folded:
        category = unicodedata.category(ch)
        if ch.isspace() or category[0] in "LNM":
            kept.append(ch)
        elif category != "Cf":
            kept.append(" ")
    return re.sub(r"\s+", " ", "".join(kept)).strip()


# Romanisation variance that carries no meaning: vowel length is not written
# consistently by any ASR, and z/j, v/w, q/k are the usual transliteration splits.
_ROMAN_DIGRAPHS = (("ee", "i"), ("oo", "u"), ("ii", "i"), ("uu", "u"), ("aa", "a"))
_ROMAN_LETTERS = str.maketrans({"z": "j", "w": "v", "q": "k"})


def romanised(text: str) -> str:
    """Fold romanised Indic text to a stem, so inflections match one list entry.

    Hindi inflects where English does not — ``kamine``, ``kamina`` and ``kaminon``
    are one word — and its romanisation is not standardised, so an exact-match term
    list needs five entries per word to reach the recall one entry gets in English.
    This collapses the variance that carries no meaning: vowel length, z/j and v/w,
    doubled letters, and the oblique and plural endings.

    It does **not** try to be a transliterator or a phonetic algorithm. Genuinely
    different spellings — ``bhikhari`` and ``bhikari``, aspirated or not — stay
    different and belong in the term list as separate entries. Fold handles
    inflection; the list handles spelling.
    """
    out: list[str] = []
    for word in normalise(text).split(" "):
        for src, dst in _ROMAN_DIGRAPHS:
            word = word.replace(src, dst)
        word = word.translate(_ROMAN_LETTERS)
        word = re.sub(r"(.)\1+", r"\1", word)
        word = re.sub(r"(?:ey|ay)$", "e", word)
        word = re.sub(r"(?:on|en|o)$", "", word) or word
        word = re.sub(r"s$", "", word) or word
        word = re.sub(r"[aeiou]+$", "", word) or word
        out.append(word)
    return " ".join(out)


def indic(text: str) -> str:
    """Strip trailing combining marks, the native-script analogue of `romanised`.

    ``कमीने``/``कमीना``/``कमीनों`` differ only in the vowel sign they end on, so
    dropping trailing marks collapses them to one stem the way the romanised fold
    collapses their transliterations. Applied to both sides of the comparison, so a
    term list carries the base form and matches every case ending.
    """
    out: list[str] = []
    for word in normalise(text).split(" "):
        stripped = word
        while stripped and unicodedata.category(stripped[-1]) in ("Mn", "Mc"):
            stripped = stripped[:-1]
        out.append(stripped or word)
    return " ".join(out)


def _is_latin(text: str) -> bool:
    for ch in text:
        if ch.isalpha():
            return "LATIN" in unicodedata.name(ch, "")
    return True


@dataclass(frozen=True)
class Term:
    """One entry of a term list, and the fold that should match it.

    The fold is chosen from the term's own script rather than from the language key
    it was filed under. Rule sets are tenant-editable, so a convention that says
    "put Devanagari in `hi` and romanised Hindi in `hi-Latn`" is a convention
    somebody will break; the script of the string itself cannot be got wrong.
    """

    text: str
    language: str

    @property
    def fold(self) -> Callable[[str], str]:
        if not _is_latin(self.text):
            return indic
        # English is not inflected the way Hindi is and gains nothing from the
        # romanised fold, while an over-eager stem there costs precision.
        return normalise if self.language == "en" else romanised


def _terms(params: dict, key: str = "terms") -> list[Term]:
    """Flatten the per-language term lists, keeping the language each came from.

    Language detection on code-mixed Hinglish is unreliable, and a Hindi threat in a
    call the ASR labelled English must still fire. Matching against every language's
    list costs a little precision and buys back the recall that actually matters.
    """
    by_language = params.get(key) or {}
    out: list[Term] = []
    for language, terms in by_language.items():
        out.extend(Term(text=term, language=language) for term in terms)
    return out


@dataclass(frozen=True)
class _Hit:
    start_ms: int
    end_ms: int
    text: str


def _find_terms(t: ChannelTranscript, terms: Iterable[Term], window: tuple[int, int] | None = None) -> list[_Hit]:
    """Locate each term in a channel, returning spans in call time.

    Matching happens over the folded word stream rather than the raw text so a
    multi-word term still resolves to real timestamps. Each term brings its own fold
    (see :class:`Term`), and the haystack is folded the same way, so a romanised
    Hindi term never has to match against text that was folded for English.
    """
    if not t:
        return []
    words = t.words
    if not words:
        # No word timings: fall back to whole-text matching and report the whole
        # channel as the span. Less useful evidence, but better than dropping the
        # finding, and the ASR adapters all supply timings in practice.
        hits = []
        for term in terms:
            fold = term.fold
            needle = fold(term.text)
            if needle and needle in fold(t.text):
                hits.append(_Hit(0, 0, term.text))
        return hits

    folded: dict[Callable[[str], str], list[str]] = {}
    hits: list[_Hit] = []
    for term in terms:
        fold = term.fold
        needle = fold(term.text)
        if not needle:
            continue
        # One pass per distinct fold, not per term: three folds over a channel
        # instead of one per entry in a term list that runs to dozens.
        normalised = folded.get(fold)
        if normalised is None:
            normalised = folded[fold] = [fold(w.text) for w in words]
        parts = needle.split(" ")
        n = len(parts)
        for i in range(len(normalised) - n + 1):
            if normalised[i : i + n] != parts:
                continue
            start, end = words[i].start_ms, words[i + n - 1].end_ms
            if window and not (start < window[1] and end > window[0]):
                continue
            hits.append(_Hit(start, end, " ".join(w.text for w in words[i : i + n])))
    return hits


def _find_patterns(t: ChannelTranscript, patterns: Iterable[str]) -> list[_Hit]:
    """Regex matches over the raw text, mapped back to word timings.

    Patterns are written against readable text, not the normalised form, because
    they encode phrasing ("we will come to your house") rather than vocabulary.
    """
    if not t or not t.text:
        return []
    hits: list[_Hit] = []
    for pattern in patterns:
        try:
            rx = re.compile(pattern)
        except re.error:
            # A tenant-authored pattern that does not compile must not take the
            # whole evaluation down; the other rules still have to run.
            continue
        for m in rx.finditer(t.text):
            start_ms, end_ms = _char_span_to_time(t, m.start(), m.end())
            hits.append(_Hit(start_ms, end_ms, m.group(0)))
    return hits


def _char_span_to_time(t: ChannelTranscript, start_char: int, end_char: int) -> tuple[int, int]:
    """Map a character offset in ``text`` to the timings of the words it covers."""
    if not t.words:
        return 0, 0
    cursor = 0
    start_ms: int | None = None
    end_ms = 0
    for w in t.words:
        idx = t.text.find(w.text, cursor)
        if idx < 0:
            continue
        cursor = idx + len(w.text)
        if idx < end_char and cursor > start_char:
            if start_ms is None:
                start_ms = w.start_ms
            end_ms = w.end_ms
    if start_ms is None:
        return 0, 0
    return start_ms, end_ms


class RuleEngine:
    """Evaluates a rule set against one call."""

    def __init__(self, rule_set: RuleSet):
        self.rule_set = rule_set
        self._handlers: dict[str, Callable[[Rule, Transcript], list[Finding]]] = {
            "abusive_language": self._term_rule,
            "threat_of_violence": self._term_rule,
            "false_legal_threat": self._term_rule,
            "false_seizure_threat": self._term_rule,
            "third_party_disclosure": self._third_party_disclosure,
            "outside_call_hours": self._outside_call_hours,
            "missing_identification": self._missing_identification,
            "no_purpose_disclosure": self._no_purpose_disclosure,
            "excessive_interruption": self._excessive_interruption,
            "repeat_contact": self._repeat_contact,
        }

    def evaluate(self, transcript: Transcript) -> list[Finding]:
        """Run every enabled rule. Order is by severity, worst first."""
        findings: list[Finding] = []
        for rule in self.rule_set.rules:
            if not rule.enabled:
                continue
            handler = self._handlers.get(rule.rule_id)
            if handler is None:
                # An unknown rule id in tenant config is a configuration error, not
                # a reason to fail the call. It is skipped, and the absence shows up
                # as a rule with no findings ever.
                continue
            findings.extend(handler(rule, transcript))
        findings.sort(key=lambda f: (-f.severity.rank, f.span_start_ms or 0))
        return findings

    # ------------------------------------------------------------------ rules

    def _channel_for(self, rule: Rule, transcript: Transcript) -> ChannelTranscript | None:
        """Which channel a rule applies to. Defaults to the agent."""
        name = rule.params.get("channel", "near")
        channel = Channel.FAR if name == "far" else Channel.NEAR
        return transcript.get(channel)

    def _term_rule(self, rule: Rule, transcript: Transcript) -> list[Finding]:
        """Term list plus optional patterns, on one channel.

        One finding per rule, not per hit: a reviewer wants to see "this call
        contained a false legal threat" once, with the first and strongest piece of
        evidence, not eleven rows for eleven synonyms.
        """
        t = self._channel_for(rule, transcript)
        if t is None:
            return []
        hits = _find_terms(t, _terms(rule.params))
        hits += _find_patterns(t, rule.params.get("patterns") or [])
        if not hits:
            return []
        hits.sort(key=lambda h: h.start_ms)
        first = hits[0]
        # Widen the evidence to a readable window around the hit so a reviewer sees
        # the sentence rather than one word out of context.
        start = max(0, first.start_ms - 3_000)
        end = first.end_ms + 3_000
        return [
            Finding(
                rule_id=rule.rule_id,
                severity=rule.severity,
                tier=1,
                span_start_ms=first.start_ms,
                span_end_ms=first.end_ms,
                evidence_text=t.span_text(start, end) or first.text,
                rationale=f"{len(hits)} matching phrase(s) on the {t.channel.speaker} channel",
            )
        ]

    def _third_party_disclosure(self, rule: Rule, transcript: Transcript) -> list[Finding]:
        """Debt details discussed after the other party denied being the borrower.

        Sequence matters: a denial followed by a disclosure is the violation. A
        disclosure followed by a denial is the agent finding out mid-call, which is
        awkward but not a breach, and flagging it would train reviewers to dismiss
        this rule.
        """
        near, far = transcript.near, transcript.far
        if near is None or far is None:
            return []
        denials = _find_terms(far, _terms(rule.params, "denial_terms"))
        if not denials:
            return []
        first_denial = min(d.start_ms for d in denials)
        window = int(rule.params.get("window_ms", 120_000))
        disclosures = [
            h
            for h in _find_terms(near, _terms(rule.params, "disclosure_terms"))
            if first_denial <= h.start_ms <= first_denial + window
        ]
        if not disclosures:
            return []
        first = min(disclosures, key=lambda h: h.start_ms)
        return [
            Finding(
                rule_id=rule.rule_id,
                severity=rule.severity,
                tier=1,
                span_start_ms=first.start_ms,
                span_end_ms=first.end_ms,
                evidence_text=near.span_text(max(0, first.start_ms - 3_000), first.end_ms + 3_000),
                rationale=(
                    "the other party denied being the borrower at "
                    f"{first_denial} ms and account details followed"
                ),
            )
        ]

    def _outside_call_hours(self, rule: Rule, transcript: Transcript) -> list[Finding]:
        """Structural: no transcript needed, only the start time in tenant time."""
        ctx = transcript.context
        tz = ZoneInfo(self.rule_set.timezone or ctx.tenant_timezone)
        local = ctx.started_at.astimezone(tz)
        start, end = self.rule_set.call_hours_start, self.rule_set.call_hours_end
        within = start <= local.time() < end
        if within:
            return []
        return [
            Finding(
                rule_id=rule.rule_id,
                severity=rule.severity,
                tier=1,
                span_start_ms=0,
                span_end_ms=0,
                evidence_text=f"call placed at {local.strftime('%H:%M %Z')}",
                rationale=(
                    f"outside the permitted window {start.strftime('%H:%M')}"
                    f"–{end.strftime('%H:%M')} {self.rule_set.timezone}"
                ),
            )
        ]

    def _missing_identification(self, rule: Rule, transcript: Transcript) -> list[Finding]:
        """The agent must state who they are and who they represent, early.

        A short call that never connected is not an identification failure; there is
        nobody to identify oneself to. The window guard below is what keeps voicemail
        and immediate hangups out of the queue.
        """
        near = transcript.near
        if near is None:
            return []
        window = int(rule.params.get("window_ms", 30_000))
        if transcript.context.duration_ms < window:
            return []
        opening = near.span_text(0, window)
        if not opening.strip():
            return []
        agency_terms = _terms(rule.params, "agency_terms")
        said_agency = any(
            term.fold(term.text) in term.fold(opening) for term in agency_terms
        )
        # A name is any capitalised token that is not a sentence opener; ASR output
        # is unreliable about casing, so this is a weak signal and is only used to
        # avoid flagging a call that clearly did introduce someone.
        said_name = bool(re.search(r"\b(?:my name is|this is|i am|main)\b", opening, re.IGNORECASE))
        if said_agency and said_name:
            return []
        missing = []
        if not said_name:
            missing.append("agent name")
        if not said_agency:
            missing.append("agency")
        return [
            Finding(
                rule_id=rule.rule_id,
                severity=rule.severity,
                tier=1,
                span_start_ms=0,
                span_end_ms=window,
                evidence_text=opening,
                rationale=f"not stated in the first {window // 1000} s: {', '.join(missing)}",
            )
        ]

    def _no_purpose_disclosure(self, rule: Rule, transcript: Transcript) -> list[Finding]:
        near = transcript.near
        if near is None:
            return []
        window = int(rule.params.get("window_ms", 60_000))
        if transcript.context.duration_ms < window:
            return []
        opening = near.span_text(0, window)
        if not opening.strip():
            return []
        if _find_terms(near, _terms(rule.params), window=(0, window)):
            return []
        return [
            Finding(
                rule_id=rule.rule_id,
                severity=rule.severity,
                tier=1,
                span_start_ms=0,
                span_end_ms=window,
                evidence_text=opening,
                rationale=f"purpose of the call not stated in the first {window // 1000} s",
            )
        ]

    def _excessive_interruption(self, rule: Rule, transcript: Transcript) -> list[Finding]:
        count = transcript.context.interruptions
        threshold = int(rule.params.get("threshold", 12))
        if count is None or count <= threshold:
            return []
        return [
            Finding(
                rule_id=rule.rule_id,
                severity=rule.severity,
                tier=1,
                rationale=f"{count} interruptions, threshold {threshold}",
                evidence_text=f"{count} interruptions",
            )
        ]

    def _repeat_contact(self, rule: Rule, transcript: Transcript) -> list[Finding]:
        ctx = transcript.context
        threshold = int(rule.params.get("threshold", 3))
        if ctx.account_ref is None or ctx.prior_contacts_24h <= threshold:
            return []
        return [
            Finding(
                rule_id=rule.rule_id,
                severity=rule.severity,
                tier=1,
                rationale=(
                    f"{ctx.prior_contacts_24h} calls to this account in 24 h, "
                    f"threshold {threshold}"
                ),
                evidence_text=f"{ctx.prior_contacts_24h} calls in 24 h",
            )
        ]
