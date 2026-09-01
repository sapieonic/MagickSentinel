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

## Two things to know before changing anything here

**Analysis failure must not stop compliance.** Tier-1 rules run off the transcript,
cost nothing, and are what the customer is buying. `test_worker.py` pins this.

**Numeric accuracy is tracked separately from WER.** Overall WER can sit at a
respectable 18% while every third amount is wrong, because numbers are a tiny
fraction of the tokens and carry all of the meaning. A promise to pay of ₹15,000
misheard as ₹50,000 destroys trust in the whole product.
