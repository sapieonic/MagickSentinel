# sentinel-pipeline

ASR, analysis and compliance workers.

Three swappable provider slots sit behind interfaces, and no provider SDK is imported
outside its adapter in `sentinel_pipeline/providers/`. Every stored artifact records
the provider and version that produced it, so results from before and after a model
change stay distinguishable.

## Layout

| Module | What it does |
|---|---|
| `models.py` | Domain types. No database session, so the rule engine is testable against a fixture corpus. |
| `asr/` | `BatchASR` and `StreamingASR` interfaces, plus WER and **numeric-entity error rate** metrics. |
| `providers/registry.py` | The one place a batch ASR provider is chosen. Default is `gemini-3.5-transcribe`; refuses a floor language the chosen provider cannot read. |
| `analysis/` | One LLM call per finalized call, validated against `contracts/schemas/analysis.json`. |
| `compliance/engine.py` | Tier 1: deterministic rules over the transcript and metadata. Runs on 100% of calls. |
| `compliance/judge.py` | Tier 2: LLM judge over flagged calls plus a deterministic sample. |
| `cost.py` | Per-tenant budgets, per-call ceilings, the 15-second floor, and the kill switch. |
| `worker.py` | The finalize sequence, and how it degrades when a provider fails. |
| `consumer.py` | The JetStream loop. Delivery semantics only. |

## Running the tests

```sh
python -m venv .venv && .venv/bin/pip install -e '.[dev]'
.venv/bin/python -m pytest
```

The suite needs no broker, no database, and no model provider.

## Three things to know before changing anything here

**Analysis failure must not stop compliance.** Tier-1 rules run off the transcript,
cost nothing, and are what the customer is buying. `test_worker.py` pins this.

**Indian-language matching depends on two folds, not on the term list alone.**
Every non-English term in the default rule set ships in both its native script and a
romanisation, because ASR output for one Hinglish call is not consistently in one
script. `compliance/engine.py` folds each side by the script of the term itself:
`romanised()` collapses inflection, vowel length and z/j-v/w variance in
transliterations, `indic()` strips trailing vowel signs. So a list carries the base
form — `kamine`, `कमीने` — and matches `kaminon` and `कमीनों` for free, while
genuinely different spellings (`bhikhari`/`bhikari`) still need their own entry.

Two consequences worth holding on to. `normalise()` must strip by Unicode category
and never by `\w`: `\w` excludes combining marks, and a punctuation strip written
against it deletes every Devanagari matra, which silently disabled native-script
matching for four of the five supported languages until it was fixed. And the
romanised fold trades precision for recall — `chore` folds onto Hindi `chor` — which
is affordable only because every conduct rule that uses it carries `judge: true`, so
tier 2 sees the loose hit and dismisses it. Do not extend that fold to a rule the
judge does not review.

**Numeric accuracy is tracked separately from WER.** Overall WER can sit at a
respectable 18% while every third amount is wrong, because numbers are a tiny
fraction of the tokens and carry all of the meaning. A promise to pay of ₹15,000
misheard as ₹50,000 destroys trust in the whole product.
