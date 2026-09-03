"""Tests for the batch-ASR provider registry.

The registry is the one place that turns configuration into a transcriber, so these
tests are mostly about its two refusals: it will not build a transcriber that cannot
read the floor's language, and it will not quietly substitute a different one. Both
failures would produce a clean-looking transcript with no flags on it, which is the
worst outcome this product has — a bank would be handed evidence that a call was fine
when nobody ever read it.

Everything runs against hand-written fakes and explicit env dicts: no SDK, no network,
no process environment. ``google-genai``, ``requests`` and ``faster-whisper`` are all
absent from the test environment on purpose.
"""

from __future__ import annotations

import inspect
import re
from dataclasses import dataclass, field
from importlib.util import find_spec

import pytest

from sentinel_pipeline import providers
from sentinel_pipeline.asr.base import ASRResult
from sentinel_pipeline.providers import registry
from sentinel_pipeline.providers.google import GoogleTranscribeASR
from sentinel_pipeline.providers.registry import (
    CAPABILITIES,
    DEFAULT_BATCH_ASR,
    SHIPPED_LANGUAGES,
    ASRSettings,
    Capabilities,
    LanguageRoutedASR,
    ProviderConfigError,
    build_batch_asr,
    settings_from_env,
    validate,
    warnings_for,
)


@dataclass
class RecordingASR:
    """A minimal ``BatchASR`` that records every call it was handed.

    Hand-written rather than mocked so the routing tests fail on what the wrapper
    actually forwards, not on a call-signature assertion.
    """

    name: str = "recording"
    version: str = "1"
    calls: list[tuple[bytes, int, str | None]] = field(default_factory=list)

    def transcribe(self, audio: bytes, *, sample_rate: int,
                   language_hint: str | None = None) -> ASRResult:
        self.calls.append((audio, sample_rate, language_hint))
        return ASRResult(
            text=f"heard by {self.name}",
            language=language_hint or "und",
            provider=self.name,
            provider_version=self.version,
        )


class FakeGoogleClient:
    """Stands in for ``genai.Client`` so the google adapter constructs without an SDK."""


def google_clients() -> dict[str, object]:
    return {"google-transcribe": FakeGoogleClient()}


# --------------------------------------------------------------------------- defaults


def test_the_default_batch_provider_is_google_transcribe():
    # Pinned as a product decision, not a detail: this string is what every floor
    # that selects nothing ends up running, and only this candidate returns per-word
    # timings *and* handles code-switching. A silent change here would change the
    # evidence spans under every compliance finding in the product.
    assert DEFAULT_BATCH_ASR == "google-transcribe"


def test_settings_with_no_arguments_select_the_default_and_configure_nothing_else():
    settings = ASRSettings()

    assert settings.provider == DEFAULT_BATCH_ASR
    assert settings.languages == ()
    assert dict(settings.routes) == {}
    assert dict(settings.api_keys) == {}
    assert dict(settings.options) == {}


def test_tamil_is_one_of_the_languages_this_product_ships_into():
    # The Tamil gap in the default provider is a routing problem, not a "we do not do
    # Tamil" problem. If ta-IN ever left this tuple the routing tests below would
    # start passing vacuously.
    assert "ta-IN" in SHIPPED_LANGUAGES


# ---------------------------------------------------------------- Capabilities.supports


@pytest.mark.parametrize("language", ["hi-IN", "ta-IN", "en-GB", "xx-XX", "", "nonsense"])
def test_a_provider_with_no_declared_languages_supports_anything(language):
    caps = Capabilities(languages=None, word_timestamps=True, code_switching=True)

    assert caps.supports(language) is True


@pytest.mark.parametrize(
    ("language", "supported"),
    [("hi-IN", True), ("en-IN", True), ("ta-IN", False), ("xx-XX", False), ("", False)],
)
def test_a_declared_language_set_supports_only_its_members(language, supported):
    caps = Capabilities(languages=frozenset({"hi-IN", "en-IN"}), word_timestamps=True,
                        code_switching=True)

    assert caps.supports(language) is supported


@pytest.mark.parametrize("language", ["HI-IN", "hi-in", "Hi-IN"])
def test_language_support_is_case_sensitive(language):
    # BCP-47 codes reach this table from tenant configuration verbatim. Matching
    # loosely here would let "HI-IN" pass selection and then be rejected by the
    # provider on the first real call, which is exactly the late failure the whole
    # module exists to move to startup.
    assert CAPABILITIES["google-transcribe"].supports(language) is False


# ------------------------------------------------------------- CAPABILITIES invariants


_CONSTRUCT_NAMES = frozenset(
    re.findall(r'name == "([^"]+)"', inspect.getsource(registry._construct))
)


def test_the_capability_table_and_the_constructor_know_the_same_providers():
    # This is the test that fails when someone adds an adapter and forgets to declare
    # what it does. An undeclared provider skips the language check entirely, so a
    # Tamil floor pointed at it would be accepted and transcribed as something else.
    assert _CONSTRUCT_NAMES, "the provider names could not be read out of _construct"
    assert _CONSTRUCT_NAMES == set(CAPABILITIES)


def test_google_transcribe_does_not_support_tamil():
    # Load-bearing absence, not an oversight: ta-IN is not in the model's locale list
    # at all. Adding it here to make a Tamil floor boot would produce fluent Hindi
    # for Tamil audio and no flags on it.
    assert CAPABILITIES["google-transcribe"].supports("ta-IN") is False
    assert CAPABILITIES["google-transcribe"].word_timestamps is True
    assert CAPABILITIES["google-transcribe"].code_switching is True


def test_sarvam_covers_tamil_but_only_at_phrase_level():
    caps = CAPABILITIES["sarvam"]

    assert caps.supports("ta-IN") is True
    # Declared honestly: a deployment routing Tamil here gets coarser evidence spans
    # than the rest of the floor, and a reviewer has to be able to know that.
    assert caps.word_timestamps is False


def test_whisper_declares_no_language_constraint_and_no_code_switching():
    assert CAPABILITIES["whisper"].languages is None
    assert CAPABILITIES["whisper"].code_switching is False


@pytest.mark.parametrize("language", SHIPPED_LANGUAGES)
def test_every_shipped_language_has_a_provider_that_declares_it(language):
    covering = [name for name, caps in CAPABILITIES.items()
                if caps.languages is not None and caps.supports(language)]

    assert covering, f"no provider declares {language}, so no floor can be configured for it"


# ------------------------------------------------------------------- settings_from_env


def test_an_empty_environment_yields_the_module_defaults():
    settings = settings_from_env({})

    assert settings.provider == DEFAULT_BATCH_ASR
    assert settings.languages == ()
    assert dict(settings.routes) == {}
    assert dict(settings.api_keys) == {}


def test_the_provider_can_be_chosen_by_environment():
    assert settings_from_env({"SENTINEL_ASR_PROVIDER": "sarvam"}).provider == "sarvam"


def test_an_empty_provider_variable_falls_back_to_the_default():
    # An unset-looking variable ("SENTINEL_ASR_PROVIDER=") must not select the empty
    # provider and fail construction with a name nobody typed.
    assert settings_from_env({"SENTINEL_ASR_PROVIDER": ""}).provider == DEFAULT_BATCH_ASR


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("hi-IN", ("hi-IN",)),
        ("hi-IN,en-IN", ("hi-IN", "en-IN")),
        ("  hi-IN , en-IN  ", ("hi-IN", "en-IN")),
        ("hi-IN,en-IN,", ("hi-IN", "en-IN")),
        ("hi-IN,,en-IN", ("hi-IN", "en-IN")),
        ("", ()),
        ("   ", ()),
        (",", ()),
    ],
)
def test_configured_languages_are_parsed_from_a_comma_separated_list(raw, expected):
    assert settings_from_env({"SENTINEL_ASR_LANGUAGES": raw}).languages == expected


@pytest.mark.parametrize("code", ["kn-IN", "xx-XX", "hi", "ta_IN"])
def test_a_language_this_product_does_not_ship_into_is_rejected_by_name(code):
    with pytest.raises(ProviderConfigError) as exc:
        settings_from_env({"SENTINEL_ASR_LANGUAGES": f"hi-IN,{code}"})

    # Naming the offending code is the point: a floor's language list is tenant
    # configuration, and "ASR is misconfigured" sends an operator to read code.
    assert code in str(exc.value)
    assert "SENTINEL_ASR_LANGUAGES" in str(exc.value)


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("ta-IN=sarvam", {"ta-IN": "sarvam"}),
        ("ta-IN=sarvam,te-IN=whisper", {"ta-IN": "sarvam", "te-IN": "whisper"}),
        ("  ta-IN = sarvam ", {"ta-IN": "sarvam"}),
        ("ta-IN=sarvam,", {"ta-IN": "sarvam"}),
        ("", {}),
    ],
)
def test_routes_are_parsed_as_language_equals_provider(raw, expected):
    assert dict(settings_from_env({"SENTINEL_ASR_ROUTES": raw}).routes) == expected


@pytest.mark.parametrize("raw", ["ta-IN", "ta-IN=", "ta-IN= ", "sarvam", "=sarvam",
                                 " =sarvam", "="])
def test_a_route_that_is_not_language_equals_provider_is_rejected(raw):
    with pytest.raises(ProviderConfigError) as exc:
        settings_from_env({"SENTINEL_ASR_ROUTES": raw})

    assert "lang=provider" in str(exc.value)


@pytest.mark.parametrize("code", ["kn-IN", "xx-XX", "hi", "ta_IN"])
def test_a_route_for_a_language_this_product_does_not_ship_into_is_rejected(code):
    # Held to the same standard as SENTINEL_ASR_LANGUAGES, and for a sharper reason: a
    # misspelled route never fires, so the floor keeps transcribing that language on
    # the default provider it was deliberately routed away from — silently.
    with pytest.raises(ProviderConfigError) as exc:
        settings_from_env({"SENTINEL_ASR_ROUTES": f"{code}=sarvam"})

    assert code in str(exc.value)
    assert "SENTINEL_ASR_ROUTES" in str(exc.value)


# ------------------------------------------------------------- per-provider settings


def test_options_are_scoped_to_the_provider_they_belong_to():
    # Flat options would reach every adapter, so a knob only one of them accepts
    # becomes an unexpected keyword argument on a routed floor's second provider.
    settings = ASRSettings(
        languages=("hi-IN", "ta-IN"),
        routes={"ta-IN": "fake-asr"},
        options={"google-transcribe": {"max_chunk_s": 600},
                 "fake-asr": {"text": "vanakkam"}},
    )

    built = build_batch_asr(settings, clients=google_clients())

    assert built.default.max_chunk_s == 600
    assert built.routes["ta-IN"].text == "vanakkam"


def test_options_for_an_unmentioned_provider_is_empty():
    assert ASRSettings().options_for("sarvam") == {}


def test_languages_for_excludes_the_ones_routed_elsewhere():
    settings = ASRSettings(languages=("hi-IN", "mr-IN", "ta-IN"),
                           routes={"ta-IN": "sarvam"})

    assert settings.languages_for("google-transcribe") == ("hi-IN", "mr-IN")
    assert settings.languages_for("sarvam") == ("ta-IN",)
    assert settings.languages_for("whisper") == ()


def test_a_sarvam_route_can_be_built_with_an_injected_session():
    # The registry has to be able to construct every provider it routes to without the
    # provider's SDK present, or the Tamil route is untestable.
    settings = ASRSettings(languages=("hi-IN", "ta-IN"), routes={"ta-IN": "sarvam"},
                           api_keys={"sarvam": "k"})

    built = build_batch_asr(settings, clients={**google_clients(), "sarvam": object()})

    assert built.routes["ta-IN"].name == "sarvam"
    assert built.routes["ta-IN"].version == "saaras:v4"


def test_the_sentinel_google_key_wins_over_the_generic_gemini_one():
    # Both variables exist in the wild. The Sentinel-specific one is the deliberate
    # choice; the generic one is whatever else on the box happened to export it.
    settings = settings_from_env({
        "SENTINEL_GOOGLE_API_KEY": "chosen",
        "GEMINI_API_KEY": "ambient",
    })

    assert settings.api_keys["google-transcribe"] == "chosen"


@pytest.mark.parametrize(
    ("env", "expected"),
    [
        ({"SENTINEL_GOOGLE_API_KEY": "k1"}, "k1"),
        ({"GEMINI_API_KEY": "k2"}, "k2"),
        ({"SENTINEL_GOOGLE_API_KEY": "", "GEMINI_API_KEY": "k3"}, "k3"),
    ],
)
def test_either_google_key_variable_is_accepted(env, expected):
    assert settings_from_env(env).api_keys["google-transcribe"] == expected


def test_no_google_key_variable_leaves_no_google_key():
    # Absent rather than empty-string: construction refuses on "no key present", and
    # an empty string would look like a key and fail at the API instead.
    assert "google-transcribe" not in settings_from_env({}).api_keys
    assert "google-transcribe" not in settings_from_env({"SENTINEL_GOOGLE_API_KEY": ""}).api_keys


def test_the_sarvam_key_is_read_from_its_own_variable():
    settings = settings_from_env({"SENTINEL_SARVAM_API_KEY": "sk"})

    assert settings.api_keys == {"sarvam": "sk"}


def test_a_full_environment_is_read_into_one_settings_object():
    settings = settings_from_env({
        "SENTINEL_ASR_PROVIDER": "google-transcribe",
        "SENTINEL_ASR_LANGUAGES": "hi-IN, ta-IN",
        "SENTINEL_ASR_ROUTES": "ta-IN=sarvam",
        "GEMINI_API_KEY": "gk",
        "SENTINEL_SARVAM_API_KEY": "sk",
    })

    assert settings.provider == "google-transcribe"
    assert settings.languages == ("hi-IN", "ta-IN")
    assert dict(settings.routes) == {"ta-IN": "sarvam"}
    assert settings.api_keys == {"google-transcribe": "gk", "sarvam": "sk"}
    # The environment carries no adapter knobs; those are passed in code.
    assert dict(settings.options) == {}


# --------------------------------------------------------- provider_for / in-use names


def test_provider_for_prefers_a_route_and_otherwise_returns_the_default():
    settings = ASRSettings(provider="google-transcribe", routes={"ta-IN": "sarvam"})

    assert settings.provider_for("ta-IN") == "sarvam"
    assert settings.provider_for("hi-IN") == "google-transcribe"
    assert settings.provider_for("unconfigured") == "google-transcribe"


def test_providers_in_use_lists_the_default_first_then_route_targets():
    settings = ASRSettings(provider="google-transcribe",
                           routes={"te-IN": "whisper", "ta-IN": "sarvam"})

    # Default first, then routes in language order, so the string is stable enough to
    # log and compare across restarts.
    assert settings.providers_in_use() == ("google-transcribe", "sarvam", "whisper")


def test_providers_in_use_deduplicates_a_provider_named_by_several_routes():
    settings = ASRSettings(routes={"ta-IN": "sarvam", "te-IN": "sarvam"})

    assert settings.providers_in_use() == ("google-transcribe", "sarvam")


def test_a_route_pointing_back_at_the_default_does_not_duplicate_it():
    settings = ASRSettings(provider="google-transcribe",
                           routes={"hi-IN": "google-transcribe"})

    assert settings.providers_in_use() == ("google-transcribe",)


# ------------------------------------------------------------------------- validate


def test_an_unknown_provider_is_refused_and_the_known_names_are_listed():
    with pytest.raises(ProviderConfigError) as exc:
        validate(ASRSettings(provider="deepgram"))

    message = str(exc.value)
    assert "deepgram" in message
    for known in CAPABILITIES:
        assert known in message


def test_an_unknown_provider_named_only_in_a_route_is_refused():
    with pytest.raises(ProviderConfigError) as exc:
        validate(ASRSettings(routes={"ta-IN": "typo-asr"}))

    assert "typo-asr" in str(exc.value)


def test_a_language_the_chosen_provider_cannot_read_is_named_in_the_error():
    with pytest.raises(ProviderConfigError) as exc:
        validate(ASRSettings(languages=("hi-IN", "ta-IN")))

    message = str(exc.value)
    # The language has to appear verbatim: that sends the operator to the tenant's
    # language list, where the fix is, rather than into the selection code.
    assert "ta-IN" in message
    assert "google-transcribe" in message
    assert "SENTINEL_ASR_ROUTES" in message
    # And it names a provider that can actually read it, so the suggestion is usable.
    assert "sarvam" in message


def test_the_error_omits_a_route_suggestion_when_nothing_declares_the_language():
    # Only providers with a declared language set are suggested; "whisper supports
    # everything" is not evidence that whisper can read Japanese well, and pointing an
    # operator at a route that would not help is worse than no suggestion.
    with pytest.raises(ProviderConfigError) as exc:
        validate(ASRSettings(provider="sarvam", languages=("ja-JP",)))

    assert "ja-JP" in str(exc.value)
    assert "SENTINEL_ASR_ROUTES" not in str(exc.value)


def test_a_tamil_floor_on_the_default_provider_is_refused():
    # The case the module exists for. Left unchecked this floor would transcribe
    # Tamil audio as something else and hand a bank a clean transcript.
    with pytest.raises(ProviderConfigError):
        validate(ASRSettings(languages=("hi-IN", "ta-IN")))


def test_routing_tamil_to_sarvam_makes_the_same_floor_valid():
    validate(ASRSettings(languages=("hi-IN", "ta-IN"), routes={"ta-IN": "sarvam"}))


def test_a_floor_with_no_configured_languages_validates():
    validate(ASRSettings())


@pytest.mark.parametrize("language", SHIPPED_LANGUAGES)
def test_a_provider_with_no_language_constraint_accepts_any_configured_language(language):
    validate(ASRSettings(provider="whisper", languages=(language,)))


def test_a_route_only_covers_the_language_it_names():
    # Routing Tamil away does not make the default provider Tamil-capable for any
    # other language that happens to be configured.
    with pytest.raises(ProviderConfigError) as exc:
        validate(ASRSettings(provider="sarvam", languages=("ta-IN", "as-IN"),
                             routes={"ta-IN": "sarvam"}))

    assert "as-IN" in str(exc.value)


# ----------------------------------------------------------------------- warnings_for


def test_sarvam_in_use_warns_that_its_timings_are_phrase_level():
    notes = warnings_for(ASRSettings(languages=("hi-IN", "ta-IN"),
                                     routes={"ta-IN": "sarvam"}))

    assert any("phrase-level" in note and "sarvam" in note for note in notes)


@pytest.mark.parametrize("languages", [(), ("hi-IN", "en-IN"), ("hi-IN", "en-IN", "mr-IN")])
def test_whisper_warns_about_code_switching_unless_the_floor_runs_one_language(languages):
    notes = warnings_for(ASRSettings(provider="whisper", languages=languages))

    assert any("code-switching" in note for note in notes)


def test_whisper_on_a_single_language_floor_raises_no_code_switching_warning():
    notes = warnings_for(ASRSettings(provider="whisper", languages=("hi-IN",)))

    assert not any("code-switching" in note for note in notes)


def test_a_default_google_floor_produces_no_warnings_at_all():
    assert warnings_for(ASRSettings(languages=("hi-IN", "en-IN"))) == []


def test_warnings_are_returned_rather_than_raised():
    # A deployment is allowed to accept coarser evidence spans; a startup that
    # refused to boot over one would be wrong. So the same settings that produce a
    # warning must still validate.
    settings = ASRSettings(languages=("hi-IN", "ta-IN"), routes={"ta-IN": "sarvam"})

    validate(settings)
    assert warnings_for(settings)


def test_an_undeclared_provider_produces_no_warnings_instead_of_an_error():
    # validate() is what rejects an unknown name; warnings_for() is called on the
    # same settings and must not raise a second, different failure over it.
    assert warnings_for(ASRSettings(provider="deepgram")) == []


# ----------------------------------------------------------------------- construction


def test_the_google_default_is_returned_as_the_adapter_itself():
    built = build_batch_asr(ASRSettings(), clients=google_clients())

    assert isinstance(built, GoogleTranscribeASR)
    assert not isinstance(built, LanguageRoutedASR)


def test_the_built_adapter_reports_its_own_name_and_version():
    # Every stored artifact records these, so before/after a model change stay
    # distinguishable. A wrapper's name here would erase that.
    built = build_batch_asr(ASRSettings(), clients=google_clients())

    assert built.name == "google-transcribe"
    assert built.version == "gemini-3.5-transcribe"


def test_a_floor_with_no_routes_is_not_wrapped_in_a_router():
    built = build_batch_asr(ASRSettings(languages=("hi-IN", "en-IN")),
                            clients=google_clients())

    assert not isinstance(built, LanguageRoutedASR)


def test_a_routed_floor_is_wrapped_and_the_wrapper_records_the_mapping():
    # Routed to fake-asr rather than sarvam so this exercises the wrapper without
    # needing the Sarvam SDK; the validation tests above cover the real Tamil route.
    settings = ASRSettings(languages=("hi-IN", "ta-IN"), routes={"ta-IN": "fake-asr"})

    built = build_batch_asr(settings, clients=google_clients())

    assert isinstance(built, LanguageRoutedASR)
    assert built.version == "default=google-transcribe,ta-IN=fake-asr"


def test_a_route_back_to_the_default_reuses_the_one_adapter_instance():
    settings = ASRSettings(languages=("hi-IN",), routes={"hi-IN": "google-transcribe"})

    built = build_batch_asr(settings, clients=google_clients())

    assert isinstance(built, LanguageRoutedASR)
    # Identity, not equality: a second adapter for the same provider would open a
    # second client and double the connection pool for nothing.
    assert built.routes["hi-IN"] is built.default


def test_a_missing_google_key_names_the_environment_variable():
    with pytest.raises(ProviderConfigError) as exc:
        build_batch_asr(ASRSettings())

    assert "SENTINEL_GOOGLE_API_KEY" in str(exc.value)
    assert "GEMINI_API_KEY" in str(exc.value)


def test_an_injected_client_stands_in_for_a_key():
    built = build_batch_asr(ASRSettings(), clients=google_clients())

    assert built.api_key is None


def test_a_missing_sarvam_key_is_refused_rather_than_falling_back_to_the_default():
    # Degrading to another transcriber without saying so would make two calls on the
    # same floor incomparable while both look authoritative.
    with pytest.raises(ProviderConfigError) as exc:
        build_batch_asr(ASRSettings(provider="sarvam"))

    assert "SENTINEL_SARVAM_API_KEY" in str(exc.value)


def test_adapter_options_are_forwarded_to_the_constructor():
    settings = ASRSettings(options={"google-transcribe": {
        "max_chunk_s": 900, "model": "gemini-3.5-transcribe-exp"}})

    built = build_batch_asr(settings, clients=google_clients())

    assert built.max_chunk_s == 900
    assert built.model == "gemini-3.5-transcribe-exp"
    assert built.version == "gemini-3.5-transcribe-exp"


def test_one_configured_language_is_pinned_as_the_adapter_hint():
    built = build_batch_asr(ASRSettings(languages=("hi-IN",)), clients=google_clients())

    assert built.language_hints == ("hi-IN",)


def test_two_or_more_configured_languages_leave_the_hints_empty():
    built = build_batch_asr(ASRSettings(languages=("hi-IN", "en-IN")),
                            clients=google_clients())

    # Deliberately empty. Pinning one locale would suppress the mid-sentence
    # code-switching this model was chosen for, and code-mixed Hinglish is the
    # normal input on these calls rather than an edge case.
    assert built.language_hints == ()


def test_an_explicit_hint_option_is_not_overwritten_by_the_single_language_default():
    settings = ASRSettings(languages=("hi-IN",),
                           options={"google-transcribe": {"language_hints": ("en-IN",)}})

    built = build_batch_asr(settings, clients=google_clients())

    assert built.language_hints == ("en-IN",)


def test_validation_happens_before_construction():
    # No key and no injected client anywhere: the Tamil floor has to be refused for
    # the language, which is the actionable reason, rather than for a missing key.
    with pytest.raises(ProviderConfigError) as exc:
        build_batch_asr(ASRSettings(languages=("hi-IN", "ta-IN")))

    assert "ta-IN" in str(exc.value)


def test_an_unknown_provider_never_reaches_construction():
    with pytest.raises(ProviderConfigError) as exc:
        build_batch_asr(ASRSettings(provider="deepgram"), clients=google_clients())

    assert "known providers are" in str(exc.value)


def test_the_fake_provider_builds_without_any_key():
    built = build_batch_asr(ASRSettings(provider="fake-asr", languages=("ta-IN",)))

    assert built.name == "fake-asr"


def test_a_provider_name_the_constructor_does_not_handle_raises_rather_than_returning_none():
    # Normally unreachable because validate() rejects it first; asserted directly so
    # the fallthrough cannot be dropped as dead code, leaving a None transcriber.
    with pytest.raises(ProviderConfigError):
        registry._construct("deepgram", ASRSettings(), {})


@pytest.mark.skipif(find_spec("faster_whisper") is not None,
                    reason="faster-whisper is installed, so its construction would load a model")
def test_an_absent_provider_sdk_surfaces_instead_of_being_swallowed():
    # The module refuses to fall back silently, and an absent SDK is one of the two
    # ways a provider cannot be constructed. The ImportError has to reach the caller.
    with pytest.raises(ModuleNotFoundError) as exc:
        build_batch_asr(ASRSettings(provider="whisper"))

    assert exc.value.name == "faster_whisper"


def test_no_settings_means_the_module_defaults_are_built():
    # build_batch_asr() with nothing at all must behave as ASRSettings() does, which
    # here means refusing for a missing google key rather than reading the process
    # environment behind the caller's back.
    with pytest.raises(ProviderConfigError) as exc:
        build_batch_asr()

    assert "google-transcribe" in str(exc.value)


def test_a_tamil_only_floor_routed_away_can_still_be_built():
    # The default provider must not be handed a language that was routed away from
    # it. This is the configuration docs/asr-provider-selection.md tells a Tamil floor
    # to use, and pinning ta-IN on the google adapter made it impossible to boot.
    settings = ASRSettings(languages=("ta-IN",), routes={"ta-IN": "fake-asr"})

    built = build_batch_asr(settings, clients=google_clients())

    assert isinstance(built, LanguageRoutedASR)
    # Nothing is pinned on the default adapter, because it serves no configured
    # language at all on this floor.
    assert built.default.language_hints == ()
    assert built.routes["ta-IN"].name == "fake-asr"


def test_a_provider_is_pinned_only_to_the_languages_it_actually_serves():
    # hi-IN stays with the default and ta-IN goes to the route, so each adapter sees
    # exactly one language and both get a hint — the two-language rule is about what
    # one provider handles, not about the floor's total.
    settings = ASRSettings(languages=("hi-IN", "ta-IN"), routes={"ta-IN": "fake-asr"})

    built = build_batch_asr(settings, clients=google_clients())

    assert built.default.language_hints == ("hi-IN",)


# --------------------------------------------------------------------- LanguageRoutedASR


def routed() -> tuple[LanguageRoutedASR, RecordingASR, RecordingASR]:
    default = RecordingASR(name="google-transcribe", version="gemini-3.5-transcribe")
    tamil = RecordingASR(name="sarvam", version="saaras:v4")
    return LanguageRoutedASR(default=default, routes={"ta-IN": tamil}), default, tamil


def test_a_call_is_routed_by_its_language_hint():
    router, default, tamil = routed()

    router.transcribe(b"audio", sample_rate=16_000, language_hint="ta-IN")

    assert len(tamil.calls) == 1
    assert default.calls == []


def test_an_unrouted_language_hint_uses_the_default_provider():
    router, default, tamil = routed()

    router.transcribe(b"audio", sample_rate=16_000, language_hint="hi-IN")

    assert len(default.calls) == 1
    assert tamil.calls == []


def test_no_language_hint_uses_the_default_provider():
    router, default, tamil = routed()

    router.transcribe(b"audio", sample_rate=16_000)

    assert len(default.calls) == 1
    assert tamil.calls == []


@pytest.mark.parametrize("hint", ["ta-IN", "hi-IN", None])
def test_the_audio_rate_and_hint_reach_the_chosen_adapter_unchanged(hint):
    router, default, tamil = routed()

    router.transcribe(b"pcm-bytes", sample_rate=8_000, language_hint=hint)

    calls = tamil.calls if hint == "ta-IN" else default.calls
    assert calls == [(b"pcm-bytes", 8_000, hint)]


def test_the_result_records_the_provider_that_actually_ran():
    router, _, _ = routed()

    result = router.transcribe(b"audio", sample_rate=16_000, language_hint="ta-IN")

    # Not "language-routed". A Tamil transcript came off a provider with coarser
    # timings than the rest of the floor's calls, and the stored artifact has to stay
    # distinguishable so a reviewer knows which spans are approximate.
    assert result.provider == "sarvam"
    assert result.provider_version == "saaras:v4"


def test_the_wrapper_still_reports_a_stable_name_of_its_own():
    router, _, _ = routed()

    assert router.name == "language-routed"


def test_the_version_string_is_deterministic_and_ordered_by_language():
    default = RecordingASR(name="google-transcribe")
    router = LanguageRoutedASR(
        default=default,
        routes={"te-IN": RecordingASR(name="whisper"),
                "ta-IN": RecordingASR(name="sarvam")},
    )

    # Sorted rather than insertion-ordered: this string is stored next to results, so
    # two restarts of the same configuration must not look like a provider change.
    assert router.version == "default=google-transcribe,ta-IN=sarvam,te-IN=whisper"


def test_an_adapter_that_reports_no_name_is_recorded_as_unknown():
    router = LanguageRoutedASR(default=object(), routes={})

    assert router.version == "default=unknown"


def test_a_router_with_no_routes_sends_everything_to_the_default():
    default = RecordingASR(name="google-transcribe")
    router = LanguageRoutedASR(default=default, routes={})

    router.transcribe(b"audio", sample_rate=16_000, language_hint="ta-IN")

    assert len(default.calls) == 1


# -------------------------------------------------------------------- package exports


def test_every_exported_name_is_importable_from_the_package():
    missing = [name for name in providers.__all__ if not hasattr(providers, name)]
    assert missing == []


def test_the_export_list_has_no_duplicates():
    assert len(providers.__all__) == len(set(providers.__all__))


@pytest.mark.parametrize(
    "name",
    ["DEFAULT_BATCH_ASR", "ASRSettings", "Capabilities", "LanguageRoutedASR",
     "ProviderConfigError", "build_batch_asr", "settings_from_env", "validate",
     "warnings_for"],
)
def test_the_registry_is_re_exported_as_the_same_object(name):
    assert name in providers.__all__
    assert getattr(providers, name) is getattr(registry, name)
