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
| `consumer.py` | The JetStream loop. Delivery semantics, authentication and TLS only. |
| `db.py` | The Postgres pool, and the RLS context every statement runs inside. |
| `persistence.py` | The concrete `Sink`, segment index, call repository and job stores. |
| `blobstore.py` | Object storage: the key layout `blob.SegmentKey` defines, S3 and a local directory. |
| `segments.py` | Reading stored audio: Opus framing, dropped-frame silence, and the foreign-audio filter. |
| `retention.py` | The nightly purge. Dry run by default at the entrypoint. |
| `coverage.py` | Reconciliation arithmetic only. Formats live in `cdr.py`. |
| `cdr.py` | Dialer CDR adapters (**OPEN-7**). CSV is a reference implementation; a bank format is a new adapter. |
| `telemetry.py` | OTLP traces and metrics, off unless a collector is configured. |
| `service.py` | The composition root: environment in, running jobs out. |
| `__main__.py` | `sentinel-pipeline consume \| retention \| coverage \| check`. |

## Running the service

```sh
sentinel-pipeline check        # validate the configuration and exit
sentinel-pipeline consume      # the JetStream finalize consumer
sentinel-pipeline retention    # the nightly purge — dry run unless --commit
sentinel-pipeline coverage     # the nightly CDR reconciliation
```

`python -m sentinel_pipeline <command>` is the same entrypoint. `check` builds
everything `consume` builds — the ASR selection, the model providers, the database
pool — and exits, so a deploy step catches a language the chosen transcriber cannot
read, a database role that bypasses row-level security, or a broker with no
credentials, rather than a stream of failed calls doing it later.

The consumer subscribes to `sentinel.call.finalize` and expects exactly the message
the gateway's outbox publishes (`db/migrations/0007`):

```json
{"call_id": "<ulid>", "tenant_id": "<uuid>", "attempt": 1,
 "finalized_at": "2026-09-01T10:19:44.802Z"}
```

Nothing else is on the bus — no transcript, no audio, no borrower data. Everything
else is looked up from Postgres under that tenant's row-level-security context.

### Configuration

| Variable | Default | What it does |
|---|---|---|
| `SENTINEL_PIPELINE_DATABASE_URL` | *required* | DSN for the `sentinel_pipeline` role. Deliberately not the gateway's variable: the two connect as different roles, and the service refuses to start if its role can bypass RLS. |
| `SENTINEL_PIPELINE_DB_POOL_MIN` / `_MAX` | `1` / `8` | Pool bounds. Keep the maximum at or above `SENTINEL_NATS_MAX_IN_FLIGHT`. |
| `SENTINEL_PIPELINE_DB_CONNECT_TIMEOUT` | `10` | Seconds. |
| `SENTINEL_S3_BUCKET` | — | S3 bucket; selects the S3 backend. Same variable the gateway writes with — the two must point at the same bucket. |
| `SENTINEL_S3_REGION` | `ap-south-1` | OPEN-4 says India; this default says so too. |
| `SENTINEL_S3_ENDPOINT` | — | MinIO or another S3-compatible endpoint. |
| `SENTINEL_BLOB_DIR` | — | Local directory; selects the development backend. One of this or the bucket is required. |
| `SENTINEL_SEGMENT_CODEC` | `opus` | `pcm16` when the stored objects are raw PCM fixtures rather than Opus. |
| `SENTINEL_NATS_SERVERS` | `nats://127.0.0.1:4222` | Comma-separated. A `tls://` URL turns TLS on. |
| `SENTINEL_NATS_DURABLE` | `finalize-workers` | Durable consumer name. |
| `SENTINEL_NATS_MAX_IN_FLIGHT` | `8` | Also the fetch batch size. |
| `SENTINEL_NATS_ACK_WAIT_SECONDS` | `300` | Well above the slowest realistic analysis. |
| `SENTINEL_NATS_MAX_DELIVER` | `4` | Then the dead-letter subject. |
| `SENTINEL_NATS_CREDS` | — | `.creds` file. The production credential. |
| `SENTINEL_NATS_NKEY_SEED` | — | nkey seed file. |
| `SENTINEL_NATS_USER` / `SENTINEL_NATS_PASSWORD` | — | Must be set together. |
| `SENTINEL_NATS_TOKEN` | — | Last resort. |
| `SENTINEL_NATS_TLS` | off | Require TLS. |
| `SENTINEL_NATS_CA` | system trust | CA bundle for the broker. |
| `SENTINEL_NATS_CLIENT_CERT` / `_KEY` | — | Mutual TLS; must be set together. |
| `SENTINEL_NATS_TLS_HOSTNAME` | — | Name to verify when the address is not the certificate's. |
| `SENTINEL_NATS_MAX_RECONNECT_ATTEMPTS` | `-1` | Forever, including the first connect: the consumer waits for the broker rather than exiting. Set a finite number where a crash loop is more visible than a quiet wait. |
| `SENTINEL_NATS_ALLOW_INSECURE` | off | Permit a remote broker with no credentials. Refused otherwise, because every message authorises model spend against a named tenant. |
| `SENTINEL_ANALYSIS_PROVIDER` | `none` | `anthropic`, `openai`, `fake` or `none` (tier-1 compliance only). Never guessed. |
| `SENTINEL_JUDGE_PROVIDER` | `none` | Same values, judged against `contracts/schemas/judge.json`. |
| `SENTINEL_ANALYSIS_MODEL` / `SENTINEL_JUDGE_MODEL` | adapter default | Model id per slot. |
| `SENTINEL_ANTHROPIC_API_KEY` / `SENTINEL_OPENAI_API_KEY` | — | Required by the provider that is selected. |
| `SENTINEL_MODEL_PRICING` | empty | `model=<input paise per Mtok>/<output paise per Mtok>,…`. Unpriced models are reported loudly rather than recorded as free. |
| `SENTINEL_RETENTION_COMMIT` | off | Without it, `retention` only reports what it would delete. |
| `SENTINEL_RETENTION_BATCH_SIZE` | `1000` | Rows per purge batch. |
| `SENTINEL_CDR_ADAPTER` | unset | `csv`, or unset/`none` to skip reconciliation (**OPEN-7**). |
| `SENTINEL_CDR_DIR` | — | Where the dialer export is delivered. |
| `SENTINEL_CDR_PATH_TEMPLATE` | `{tenant_id}/{day}.csv` | Formatted with `tenant_id` and the ISO `day`. |
| `SENTINEL_CDR_COLUMNS` | repo fixture names | `field=column,…` overrides. A typo is refused, not ignored. |
| `SENTINEL_CDR_DURATION_UNIT` | `s` | `s`, `ms` or `hms`. |
| `SENTINEL_CDR_TIMESTAMP_FORMAT` | ISO 8601 | `strptime` format. |
| `SENTINEL_CDR_TIMEZONE` | `Asia/Kolkata` | Applied to naive timestamps. |
| `SENTINEL_CDR_DELIMITER` | `,` | |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | unset | Setting it enables telemetry. Nothing is imported or exported without it. |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `http/protobuf` | `grpc` needs the `otlp-grpc` extra. |
| `SENTINEL_OTEL_ENABLED` | unset | `1` to export to the OTLP default endpoint; `0` to force off. |
| `OTEL_SERVICE_NAME` | `sentinel-pipeline` | |
| `SENTINEL_LOG_LEVEL` | `INFO` | Logs are JSON, one object per line. |

The ASR selection has its own variables, documented in `providers/registry.py`:
`SENTINEL_ASR_PROVIDER`, `SENTINEL_ASR_LANGUAGES`, `SENTINEL_ASR_ROUTES`,
`SENTINEL_GOOGLE_API_KEY`, `SENTINEL_SARVAM_API_KEY`. The floor's per-tenant language
comes from `tenants.policy->>'language'`, and the kill switch from
`tenants.policy->>'model_kill_switch'`.

## Running the tests

```sh
python -m venv .venv && .venv/bin/pip install -e '.[dev]'
.venv/bin/python -m pytest
```

The suite needs no broker, no database, no object store and no model provider: the
SDKs are imported inside the code that uses them, so in practice `pytest` and
`jsonschema` are enough.

```sh
bash tests/pg_integration.sh
```

The same persistence code against a throwaway PostgreSQL with every migration
applied, connected as `sentinel_pipeline`. It asserts the pipeline's half of what
`db/test/rls_test.sh` asserts for the gateway — a query with no tenant context
returns zero rows rather than everything — plus the two cases where a redelivered
message must lose to a human: a reviewed flag and an agent-corrected promise to pay.
Without the two `SENTINEL_PIPELINE_TEST_*` DSNs those tests skip, which is what keeps
`python -m pytest` runnable with no database.

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

**Foreign audio is stored and must never be transcribed.** `contracts/wire.md` §4.2:
on a tier B capture the loopback stream carries everything the agent's speakers play,
and the client flags whatever arrived while the softphone session was inactive. The
gateway stores it — being able to prove what was discarded is worth the space — and
`segments.py` is the one place it is filtered out, in the SQL and again in Python.
Getting that wrong is not a quality regression: it files RBI conduct findings quoted
from an agent's music, against an agent who never said any of it.

**Telemetry is a place borrower data can leak.** `telemetry.py` allowlists attribute
keys per signal and drops everything else. `tenant_id` is fine anywhere; `call_id` is
allowed on spans and refused on metrics, where it would be one time series per call;
user uids, account references, transcript text and evidence spans are refused
everywhere, and a failing provider's exception *message* is never recorded, because a
provider that echoes its input can put a transcript fragment in it.
