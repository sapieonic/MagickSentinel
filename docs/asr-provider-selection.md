# ASR provider selection

ASR quality is the top technical risk on this project, and ASR is also the single
largest recurring cost in the pipeline — larger than analysis and judging combined.
This document records what the candidate providers actually offer as of **September
2026**, what that costs at floor scale, and which ones survive the product's hard
constraints.

## Decision

**`gemini-3.5-transcribe` is the default batch ASR provider**, selected in
`sentinel_pipeline/providers/registry.py`. Tamil floors route to Sarvam, because the
default model has no Tamil at all.

Two things about that decision are worth stating plainly rather than leaving to be
discovered.

**It is ahead of the measurement.** `sentinel_pipeline/asr/__init__.py` states the exit
criterion and it has not changed: **200 hand-labelled real calls per language**, scored
on WER *and* on the numeric-entity error rate that `sentinel_pipeline/asr/evaluate.py`
tracks separately. That measurement does not exist yet. This default was chosen on
capability — per-word timings and code-switching are the two features the product
cannot work without — not on measured accuracy on our own calls. It is the best
available default, not a validated one, and the measurement can still overturn it.
Sarvam's own benchmark claims `saaras:v4` beats Gemini on Indian languages; if that
holds up here, the answer changes.

**It takes a position on OPEN-4.** The Gemini API endpoint
(`generativelanguage.googleapis.com`) is global, not India-resident. Making it the
default means borrower audio leaves India by default. That is the commitment OPEN-4
exists to get in writing from the bank, and it is now assumed rather than confirmed.
Two exits if the bank says no: route everything to Sarvam (India-hosted, and the
registry already supports it as a whole-floor provider) or to self-hosted IndicWhisper
on Indian infrastructure. Both cost a capability — see the table below — and neither is
a code change, which is the point of the registry.

## The three constraints that eliminate candidates before accuracy

**1. Per-word timestamps are mandatory.** Every finding carries a span, and
`ChannelTranscript.span_text` builds it from `Word.start_ms`/`end_ms`. The rule engine
matches n-grams over the word list and takes the span from the first and last word it
matched (`compliance/engine.py`). A provider that returns only phrase- or
sentence-level timings turns every evidence quote into an approximation, and "a flag a
reviewer cannot trace to specific words is not usable as evidence with a bank" is the
premise the whole compliance tier rests on.

**2. Code-mixed Hinglish is the primary input, not an edge case.** A model that has to
be told "this call is Hindi" and then meets three English sentences in the middle of it
will drop them or transliterate them badly. Mid-sentence code-switching support is a
requirement.

**3. Data residency, pending OPEN-4.** The working assumption is India-only. This is
where Google Cloud Speech-to-Text falls over, and the detail is worth stating exactly
because it is not obvious from the marketing pages — see below.

Diarization is explicitly **not** a requirement and never will be. The two channels
were captured separately, so the speaker is known exactly rather than inferred
(`models.Channel`). Every provider's diarization feature is irrelevant here, and
several providers make diarization mutually exclusive with the features we do need, so
this is a real advantage rather than a neutral fact.

## What each candidate actually does

### Google, option A: `gemini-3.5-transcribe` — the default

A speech-to-text model built on Gemini's audio understanding, GA, latest update August
2026. It is the only Google model that satisfies all three constraints at once.

| | |
|---|---|
| Word-level timestamps | **Yes** — `mode: {type: verbatim, timestamp_granularities: ["word"]}`, returned as `word_info` annotations with `start_offset`/`end_offset` |
| Code-switching | **Yes** — automatic detection across 85+ locales, documented for intra- and inter-sentential mixing |
| Indic coverage | Hindi, Marathi, Telugu, Indian English, plus Bengali, Gujarati, Kannada, Malayalam, Oriya, Punjabi, Assamese |
| **Tamil** | **Not supported.** `ta-IN` is absent from the model's locale list |
| Custom vocabulary | Up to 1,000 terms — but **incompatible with word timestamps**; the API rejects a request carrying both |
| Audio limit | 1 hour per request, dropping to **30 minutes when word timestamps are on** |
| Input | Raw PCM accepted directly as `audio/l16` with `sample_rate`/`channels` alongside it, so no WAV wrapping |
| Batch API | **Not supported** — no 50% batch discount on this model |
| Price | $2.00/M audio input tokens + $12.00/M text output ⇒ **≈$0.0051/min** |

Two consequences deserve to be called out rather than discovered later.

The **timestamps-versus-vocabulary exclusion** is a real loss. Custom vocabulary is
exactly the lever you would reach for on this product — bank names, product names,
"EMI", "settlement", the recovery agency's own name — and it is the lever most likely
to improve the numeric-entity error rate. We cannot have it and evidence spans at the
same time. Constraint 1 wins, so `GoogleTranscribeASR` refuses the combination at
construction rather than per call.

The **Tamil gap** means Google alone cannot cover the four languages in the README. A
Tamil floor needs a second provider for that language, which is an argument for the
per-tenant provider selection the `BatchASR` protocol already allows, not against
Google.

For the live widget path there is `gemini-3.5-transcribe-live` (WebSockets, 10-minute
sessions, ≈$0.009/min) — but note it does **not** support word timestamps at all. That
is acceptable there and only there: streaming output "is never persisted and never
reaches the portal".

### Google, option B: `chirp_2` / `chirp_3` on Cloud Speech-to-Text V2 — rejected

This is the option to reach for by reputation, and it does not work here.

`chirp_3` is the newest Chirp, has Hindi, Marathi, **Tamil** and Telugu, and lists
diarization as GA. But word-level timestamps appear in its documentation under
*"Chirp 3 doesn't support the following features"*, annotated "can be optionally
enabled, which some transcription degradation is expected", and word-level confidence
"returns a value, but it isn't truly a confidence score". Building a bank-facing
evidence trail on a feature the vendor lists as unsupported is not a defensible
position. `chirp_3` is also only in the `us` and `eu` multi-regions.

`chirp_2` does support word timestamps and word-level confidence, and covers all four
languages — but it has no code-switching support at all, and its regions are
`us-central1`, `europe-west4` and `asia-southeast1`.

And then the fact that settles it. Filtering the V2 supported-languages table by
region, **`asia-south1` (Mumbai) supports exactly one combination: `en-US` with
`telephony_short`.** Not Hindi, not Marathi, not Tamil, not Telugu, not even Indian
English. Choosing Cloud STT means every second of Indian borrower audio is transcribed
in Singapore at best — which is precisely the commitment OPEN-4 exists to get in
writing from the bank *before* the infrastructure is built.

Cloud STT pricing, for the record, is the most attractive on the list: standard
recognition tiers from $0.016/min down to $0.004/min above 2M min/month, and **Dynamic
Batch at a flat $0.003/min**. Dynamic Batch fulfils within 24 hours, which for a
product whose value proposition is same-day 100% monitoring is a separate problem on
top of the residency one.

### Sarvam AI: `saaras:v4` — the Tamil route, and the residency exit

Indian, Indic-specialised, and the incumbent adapter in this repo
(`providers/sarvam.py`).

Two defects in that adapter had to be fixed before Sarvam could be relied on as the
Tamil route, and both would have failed on the first live call rather than degrading:
it was pinned to `saarika:v2`, which is no longer in the API's model enum at all, and
it parsed `timestamps.words` as a list of word objects when the response carries three
parallel arrays (`words`, `start_time_seconds`, `end_time_seconds`) that line up by
index. It now targets `saaras:v4` and reads the documented shape.

| | |
|---|---|
| Word-level timestamps | **No.** The `timestamps` object returns `words` with `start_time_seconds`/`end_time_seconds` per *sentence or phrase*, not per word |
| Code-switching | **Yes**, natively, plus an explicit `codemix` output mode |
| Indic coverage | 22 Indian languages — **including Tamil** — plus Indian and global English |
| Accuracy | 19.31% WER on IndicVoices across 10 languages; Sarvam's own benchmark claims it beats GPT-4o Transcribe, Gemini 3 Pro, Deepgram Nova-3 and Scribe v2 on Indian languages. Vendor-reported, so this is a hypothesis for our evaluation, not an input to it |
| Residency | India-hosted, which answers OPEN-4 outright |
| Price | ₹30/hour = **₹0.50/min**, billed per second; ₹45/hour with diarization we do not need |

Sarvam is the best fit on language coverage, residency and (claimed) Indic accuracy,
and it fails constraint 1. Phrase-level timings would mean synthesising per-word
timings by interpolation, which produces spans that look precise and are not — the
worst of the available outcomes for a compliance record.

That is a question to put to Sarvam rather than a closed door: if `saaras:v4` can
return per-word offsets, it becomes the leading candidate on every axis at once. Until
it can, it is measurable for WER but not shippable as the compliance transcript.

### Self-hosted IndicWhisper / Whisper large-v3 — keep as the residency fallback

Already adapted (`providers/whisper.py`), already VAD-filtered against the
hallucination-over-silence failure that would otherwise invent an amount nobody said.
Gives true word timestamps and word probabilities, covers Tamil, runs on Indian
infrastructure with no third party in the path, and has no per-minute price at all —
only GPU cost, which at floor scale is roughly an order of magnitude below any API.

It is also the slowest to get right, the only option that makes us responsible for
throughput and uptime, and the one whose code-mixed numeric accuracy is least
predictable without measurement.

## What this costs at floor scale

The repo's own volume assumption: a 200-seat floor at 5 h talk time per agent is
≈60,000 call-minutes a day.

**The number that matters is double that.** Both channels are transcribed separately,
so billable ASR audio is **120,000 minutes/day**, or ≈3.12M minutes/month over 26
working days. Every figure below already includes that doubling; anyone reasoning from
the 60,000 figure will be out by 2×.

At ₹94/USD (3 September 2026):

| Option | Per ASR-minute | Per month, 200-seat floor | Per seat/month |
|---|---|---|---|
| Cloud STT Dynamic Batch | $0.003 | ≈$9,400 / ₹8.8 L | ₹4,400 |
| **`gemini-3.5-transcribe`** | **$0.0051** | **≈$15,900 / ₹15.0 L** | **₹7,500** |
| Sarvam `saaras:v4` | ₹0.50 | ₹15.6 L | ₹7,800 |
| Cloud STT standard, tiered | $0.016→$0.004 | ≈$25,500 / ₹24.0 L | ₹12,000 |
| `gemini-3.5-transcribe-live` | $0.009 | ≈$28,100 / ₹26.4 L | ₹13,200 |
| Self-hosted Whisper on GPU | — | order ₹1–3 L (unmeasured) | ₹500–1,500 |

Gemini Transcribe and Sarvam land within 5% of each other, so **price is not the
deciding factor between the two leading candidates** — coverage, residency and word
timestamps are.

Two cost levers are worth knowing about before anyone negotiates a per-seat price:

- **There is no ASR floor.** `CostPolicy.min_call_ms` skips *analysis* for calls under
  15 seconds; ASR runs on everything. An equivalent ASR floor is free money on a
  dialer floor full of two-second no-answers.
- **We are paying to transcribe silence.** Each channel is billed for the whole call
  duration including the other party's talk time. The client already has a VAD, so
  transcribing only voiced spans would cut ASR minutes roughly in half. It would mean
  carrying per-span offsets through the adapter to keep the word timeline intact —
  cheap to do correctly, and worth about ₹7 lakh a month per floor at these prices.

Latency, for completeness: Gemini Transcribe is a synchronous request, so a 5-minute
call comes back in seconds and the "call.end → portal" path stays same-minute. Cloud
STT Dynamic Batch is up to 24 hours. Self-hosted latency is whatever we provision for.

## How the default is configured

`providers/registry.py` is the only place a provider is chosen. Nothing selects one
per call site, so a swap is configuration:

```sh
SENTINEL_GOOGLE_API_KEY=...            # or GEMINI_API_KEY
SENTINEL_ASR_LANGUAGES=hi-IN,mr-IN     # the floor's languages
```

A Hindi/Marathi/Telugu floor needs nothing else. Two configurations the registry
refuses to start with, both deliberately:

```sh
# Rejected: the default model has no Tamil, and transcribing Tamil audio as something
# else would hand a bank a clean-looking transcript with no flags on it.
SENTINEL_ASR_LANGUAGES=hi-IN,ta-IN

# Accepted: Tamil goes where Tamil works.
SENTINEL_ASR_LANGUAGES=hi-IN,ta-IN
SENTINEL_ASR_ROUTES=ta-IN=sarvam
SENTINEL_SARVAM_API_KEY=...
```

Whole-floor alternatives, for the OPEN-4 exits above:

```sh
SENTINEL_ASR_PROVIDER=sarvam           # India-hosted; coarser evidence spans
SENTINEL_ASR_PROVIDER=whisper          # self-hosted; no third party in the path
```

A language hint is passed to the model **only when the provider serves exactly one of
the floor's languages**. With several, the model's own detection plus code-switching is
the behaviour we want, and pinning one locale would suppress it mid-sentence.

**Routing needs `CallContext.language` set per call.** `Finalizer` forwards it as the
provider's `language_hint`, and that argument is the only thing `LanguageRoutedASR`
routes on. A `ta-IN` route with `CallContext.language` left `None` sends Tamil audio to
the default provider anyway — which is the failure the startup validation exists to
prevent, so the two have to be configured together. Nothing yet threads the tenant's
language from the database into `CallContext`; that is the remaining wiring, and until
it lands a routed floor is configured but inert.

## Still outstanding

0. **Thread the tenant's language into `CallContext.language`.** Until that exists,
   `SENTINEL_ASR_ROUTES` is configured but inert, because the router has nothing to
   route on. This is the one item on this list that makes a documented configuration
   not work, so it goes first.
1. **Run the measurement.** 200 hand-labelled calls per language against Gemini
   Transcribe, Sarvam `saaras:v4` and IndicWhisper, reporting WER and numeric-entity
   error rate separately. Configure Gemini with `language_hints=()` for the Hinglish
   set so the measurement reflects the code-switching path production runs.
2. **Get OPEN-4 answered in writing.** The default now assumes the answer.
3. **Ask Sarvam for per-word offsets.** It is the one change that would make the
   strongest candidate on coverage and residency also viable as the compliance
   transcript everywhere, Tamil included.
4. **Decide whether the timestamps-versus-vocabulary trade is the right way round.**
   We currently take timestamps. If the measurement shows the numeric-entity error
   rate is what fails, custom vocabulary is the lever we gave up to get spans, and
   that is worth revisiting with the numbers in hand.

## Sources

- [Gemini 3.5 Transcribe model card](https://ai.google.dev/gemini-api/docs/models/gemini-3.5-transcribe)
- [Audio transcription guide](https://ai.google.dev/gemini-api/docs/transcribe) — feature matrix, locale list, incompatibilities
- [Interactions API reference](https://ai.google.dev/api/interactions-api) — input parts, `word_info` annotations, usage metadata
- [Gemini API pricing](https://ai.google.dev/gemini-api/docs/pricing)
- [Chirp 3 documentation](https://docs.cloud.google.com/speech-to-text/docs/models/chirp-3) — the unsupported-features table
- [Chirp 2 documentation](https://docs.cloud.google.com/speech-to-text/docs/models/chirp-2)
- [Cloud STT V2 supported languages](https://docs.cloud.google.com/speech-to-text/docs/speech-to-text-supported-languages) — filter by region to reproduce the `asia-south1` finding
- [Cloud STT pricing](https://cloud.google.com/speech-to-text/pricing)
- [Sarvam pricing](https://docs.sarvam.ai/api-reference-docs/pricing) and [Saaras model docs](https://docs.sarvam.ai/api/getting-started/models/saaras)
