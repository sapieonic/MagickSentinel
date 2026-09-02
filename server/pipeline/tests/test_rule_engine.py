"""Tier-1 rule engine, against a fixture corpus.

The corpus is deliberately split into calls that must fire and calls that must not.
Precision matters as much as recall here: a compliance queue that cries wolf gets
ignored, and an ignored queue is worse than no queue because the customer believes
they are covered.
"""

import re
from datetime import datetime, timezone

import pytest

from conftest import COMPLIANT_OPENING, call, channel
from sentinel_pipeline.compliance.engine import RuleEngine, load_rule_set, normalise
from sentinel_pipeline.models import Channel, Severity


@pytest.fixture
def engine(default_rules):
    return RuleEngine(default_rules)


def rule_ids(findings):
    return {f.rule_id for f in findings}


# --------------------------------------------------------------- must fire


def test_false_legal_threat_fires_with_a_traceable_span(engine):
    near = channel(
        Channel.NEAR,
        (0, COMPLIANT_OPENING),
        (30_000, "If you do not pay today we will file a police case against you."),
    )
    findings = engine.evaluate(call(near=near, far=channel(Channel.FAR, (10_000, "I understand."))))
    hit = next(f for f in findings if f.rule_id == "false_legal_threat")
    assert hit.severity is Severity.CRITICAL
    assert hit.tier == 1
    assert hit.span_start_ms is not None and hit.span_start_ms >= 30_000
    assert "police case" in hit.evidence_text.lower(), hit.evidence_text


def test_threat_of_violence_fires_on_a_pattern_not_just_a_term(engine):
    near = channel(
        Channel.NEAR,
        (0, COMPLIANT_OPENING),
        (40_000, "We will come to your house tomorrow and you will regret this."),
    )
    findings = engine.evaluate(call(near=near))
    assert "threat_of_violence" in rule_ids(findings)


def test_abusive_language_fires_on_the_agent_channel(engine):
    near = channel(
        Channel.NEAR,
        (0, COMPLIANT_OPENING),
        (50_000, "You are a shameless thief and everyone should know it."),
    )
    findings = engine.evaluate(call(near=near))
    hit = next(f for f in findings if f.rule_id == "abusive_language")
    assert hit.severity is Severity.CRITICAL


def test_seizure_threat_fires(engine):
    near = channel(
        Channel.NEAR,
        (0, COMPLIANT_OPENING),
        (60_000, "We will seize your car this week if the amount is not cleared."),
    )
    assert "false_seizure_threat" in rule_ids(engine.evaluate(call(near=near)))


def test_third_party_disclosure_fires_only_after_a_denial(engine):
    far = channel(Channel.FAR, (5_000, "Wrong number, he is not here, I am the neighbour."))
    near = channel(
        Channel.NEAR,
        (0, "Hello, is that Mr Kumar?"),
        (12_000, "His outstanding is fifteen thousand rupees, please pass on the message."),
    )
    assert "third_party_disclosure" in rule_ids(engine.evaluate(call(near=near, far=far)))


def test_disclosure_before_the_denial_is_not_a_breach(engine):
    # The agent stated the balance and only then learned they had the wrong person.
    # Flagging this would train reviewers to dismiss the rule.
    near = channel(
        Channel.NEAR,
        (0, COMPLIANT_OPENING),
        (5_000, "Your outstanding is fifteen thousand rupees."),
    )
    far = channel(Channel.FAR, (30_000, "Wrong number, he is not here."))
    assert "third_party_disclosure" not in rule_ids(engine.evaluate(call(near=near, far=far)))


def test_outside_call_hours_is_structural_and_needs_no_transcript(engine):
    late = datetime(2026, 9, 1, 16, 30, tzinfo=timezone.utc)  # 22:00 IST
    findings = engine.evaluate(call(near=channel(Channel.NEAR, (0, COMPLIANT_OPENING)),
                                    started_at=late))
    hit = next(f for f in findings if f.rule_id == "outside_call_hours")
    assert hit.severity is Severity.HIGH
    assert "22:00" in hit.evidence_text


def test_early_morning_call_is_outside_hours(engine):
    early = datetime(2026, 9, 1, 1, 0, tzinfo=timezone.utc)  # 06:30 IST
    assert "outside_call_hours" in rule_ids(
        engine.evaluate(call(near=channel(Channel.NEAR, (0, COMPLIANT_OPENING)), started_at=early))
    )


def test_missing_identification_fires_when_the_agent_launches_straight_in(engine):
    near = channel(Channel.NEAR, (0, "You owe money. When are you paying? This is the third time."))
    findings = engine.evaluate(call(near=near))
    hit = next(f for f in findings if f.rule_id == "missing_identification")
    assert hit.severity is Severity.MEDIUM
    assert hit.span_end_ms == 30_000


def test_no_purpose_disclosure_fires(engine):
    near = channel(
        Channel.NEAR,
        (0, "Hello, my name is Ravi and I am calling from Acme Recovery Services."),
        (5_000, "How is the weather there? Is this a good time to speak with you today?"),
    )
    assert "no_purpose_disclosure" in rule_ids(engine.evaluate(call(near=near)))


def test_excessive_interruption_uses_the_analysis_count(engine):
    findings = engine.evaluate(
        call(near=channel(Channel.NEAR, (0, COMPLIANT_OPENING)), interruptions=30)
    )
    hit = next(f for f in findings if f.rule_id == "excessive_interruption")
    assert hit.severity is Severity.LOW
    assert "30 interruptions" in hit.rationale


def test_repeat_contact_fires_above_the_threshold(engine):
    findings = engine.evaluate(
        call(near=channel(Channel.NEAR, (0, COMPLIANT_OPENING)), prior_contacts_24h=7)
    )
    assert "repeat_contact" in rule_ids(findings)


# ----------------------------------------------------------- must not fire


def test_a_clean_call_produces_no_findings(engine):
    near = channel(
        Channel.NEAR,
        (0, COMPLIANT_OPENING),
        (20_000, "Can you tell me when you will be able to make the payment?"),
        (60_000, "Thank you, I have noted fifteen thousand rupees on the fifteenth."),
    )
    far = channel(
        Channel.FAR,
        (12_000, "Yes speaking."),
        (40_000, "I can pay fifteen thousand on the fifteenth of this month."),
    )
    assert engine.evaluate(call(near=near, far=far)) == []


def test_a_borrower_swearing_is_never_the_agents_violation(engine):
    # This is the single most important precision case: borrowers on a collections
    # floor are frequently abusive, and flagging the agent for it would make every
    # difficult call a compliance incident.
    near = channel(
        Channel.NEAR,
        (0, COMPLIANT_OPENING),
        (30_000, "I understand you are upset. Can we agree a date?"),
    )
    far = channel(
        Channel.FAR,
        (20_000, "You are a thief and a shameless bastard, do not call me again."),
        (45_000, "I will send goons to your office."),
    )
    findings = engine.evaluate(call(near=near, far=far))
    assert rule_ids(findings) == set(), findings


def test_describing_a_lawful_process_is_not_a_seizure_threat(engine):
    near = channel(
        Channel.NEAR,
        (0, COMPLIANT_OPENING),
        (30_000, "If this stays unpaid the lender may begin recovery proceedings "
                 "through the tribunal, and you would be served notice first."),
    )
    assert "false_seizure_threat" not in rule_ids(engine.evaluate(call(near=near)))


def test_a_short_unconnected_call_is_not_an_identification_failure(engine):
    # Ten seconds of ringback and a hangup. There was nobody to identify oneself to.
    near = channel(Channel.NEAR, (0, "Hello?"))
    findings = engine.evaluate(call(near=near, duration_ms=9_000))
    assert "missing_identification" not in rule_ids(findings)
    assert "no_purpose_disclosure" not in rule_ids(findings)


def test_a_call_at_the_edge_of_the_window_is_inside_it(engine):
    at_open = datetime(2026, 9, 1, 2, 30, tzinfo=timezone.utc)   # 08:00 IST exactly
    before_close = datetime(2026, 9, 1, 13, 29, tzinfo=timezone.utc)  # 18:59 IST
    for when in (at_open, before_close):
        findings = engine.evaluate(
            call(near=channel(Channel.NEAR, (0, COMPLIANT_OPENING)), started_at=when)
        )
        assert "outside_call_hours" not in rule_ids(findings), when


def test_repeat_contact_needs_an_account_reference(engine):
    # Without an account_ref there is nothing to count against, and guessing would
    # produce flags nobody can investigate.
    findings = engine.evaluate(
        call(near=channel(Channel.NEAR, (0, COMPLIANT_OPENING)),
             account_ref=None, prior_contacts_24h=9)
    )
    assert "repeat_contact" not in rule_ids(findings)


# ------------------------------------------------------------- mechanics


def test_findings_are_ordered_worst_first(engine):
    near = channel(
        Channel.NEAR,
        (0, "You will be arrested tomorrow."),
        (40_000, "We will seize your car."),
    )
    findings = engine.evaluate(call(near=near, interruptions=40))
    ranks = [f.severity.rank for f in findings]
    assert ranks == sorted(ranks, reverse=True), [(f.rule_id, f.severity) for f in findings]


def test_one_finding_per_rule_not_one_per_synonym(engine):
    near = channel(
        Channel.NEAR,
        (0, COMPLIANT_OPENING),
        (30_000, "There will be a police case, a criminal case, and a warrant, "
                 "and you will go to jail."),
    )
    findings = [f for f in engine.evaluate(call(near=near)) if f.rule_id == "false_legal_threat"]
    assert len(findings) == 1, "a reviewer wants one row per violation, not one per synonym"
    # The count is still reported, so the reviewer can see it was not a single slip.
    assert re.match(r"^[2-9]\d* matching phrase", findings[0].rationale), findings[0].rationale
    # The evidence points at the first hit, not the last.
    assert findings[0].span_start_ms >= 30_000


def test_normalisation_makes_matching_script_and_punctuation_insensitive():
    assert normalise("Police  Case!!") == normalise("police case")
    assert normalise("गिरफ्तार,") == normalise("गिरफ्तार")
    # The two assertions above both passed while normalise() was deleting every
    # Devanagari matra, because it deleted them from both sides. Comparing against
    # the literal is what actually pins the property.
    assert normalise("गिरफ्तार,") == "गिरफ्तार"


# -------------------------------------------------------- indian languages
#
# The rule set ships each non-English term in both its native script and a
# romanisation, because ASR output for one Hinglish call is not consistently in one
# script. These pin the folds that make one list entry cover both, and the price
# paid for them.


def test_normalise_keeps_the_combining_marks_devanagari_is_written_with():
    # The regression that motivated all of this: a punctuation strip written as
    # [^\w\s] deletes combining marks, because \w does not match category Mn.
    # कमीने became "कम न" and no Hindi term could ever match.
    assert normalise("कमीने") == "कमीने"
    assert normalise("तुम कमीने हो!") == "तुम कमीने हो"
    assert normalise("बदतमीज़") == "बदतमीज़", "the nukta is a combining mark too"
    assert normalise("முட்டாள்") == "முட்டாள்"
    assert normalise("సిగ్గు లేని") == "సిగ్గు లేని"


def test_zero_width_joiners_do_not_split_a_word_in_two():
    # ZWNJ is a format character and appears inside Devanagari words. Replacing it
    # with a space would turn one word into two and break a single-word term.
    assert normalise("क\u200dमीने") == "कमीने"


def test_devanagari_abuse_fires_the_same_rule_as_its_romanisation(engine):
    near = channel(Channel.NEAR, (0, COMPLIANT_OPENING), (50_000, "तुम कमीने हो"))
    assert "abusive_language" in rule_ids(engine.evaluate(call(near=near)))


@pytest.mark.parametrize("form", ["kamine", "kaminey", "kameene", "kamina", "kaminon"])
def test_romanised_inflections_all_match_one_list_entry(engine, form):
    # Hindi inflects where English does not, and its romanisation is not
    # standardised. Without the fold each of these needs its own list entry, and
    # whichever one nobody thought of is a silent miss.
    near = channel(Channel.NEAR, (0, COMPLIANT_OPENING), (50_000, f"tum {form} ho"))
    assert "abusive_language" in rule_ids(engine.evaluate(call(near=near)))


@pytest.mark.parametrize("form", ["कमीने", "कमीना", "कमीनों", "कुत्ते", "कुत्ता", "कुत्तों"])
def test_devanagari_case_endings_all_match_one_list_entry(engine, form):
    near = channel(Channel.NEAR, (0, COMPLIANT_OPENING), (50_000, f"तुम {form} हो"))
    assert "abusive_language" in rule_ids(engine.evaluate(call(near=near)))


def test_tamil_and_telugu_fire_in_their_own_scripts(engine):
    for text in ("நீ முட்டாள்", "నువ్వు దొంగోడు"):
        near = channel(Channel.NEAR, (0, COMPLIANT_OPENING), (50_000, text))
        assert "abusive_language" in rule_ids(engine.evaluate(call(near=near))), text


def test_hindi_threats_fire_in_devanagari(engine):
    near = channel(Channel.NEAR, (0, COMPLIANT_OPENING), (30_000, "हम तुम्हें जेल भेज देंगे"))
    assert "false_legal_threat" in rule_ids(engine.evaluate(call(near=near)))


def test_a_devanagari_opening_is_a_compliant_identification(engine):
    # The mirror of the rules above: the folds must also let an agent who did the
    # right thing in Hindi off the hook, or the queue fills with correct calls.
    near = channel(
        Channel.NEAR,
        (0, "नमस्ते मेरा नाम रवि है मैं एक्मे रिकवरी की तरफ से बोल रहा हूँ "
            "आपके लोन के बारे में बात करनी है"),
    )
    fired = rule_ids(engine.evaluate(call(near=near)))
    assert "no_purpose_disclosure" not in fired


def test_the_loose_fold_is_not_applied_to_english_terms(engine):
    # English is not inflected the way Hindi is and gains nothing from the stem,
    # while an over-eager fold there costs precision on the language most of the
    # list is written in. "cheats" must not match the entry "cheat".
    near = channel(Channel.NEAR, (0, COMPLIANT_OPENING), (50_000, "The lender cheats nobody."))
    assert "abusive_language" not in rule_ids(engine.evaluate(call(near=near)))


def test_the_romanised_fold_costs_precision_and_the_judge_is_why_that_is_affordable(engine):
    # A known, accepted collision: "chore" folds to the same stem as the Hindi
    # "chor". It is pinned rather than hidden — every rule that uses the loose fold
    # on a conduct term carries judge:true, so tier 2 sees this one and dismisses
    # it. If the fold is ever tightened, this test is the record of what changed.
    near = channel(Channel.NEAR, (0, COMPLIANT_OPENING), (50_000, "That is not my chore."))
    assert "abusive_language" in rule_ids(engine.evaluate(call(near=near)))


def test_the_folds_do_not_fire_on_an_ordinary_english_collections_call(engine):
    near = channel(
        Channel.NEAR,
        (0, COMPLIANT_OPENING),
        (40_000, "Your outstanding balance is fifteen thousand rupees and the payment "
                 "was due on the second. Can you transfer it this week?"),
    )
    far = channel(Channel.FAR, (60_000, "Yes, I will transfer it on Friday."))
    assert rule_ids(engine.evaluate(call(near=near, far=far))) == set()


def test_hindi_terms_match_even_when_the_asr_labelled_the_call_english(engine):
    # Code-mixed Hinglish is the norm, and language detection on it is unreliable.
    # A Hindi threat in a call tagged 'en' still has to fire.
    near = channel(Channel.NEAR, (0, COMPLIANT_OPENING), (30_000, "Hum tumhe jail bhej denge."))
    near.language = "en"
    assert "false_legal_threat" in rule_ids(engine.evaluate(call(near=near)))


def test_a_disabled_rule_does_not_fire():
    definition = {
        "rules": [
            {"rule_id": "abusive_language", "enabled": False, "severity": "critical",
             "params": {"terms": {"en": ["thief"]}}},
        ]
    }
    engine = RuleEngine(load_rule_set(definition, version=2))
    near = channel(Channel.NEAR, (0, "You are a thief."))
    assert engine.evaluate(call(near=near)) == []


def test_a_tenant_can_override_severity():
    definition = {
        "rules": [
            {"rule_id": "abusive_language", "enabled": True, "severity": "low",
             "params": {"terms": {"en": ["thief"]}}},
        ]
    }
    engine = RuleEngine(load_rule_set(definition, version=2))
    findings = engine.evaluate(call(near=channel(Channel.NEAR, (0, "You are a thief."))))
    assert findings[0].severity is Severity.LOW


def test_an_unparseable_tenant_pattern_does_not_break_the_evaluation():
    definition = {
        "rules": [
            {"rule_id": "threat_of_violence", "enabled": True, "severity": "critical",
             "params": {"patterns": ["([unclosed"], "terms": {"en": ["break your legs"]}}},
        ]
    }
    engine = RuleEngine(load_rule_set(definition, version=2))
    findings = engine.evaluate(call(near=channel(Channel.NEAR, (0, "I will break your legs."))))
    assert rule_ids(findings) == {"threat_of_violence"}


def test_an_unknown_rule_id_is_skipped_rather_than_crashing():
    definition = {"rules": [{"rule_id": "not_a_real_rule", "enabled": True, "severity": "high"}]}
    engine = RuleEngine(load_rule_set(definition, version=2))
    assert engine.evaluate(call(near=channel(Channel.NEAR, (0, "anything")))) == []


def test_a_missing_channel_does_not_crash_the_engine(engine):
    # Tier B suppression can leave a call with far-channel audio only.
    assert engine.evaluate(call(near=None, far=channel(Channel.FAR, (0, "hello")))) is not None


def test_transcripts_without_word_timings_still_match(engine, make_channel):
    from sentinel_pipeline.models import ChannelTranscript

    near = ChannelTranscript(
        channel=Channel.NEAR,
        text="We will file a police case against you.",
        words=[],
    )
    assert "false_legal_threat" in rule_ids(engine.evaluate(call(near=near)))


def test_the_shipped_defaults_cover_all_ten_rules(default_rules):
    assert len(default_rules.rules) == 10
    assert {r.rule_id for r in default_rules.rules} == {
        "abusive_language", "threat_of_violence", "false_legal_threat",
        "false_seizure_threat", "third_party_disclosure", "outside_call_hours",
        "missing_identification", "no_purpose_disclosure", "excessive_interruption",
        "repeat_contact",
    }
    assert all(r.enabled for r in default_rules.rules)
