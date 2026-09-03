"""Which ASR provider the pipeline builds, and how a floor's languages route to it.

Until this module existed nothing in the pipeline chose a provider: ``Finalizer``
takes an injected ``BatchASR`` and every adapter was constructible but unreachable.
This is the one place that decides, so a provider swap is a configuration change
rather than a code change — the same property ``providers/openai.py`` exists for on
the analysis slot.

**The default batch provider is** ``google-transcribe``, which is this registry's name
for the ``gemini-3.5-transcribe`` model. It is the only candidate
that returns per-word timings *and* handles mid-sentence code-switching, and both are
load-bearing: every compliance finding carries a span built from
``Word.start_ms``/``end_ms``, and the input is code-mixed Hinglish rather than clean
single-language speech. ``docs/asr-provider-selection.md`` records the full comparison.

Two things this module refuses to do quietly.

**It will not build a transcriber that cannot read the floor's language.** The default
model has no Tamil at all. A Tamil floor configured against it would not fail — it
would transcribe Tamil audio as something else and hand a bank a clean-looking
transcript with no flags on it. So a configured language the chosen provider does not
support is a startup error naming the language, unless an explicit route sends that
language somewhere that does support it.

**It will not silently fall back.** A provider that cannot be constructed — a missing
key, an absent SDK — raises. Degrading to a different transcriber without saying so
would make two calls on the same floor incomparable while both look authoritative,
and every stored artifact records the provider that produced it precisely so that
cannot happen.
"""

from __future__ import annotations

import os
from collections.abc import Mapping
from dataclasses import dataclass, field

from ..asr.base import ASRResult, BatchASR

#: The batch ASR provider used when nothing selects one.
DEFAULT_BATCH_ASR = "google-transcribe"

#: Languages this product ships into, as BCP-47. A floor is configured with some
#: subset of these; anything outside it is a typo rather than a new market.
SHIPPED_LANGUAGES = ("hi-IN", "mr-IN", "ta-IN", "te-IN", "en-IN")


class ProviderConfigError(RuntimeError):
    """A provider selection that cannot be honoured as written."""


# --------------------------------------------------------------------- capabilities


@dataclass(frozen=True)
class Capabilities:
    """What a provider can do, as far as selection is concerned.

    Declared here rather than asked of the adapter because selection has to happen
    before construction: refusing a Tamil floor is only useful if it happens before
    an API key is required, not after the first call comes back wrong.
    """

    #: ``None`` means "every language", for a provider whose coverage is not the
    #: constraint worth encoding (a locally run model, or a fake).
    languages: frozenset[str] | None
    #: Per-word timings, not per phrase. Without these a finding's evidence span is
    #: an interpolation that looks precise and is not.
    word_timestamps: bool
    #: Mid-sentence language switching, which is the normal case on these calls.
    code_switching: bool

    def supports(self, language: str) -> bool:
        return self.languages is None or language in self.languages


# Kept as data next to the registry so adding an adapter means stating what it does,
# in the same commit, where the selection logic can see it.
CAPABILITIES: dict[str, Capabilities] = {
    "google-transcribe": Capabilities(
        # gemini-3.5-transcribe's India-relevant locales. Tamil is absent from the
        # model's list entirely; see providers/google.py.
        languages=frozenset({"as-IN", "bn-IN", "en-IN", "gu-IN", "hi-IN", "kn-IN",
                             "ml-IN", "mr-IN", "or-IN", "pa-IN", "te-IN"}),
        word_timestamps=True,
        code_switching=True,
    ),
    "sarvam": Capabilities(
        # 22 Indian languages including Tamil, which is why it is the Tamil route.
        languages=frozenset(SHIPPED_LANGUAGES) | frozenset({"bn-IN", "gu-IN", "kn-IN",
                                                            "ml-IN", "or-IN", "pa-IN"}),
        # Sentence- or phrase-level only. Recorded honestly so a deployment that
        # routes to Sarvam knows its evidence spans are coarser.
        word_timestamps=False,
        code_switching=True,
    ),
    "whisper": Capabilities(
        languages=None,
        word_timestamps=True,
        # Whisper handles one language per pass; code-mixed speech is the case it is
        # weakest on, which is what the Phase 3 measurement is for.
        code_switching=False,
    ),
    "fake-asr": Capabilities(languages=None, word_timestamps=True, code_switching=True),
}


# ------------------------------------------------------------------------- settings


@dataclass(frozen=True)
class ASRSettings:
    """Everything needed to build the batch ASR slot.

    ``languages`` is the floor's configured set. It is not passed through to the
    provider as a hint unless there is exactly one — with several, detection plus
    code-switching is the point, and pinning one locale would defeat it.
    """

    provider: str = DEFAULT_BATCH_ASR
    languages: tuple[str, ...] = ()
    #: Per-language overrides, e.g. ``{"ta-IN": "sarvam"}``. A language listed here
    #: is transcribed by the named provider rather than by ``provider``.
    routes: Mapping[str, str] = field(default_factory=dict)
    api_keys: Mapping[str, str] = field(default_factory=dict)
    #: Constructor keyword arguments per provider name, e.g.
    #: ``{"google-transcribe": {"max_chunk_s": 600}}``. Keyed by provider rather than
    #: flat because a routed floor builds two adapters, and a knob only one of them
    #: accepts would otherwise reach the other as an unexpected keyword argument.
    options: Mapping[str, Mapping[str, object]] = field(default_factory=dict)

    def options_for(self, provider: str) -> dict[str, object]:
        return dict(self.options.get(provider, {}))

    def provider_for(self, language: str) -> str:
        return self.routes.get(language, self.provider)

    def providers_in_use(self) -> tuple[str, ...]:
        # dict.fromkeys rather than a membership test against a list being extended:
        # the latter is correct only because CPython evaluates the generator lazily,
        # and a refactor to a comprehension would silently drop the deduplication.
        return tuple(dict.fromkeys([self.provider, *(self.routes[lang]
                                                     for lang in sorted(self.routes))]))

    def languages_for(self, provider: str) -> tuple[str, ...]:
        """The configured languages this provider is the one actually serving.

        Not simply ``languages``: a language routed elsewhere is not this provider's
        problem, and treating it as such is what made a Tamil-only floor with a
        correct Tamil route fail to build at all.
        """
        return tuple(lang for lang in self.languages if self.provider_for(lang) == provider)


def settings_from_env(env: Mapping[str, str] | None = None) -> ASRSettings:
    """Read the ASR selection out of the environment.

    ``SENTINEL_ASR_PROVIDER``   provider name; defaults to the module default.
    ``SENTINEL_ASR_LANGUAGES``  comma-separated BCP-47 codes for the floor.
    ``SENTINEL_ASR_ROUTES``     comma-separated ``lang=provider`` overrides.
    ``SENTINEL_GOOGLE_API_KEY`` / ``GEMINI_API_KEY``, ``SENTINEL_SARVAM_API_KEY``.
    """
    env = os.environ if env is None else env

    languages = _split_list(env.get("SENTINEL_ASR_LANGUAGES", ""))
    for code in languages:
        if code not in SHIPPED_LANGUAGES:
            raise ProviderConfigError(
                f"SENTINEL_ASR_LANGUAGES lists {code!r}, which is not a language this "
                f"product ships into. Expected some of: {', '.join(SHIPPED_LANGUAGES)}."
            )

    routes: dict[str, str] = {}
    for entry in _split_list(env.get("SENTINEL_ASR_ROUTES", "")):
        language, sep, name = entry.partition("=")
        language, name = language.strip(), name.strip()
        if not sep or not language or not name:
            raise ProviderConfigError(
                f"SENTINEL_ASR_ROUTES entry {entry!r} is not in lang=provider form"
            )
        # Held to the same standard as SENTINEL_ASR_LANGUAGES. A typo here is worse
        # than one there: a misspelled route silently never fires, so the floor keeps
        # running on the default provider for a language it was routed away from.
        if language not in SHIPPED_LANGUAGES:
            raise ProviderConfigError(
                f"SENTINEL_ASR_ROUTES routes {language!r}, which is not a language this "
                f"product ships into. Expected some of: {', '.join(SHIPPED_LANGUAGES)}."
            )
        routes[language] = name

    api_keys = {}
    google_key = env.get("SENTINEL_GOOGLE_API_KEY") or env.get("GEMINI_API_KEY")
    if google_key:
        api_keys["google-transcribe"] = google_key
    if sarvam_key := env.get("SENTINEL_SARVAM_API_KEY"):
        api_keys["sarvam"] = sarvam_key

    return ASRSettings(
        provider=env.get("SENTINEL_ASR_PROVIDER") or DEFAULT_BATCH_ASR,
        languages=languages,
        routes=routes,
        api_keys=api_keys,
    )


def _split_list(raw: str) -> tuple[str, ...]:
    return tuple(part.strip() for part in raw.split(",") if part.strip())


# ---------------------------------------------------------------------- validation


def validate(settings: ASRSettings) -> None:
    """Reject a selection that cannot do the job, before anything is constructed.

    Raises :class:`ProviderConfigError` with the language named, because "ASR is
    misconfigured" sends someone reading code and "no provider covers ta-IN" sends
    them to the tenant's language list.
    """
    for name in settings.providers_in_use():
        if name not in CAPABILITIES:
            raise ProviderConfigError(
                f"unknown ASR provider {name!r}; known providers are "
                f"{', '.join(sorted(CAPABILITIES))}"
            )

    for language in settings.languages:
        name = settings.provider_for(language)
        if not CAPABILITIES[name].supports(language):
            covering = sorted(n for n, c in CAPABILITIES.items()
                              if c.supports(language) and c.languages is not None)
            hint = (f" Route it explicitly with SENTINEL_ASR_ROUTES={language}=<provider>; "
                    f"{' or '.join(covering)} supports it." if covering else "")
            raise ProviderConfigError(
                f"ASR provider {name!r} does not support {language}, which this floor "
                f"is configured for.{hint}"
            )


def warnings_for(settings: ASRSettings) -> list[str]:
    """Selection problems that degrade the product without breaking it.

    Separate from :func:`validate` because these are judgement calls a deployment is
    allowed to make — coarse evidence spans are worse than precise ones, not unusable
    — and a startup that refuses to boot over one would be wrong.
    """
    notes = []
    for name in settings.providers_in_use():
        caps = CAPABILITIES.get(name)
        if caps is None:
            continue
        if not caps.word_timestamps:
            notes.append(
                f"{name} returns phrase-level rather than per-word timings, so evidence "
                f"spans on its calls are approximate"
            )
        if not caps.code_switching and len(settings.languages) != 1:
            notes.append(
                f"{name} does not handle code-switching, and this floor is configured "
                f"for {len(settings.languages) or 'no'} languages"
            )
    return notes


# ------------------------------------------------------------------------- building


def build_batch_asr(settings: ASRSettings | None = None, *,
                    clients: Mapping[str, object] | None = None) -> BatchASR:
    """Construct the batch ASR slot.

    Returns a single adapter when one provider covers the floor, and a
    :class:`LanguageRoutedASR` when a route sends some language elsewhere. Callers
    get a ``BatchASR`` either way and never need to know which.

    ``clients`` injects a pre-built SDK client per provider name, which is how tests
    exercise this without an SDK or a key.
    """
    settings = settings or ASRSettings()
    validate(settings)
    clients = clients or {}

    default = _construct(settings.provider, settings, clients)
    if not settings.routes:
        return default

    routed = {
        language: (default if name == settings.provider
                   else _construct(name, settings, clients))
        for language, name in settings.routes.items()
    }
    return LanguageRoutedASR(default=default, routes=routed)


def _construct(name: str, settings: ASRSettings, clients: Mapping[str, object]) -> BatchASR:
    options = settings.options_for(name)
    key = settings.api_keys.get(name)
    client = clients.get(name)

    # The languages *this* provider serves, not the floor's whole set: a language
    # routed elsewhere is not this adapter's problem, and pinning one on the default
    # adapter is what stopped a Tamil-only floor with a correct Tamil route from
    # building at all.
    #
    # Pinned only when that comes to exactly one language. With several, the model's
    # own detection plus code-switching is the behaviour we want, and naming one
    # locale would suppress it mid-sentence.
    served = settings.languages_for(name)
    only_language = served[0] if len(served) == 1 else None

    if name == "google-transcribe":
        from .google import GoogleTranscribeASR  # noqa: PLC0415 - lazy, see providers/__init__

        if client is None and not key:
            raise ProviderConfigError(
                "google-transcribe needs SENTINEL_GOOGLE_API_KEY (or GEMINI_API_KEY)"
            )
        if only_language:
            options.setdefault("language_hints", (only_language,))
        return GoogleTranscribeASR(api_key=key, client=client, **options)

    if name == "sarvam":
        from .sarvam import SarvamASR  # noqa: PLC0415

        if client is None and not key:
            raise ProviderConfigError("sarvam needs SENTINEL_SARVAM_API_KEY")
        return SarvamASR(api_key=key or "", session=client, **options)

    if name == "whisper":
        from .whisper import WhisperASR  # noqa: PLC0415

        return WhisperASR(**options)

    if name == "fake-asr":
        from .fake import FakeASR  # noqa: PLC0415

        return FakeASR(**options)

    raise ProviderConfigError(f"unknown ASR provider {name!r}")


@dataclass
class LanguageRoutedASR:
    """Sends each call to the provider that can read its language.

    Exists because the default model has no Tamil. Routing is by the caller's
    ``language_hint`` only: there is no sniffing step, because guessing the language
    in order to pick the model that detects the language is circular, and a floor's
    language is tenant configuration that is already known.

    The result records the provider that actually ran, not this wrapper, so a
    transcript from the Tamil route stays distinguishable from the rest.
    """

    default: BatchASR
    routes: Mapping[str, BatchASR]

    name: str = "language-routed"

    def __post_init__(self) -> None:
        parts = [f"default={_name_of(self.default)}"]
        parts.extend(f"{lang}={_name_of(asr)}" for lang, asr in sorted(self.routes.items()))
        self.version = ",".join(parts)

    def transcribe(self, audio: bytes, *, sample_rate: int,
                   language_hint: str | None = None) -> ASRResult:
        chosen = self.routes.get(language_hint, self.default) if language_hint else self.default
        return chosen.transcribe(audio, sample_rate=sample_rate, language_hint=language_hint)


def _name_of(asr: object) -> str:
    return str(getattr(asr, "name", "unknown"))
