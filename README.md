# MagickVoice Sentinel

Sentinel is a call monitoring product for debt collections floors in India. Collections
BPOs make outbound calls using dialer systems their bank clients mandate and that cannot
be replaced, so Sentinel captures call audio at the agent's Windows desktop instead,
ships it to a server, and produces a per-call summary and disposition, an extracted
promise-to-pay, per-speaker sentiment, and compliance flags against RBI fair-practices
and recovery-agent guidelines.

The compliance flagging is the part customers buy. A BPO adopts Sentinel to tell its
bank client that 100% of calls are monitored rather than the 2–5% manual QA sample it
samples today. Where correctness has to be traded off, compliance correctness wins over
sentiment sophistication.

Two consumers: a small always-on desktop widget for the agent, and a web portal for
supervisors, QA, compliance, and the bank client.

The authoritative build specification lives outside this repository. This README
describes what the repository contains today and how to work with it.

## Repository layout

```
contracts/        OpenAPI, the WSS binary protocol, JSON Schemas for AI output
db/               SQL migrations and the row-level-security acceptance tests
client/           Rust workspace: sentinel-core, sentinel-capture,
                  sentinel-service, sentinel-agent, and the WiX installer
server/gateway/   Go: REST API and WSS ingest
server/pipeline/  Python: ASR, analysis, compliance, cost, retention, coverage
web/              React: shared components plus the widget and portal apps
deploy/           Container images, the compose stack, the migration runner,
                  and the OpenTelemetry / Grafana observability configuration
docs/             Deployment, security, architecture, Phase 0, open decisions
```

`contracts/` is the source of truth. Client, gateway and web are all expected to
generate or hand-write types against it, and a change to an API shape starts there. The
wire protocol has a shared fixture, `contracts/fixtures/wire_vectors.json`, that both
the Rust codec test and the Go codec test read, so the two implementations cannot drift
without a test failing.

## Running the test suites

Every command below was run against this working tree and passes, with one stated
exception: `tests/pg_integration.sh` needs a dependency this container does not carry, and
is described from its source rather than from a run. Several work streams are editing the
repository at once, so if one of them fails for you, check `git log` before assuming the
command is wrong.

```
cd client && cargo test
```

342 tests across the whole workspace: the call state machine, the spool, the wire
codec, the VAD, the resampler, device matching, tier classification, foreign-audio
suppression, the service's IPC codec and supervisor, the PKCS#10 CSR builder and the
DER encoder under it (`client/sentinel-service/src/csr.rs`, `src/der.rs`), the enrollment
exchange, the spool-key wrap/unwrap (`src/spoolkey.rs`) and the OTLP log encoder, plus an
integration test against the shared wire fixture. No audio hardware, no network, no
database. This is the suite to run while working on the client.

```
cd client && cargo check --target x86_64-pc-windows-gnu
```

Type-checks the Windows-only code — the COM work under
`client/sentinel-capture/src/windows/` and `client/sentinel-service/src/windows/`, and
the CNG and DPAPI code under `client/sentinel-service/src/devicekey/cng.rs` and
`src/spoolkey.rs` — which is compiled out on Linux. The target and mingw-w64 are
installed in the dev container; mingw is needed because `rusqlite` and `audiopus` build C
sources for the target rather than only type-checking Rust. Add `--all-targets` to cover
the test targets too. This catches signature and feature-flag breakage in code that no
test exercises; it does not tell you the code works.

The `rust-windows` job in `.github/workflows/ci.yml` runs the suite on `windows-latest`
under MSVC, then runs it again in the release configuration with
`--features sentinel-core/sqlcipher` — the first thing anywhere that executes the
`PRAGMA key` path — builds `SentinelAgent.exe` and `SentinelService.exe`, and asserts
they import no DLL a clean Windows 10 image lacks. **It is green.**

Be precise about what that does and does not establish. It establishes that the code
compiles and *links* for MSVC, that the platform-neutral suite passes on Windows in both
profiles, and that the shipping binaries build with no stray runtime dependency. It does
not establish that the Windows-only code works: there are still zero tests under
`client/**/src/windows/`, the credential tests inject a stub device key rather than
touching CNG, and no hardware-in-the-loop test has run against a real audio endpoint. A
hosted runner has no audio endpoint and no softphone, so it never will.

```
bash db/test/rls_test.sh
```

Row-level security acceptance tests. Boots a throwaway PostgreSQL 16 cluster, applies
every migration, and asserts the isolation properties directly against the database as
`sentinel_app` — the NOBYPASSRLS role the gateway actually connects as. Eighteen checks
at the time of writing, covering cross-tenant reads, agent-to-agent isolation, supervisor
team scoping, the client role's flagged-calls-only view, and the missing-context case,
which must return zero rows rather than all of them. Run this after any change to
`db/migrations/`.

`db/test/pgtest.sh` is the harness the three database-backed scripts share — this one,
`gateway_it.sh`, and the pipeline's `tests/pg_integration.sh` below. It re-executes the
calling script as an unprivileged user when run as root, because `initdb` refuses to run
as root, and it installs a stub `vector` extension when pgvector is absent so the schema
still applies. Production images must carry the real pgvector.

```
bash db/test/gateway_it.sh
```

Gateway integration tests against a migrated throwaway PostgreSQL. Same harness, then
`go test ./...` with `SENTINEL_TEST_DATABASE_URL` and `SENTINEL_TEST_ADMIN_DATABASE_URL`
pointed at it. The app DSN connects as `sentinel_app`; the admin DSN is the schema owner,
used for seeding and for read-backs that deliberately check rows the application role
must not be able to see.

```
cd server/gateway && go test ./...
```

Runs the same packages without a database. The integration tests skip rather than fail
when the two `SENTINEL_TEST_*` variables are unset, so this exercises the codec, the
ingest session logic and the token verifier only. Use `gateway_it.sh` when you need the
database-backed coverage.

```
cd server/pipeline && python -m pytest
```

The rule-engine, analysis, judge, cost and worker tests, and now also the persistence
layer, the object-store segment reader, the retention and coverage jobs, the CDR adapter
registry, the service entry point and the telemetry setup. `pip install -e '.[dev]'` from
`server/pipeline` is the documented setup and works on Python 3.11 and 3.12: that gives
675 passed and 19 skipped, the 19 being the Postgres integration tests below. In practice
the suite needs only `pytest` and `jsonschema` — 672 passed and 22 skipped, the extra
three being the OpenTelemetry SDK tests in `tests/test_telemetry.py` — because the
provider SDKs are imported inside their adapters and the rest of the declared runtime
dependencies are not reached by anything under test.

`retention.py` and `coverage.py` used to have no test coverage at all. They now have
`tests/test_retention_jobs.py` and `tests/test_coverage.py` behind them, which matters
more for retention than for anything else in this repository: its failure mode is
deleting the wrong data, and until those tests existed the module had never deleted
anything. It also defaults to a dry run — `RetentionJob(dry_run=True)`, and the CLI needs
an explicit `--commit`.

```
cd server/pipeline && bash tests/pg_integration.sh
```

Nineteen integration tests against a migrated throwaway PostgreSQL, on the same harness.
This one needs the `psycopg` extra as well as a database, so unlike the two scripts above
it is not runnable from a bare checkout. They connect as
`sentinel_pipeline`, the second NOBYPASSRLS role, and assert that the pipeline's own
writes and reads are confined to one tenant — including that `Database.assert_rls_enforced`
(`sentinel_pipeline/db.py`) refuses to start against a role that can bypass row-level
security, which is what a DSN accidentally pointed at the schema owner looks like. Without
the two `SENTINEL_PIPELINE_TEST_*` variables these skip, so the default suite stays
runnable with no database.

```
cd web && npm run typecheck && npm test && npm run build
```

Typechecks the three workspaces, runs the 224 vitest tests, and builds the widget and the
portal with vite. All three ran green while this was written.

**`npm ci` works now.** The lockfile that predated the `@sentinel/shared` and
`@sentinel/portal` workspaces has been regenerated and committed, so a clean install runs;
`npm ci --dry-run` in `web/` is the one-line check. The CI web job used to carry that
dry-run as a guard and skip itself when it failed. It no longer skips — it runs `npm ci`
and gates — which also means the job now builds the widget bundle that the MSI packages,
so a broken web build fails the build rather than producing an installer with a blank
widget in it. Use `npm ci` rather than `npm install` when you want the lockfile enforced.

### Advisory rather than gating

`cargo fmt --check` currently reports diffs across most of the client tree. It runs in
CI as an advisory step rather than a gate, because failing the build on formatting in a
tree that several work streams are editing simultaneously would block unrelated work;
drop `continue-on-error` from that step once a `cargo fmt` pass has landed.

`cargo clippy` does gate, but still without `-D warnings`. On Linux the tree is now
clean — `cargo clippy --all-targets` from `client/` reports zero warnings — so the flag
can be added to the Linux job today, and the comment in `.github/workflows/ci.yml` that
says the tree "currently has two clippy warnings" is out of date. The MSVC job in the same
workflow lints the `cfg(windows)` modules that Linux clippy cannot see; that job now runs,
so its count is observable in the log rather than unknown. The Linux job already gates on
`-D warnings`. Take the MSVC gate in its own commit, once a run has reported a count and
it has been cleared, so a first failure there is about a change rather than about
pre-existing lint debt.

## What state each component is in

The repository is under active development across several parallel work streams, so
treat this as a snapshot rather than a fixed inventory. Everything below was read from
the tree at commit `2a2bb02`, not inferred; where something is written but unexercised it
says so, and that distinction is doing more work in this section than it used to.

**`contracts/`** — usable. `openapi.yaml` is a valid OpenAPI 3.1 document, and every
path in it is currently routed by the gateway. `wire.md` specifies version 1 of the
binary ingest protocol and is implemented on both sides; `/v1/ingest` is described there
rather than in the OpenAPI document, which is the one place the two diverge. `schemas/`
holds three JSON Schema 2020-12 documents — `analysis.json`, `judge.json` and
`rule_set.json`. The CI contracts job checks that all of these parse and that the schemas
are valid; nothing checks the contract against the implementation, so that agreement is
maintained by hand.

**`db/`** — the most complete part of the repository. Eight migrations build the schema,
enable row-level security on every tenant-scoped table, create the `sentinel_app` and
`sentinel_pipeline` roles as `NOBYPASSRLS`, install a default rule set, and add four
narrow `SECURITY DEFINER` functions for the operations that legitimately precede a tenant
context: consuming an enrollment token, registering the enrolled device, resolving a
client certificate back to a device (`0005_bootstrap_functions.up.sql`), and — added with
the scheduled pipeline jobs — `sentinel_pipeline_tenants()`, which is how a nightly job
learns which tenants exist when the `tenants_self` policy would otherwise require a tenant
context to discover one (`0008_pipeline_jobs.up.sql`). The set of tenant-crossing
operations is still small and still greppable, but it is four now rather than three.
`0007_finalize_outbox.up.sql` adds `call_finalize_outbox`, the transactional outbox
described under the gateway below. The schema goes beyond the spec's tables where the
implementation needed it — `teams`, `enrollment_tokens`, `ingest_watermarks`,
`prompt_templates`, `device_events` and `default_rule_set`.

**`client/sentinel-core`** — the platform-neutral half, and it is real and tested: the
call state machine including hold-resume and mid-call sign-out, the spool with
ack-gated deletion and reported eviction, the wire codec, the policy types, and the
uplink's retry policy.

**`client/sentinel-capture`** — split. The testable parts (the `CaptureSource` trait,
the WAV replay source, VAD, the stateful resampler, container-ID device matching, tier
classification, foreign-audio suppression) are implemented and tested. The Windows COM
implementations — tier A process loopback, tier B endpoint loopback, softphone session
tracking, device-change notification, OS build detection from the registry — are written
and type-check for the Windows target, but nothing exercises them. The Windows CI job in
`.github/workflows/ci.yml` now runs and is green, which means this code compiles, links
and ships inside the binaries CI produces — it still does not exercise a single line of
WASAPI, because a hosted runner has no audio endpoint and no softphone. There is no
hardware-in-the-loop test, so treat these modules as unverified rather than as working.

**`client/sentinel-service`** — substantial and unit-tested: the service control-manager
entry point, the named-pipe host and its length-prefixed JSON codec, the agent
supervisor with exponential backoff and restart counting, service recovery
configuration, config sync, update staging, crash-dump handling and device identity. It
now also owns the machine's two secrets. `src/devicekey/` generates the device's P-256
key **non-exportably in CNG** and signs with it through `NCryptSignHash`, so the private
key is never a file and never in the process's address space; `src/csr.rs` and `src/der.rs`
build the PKCS#10 request against it. `src/spoolkey.rs` wraps the SQLCipher key with DPAPI
at **machine** scope — machine rather than user because the service and the agent run as
different principals. A software-key fallback exists for Linux CI and refuses to
construct itself in a release build without the `dev-software-device-key` feature; which
kind signed a CSR travels with the credential rather than being inferred. Like
`sentinel-capture`, its Windows-specific halves type-check and are not executed by
anything that has run.

**`client/sentinel-agent`** — no longer a scaffold. `src/main.rs` wires the real thing:
PKCE against Identity Platform in the system browser, the device credential, the Opus
encoder, the spool, the WSS uplink and the WebView2 widget host. Two things worth knowing
because the old text said the opposite of both. `device_certificate()` loads a real
credential (`src/device.rs`) and presents it over mTLS through a `rustls` signer that
delegates to CNG, so there is no path that connects without a client certificate. And
`spool_key()` reads the DPAPI-wrapped blob rather than falling back to a literal: the
`"unconfigured"` default and the `SENTINEL_SPOOL_KEY` environment variable are gone, and
the caller blocks capture on the error instead, because a spool that looks encrypted
while every machine on the floor shares one key is worse than no encryption at all.

An older TODO in `src/main.rs` claimed that `windows-rs` 0.58 exposes no NCrypt surface
for generating a non-exportable P-256 key. That was checked and is false — the pinned
0.58 exports the whole `NCrypt*` set under the `Win32_Security_Cryptography` feature the
crate already enables — and the note recording it is in
`client/sentinel-service/src/devicekey/mod.rs`. If you find that claim repeated anywhere,
it is stale.

**`client/installer`** — a full WiX v4 package: `Sentinel.wxs`, `Sentinel.wixproj` and
`build.ps1`, which builds both binaries for `x86_64-pc-windows-msvc` with
`--features sentinel-core/sqlcipher`, signs them, builds the MSI and signs that.
`.github/workflows/release.yml` runs the same order on a tag.

**`build.ps1` has still never been executed, no MSI has been built, nothing has been
signed, and no release has been produced.** Treat the packaging as authored rather than
as working.

Two narrower things are no longer true, since this section used to claim no PowerShell
here had ever run. `.github/scripts/assert-no-stray-dll-deps.ps1` runs on every Windows
CI job, and it earned its place immediately: it caught the shipping binaries importing
`vcruntime140.dll` before any tag existed. And every `.ps1` in the repository now parses
— which sounds like nothing until you know that the first execution of that gate failed
on a `??` that throws instead of coalescing under `Set-StrictMode`, a bug no amount of
reading had found.

**`server/gateway`** — a working service. It verifies Identity Platform ID tokens
against Google's JWKS, cross-checks the device certificate's tenant against the token's,
enforces the `/v1/me` namespace rule mechanically in middleware, runs every query inside
a transaction carrying the row-level-security context, and serves the WSS ingest
endpoint with cumulative per-channel acks and idempotent `(call_id, channel, seq)`
writes. The two gaps this section used to list are closed. `internal/ca` is a production
PEM-backed intermediate CA — it deliberately cannot *create* a CA, only load one, and
refuses a certificate not marked as a CA — and `main.go` now refuses to start without
`SENTINEL_CA_CERT` and `SENTINEL_CA_KEY` rather than booting into a gateway that answers
`503 no_ca` to every enrollment. `internal/blob/s3.go` is a real S3 backend defaulting to
`ap-south-1`, so `SENTINEL_BLOB_DIR` is now the development path and logs a warning.

Several things are new alongside those. `POST /v1/oauth/token` (`internal/api/token.go`,
`internal/idp`) brokers the authorization-code exchange against Identity Platform
server-side, so the desktop stays a public client per RFC 8252 and no client secret or
API key ships in an MSI; `internal/idp` has no unit tests of its own, though the endpoint
around it does. `main.go` wires `LiveTickets` — the supervisor's SSE floor view was
answering 503 — and `AllowedOrigins`, where there was previously no CORS handling at all.
`GET /readyz` probes the database and the object store, separately from `/healthz`, which
only answers that the process is alive. And `internal/outbox` plus migration 0007 make the
gateway the producer for `sentinel.call.finalize`: the finalize and the intent to publish
commit in one transaction and a drainer moves rows into JetStream, because a publish that
fails after the commit means a call that is captured, stored, billed and never analysed,
with no error anywhere. Request logging now folds `trace_id` and `span_id` into the
existing line (`internal/httpx/httpx.go`) from the OpenTelemetry setup in
`internal/telemetry`.

**`server/pipeline`** — the full shape is now present. All ten tier-1 rules are
implemented and tested against a fixture corpus; the tier-2 judge validates against
`contracts/schemas/judge.json` and discards an upheld verdict carrying no evidence span;
the analyser validates against `contracts/schemas/analysis.json`; `worker.py`
orchestrates ASR to analysis to compliance as a pure function over injected interfaces,
with a deliberate degradation order (analysis failing must not stop compliance);
`consumer.py` wraps that in a NATS JetStream loop; `cost.py` implements the budget,
ceiling, minimum-duration and kill-switch controls; `asr/evaluate.py` computes WER and
numeric-entity error rate separately; and `providers/` holds adapters for Sarvam,
Whisper, Anthropic and OpenAI alongside a deterministic fake, each importing its SDK
inside the adapter.

The `Protocol` interfaces now have concrete implementations behind them, which is the
change that turns this from a library into a service. `db.py` opens a pool as
`sentinel_pipeline` under row-level security and refuses to start if that role can bypass
it; `persistence.py` is the `Sink`, writing transcripts, analyses, findings and the
promise-to-pay without ever overwriting a human correction; `blobstore.py` and
`segments.py` are the `SegmentSource`, reading Opus out of object storage, decoding it and
dropping foreign-marked segments twice over — once in SQL and once in Python — because
transcribing a tier B machine's music and filing the hits against an agent is a fabricated
compliance record rather than a quality regression. `service.py` and `__main__.py` give it
an entry point and a CLI (`consume | retention | coverage | check`), the NATS client speaks
authentication and TLS, `cdr.py` holds an adapter registry with CSV as the reference
implementation, migration 0008 adds what the scheduled jobs need, and `telemetry.py` sets
up OpenTelemetry. One caveat on cost: `persistence.py` writes the analysis row before the
judge runs and there is one `cost_paise` column on `analyses`, so per-tenant spend totals
are low by the judge's share — the module says so, and it is low rather than high, but a
budget ceiling computed from that figure is not the whole bill.

**`web/`** — all three workspaces exist, build and are tested (224 tests). `web/shared/`
holds the API client, the role capability map, money formatting in paise, and
presentational components including the non-dismissible recording indicator; `web/widget/`
and `web/portal/` are vite applications with their own vitest suites. The portal now signs
in against Google Identity Platform for real (`web/portal/src/auth/`), replacing the
`window.__SENTINEL_PORTAL_TOKEN__` seam, and the shared `ApiClient` refreshes tokens and
buys exactly one forced refresh and one retry on a 401 before giving up, so a stale token
puts the user on the sign-in screen rather than into a retry loop. The widget's calls into
its WebView2 host are time-bounded, because a host that never answers used to hang the
widget rather than degrade it. The widget's vite build is configured to emit **one
self-contained file** — `web/widget/dist/index.html` with everything inlined and no
`assets/` directory beside it — which `.github/scripts/stage-widget.ps1` stages as
`widget.html` and refuses to stage if anything was left un-inlined.
`client/installer/Sentinel.wxs` packages exactly that one file, so vite's default output
of `index.html` plus hashed assets would have produced an MSI that installed cleanly,
reported healthy, and rendered a blank widget — including the recording indicator, which
is a compliance requirement rather than a nicety.

**`deploy/` and `.github/`** — authored, and almost entirely unexecuted. `deploy/` holds
Dockerfiles for the two server services, a compose stack (Postgres with pgvector, NATS
with authentication, MinIO), a migration runner that records a SHA-256 per applied file,
and `deploy/observability/` with an OTel Collector, Tempo, Prometheus, Loki and Grafana,
provisioned as code, carrying one dashboard of fourteen panels across six rows and
eighteen alert rules. `.github/workflows/` gains the `windows-latest` job described above
and a tag-triggered release workflow that builds and signs the MSI.

Be precise about what that means. The observability configuration was validated as
configuration, and the collector's redaction processor was exercised against a real
payload. Beyond that: no container image has been built, the compose stack has not been
stood up, no dashboard has ever displayed a data point, no alert rule has ever fired, and
neither the Windows CI job nor the release workflow has run. This is the part of the
repository where the gap between "written" and "working" is widest, and the reason to say
so here is that a reader who assumes otherwise will plan a pilot around it.

## Documentation

- [`illustration/`](illustration/) — animated diagrams of the build, the service map, call
  detection, the ingest protocol and tenant isolation. Published as GitHub Pages.
- [`docs/local-setup-guide.md`](docs/local-setup-guide.md) — getting the stack running on
  a development machine, and what to poke at once it is.
- [`deploy/README.md`](deploy/README.md) — the container images, the compose stack, the
  migration runner and the observability configuration, including what the local stack
  cannot do and why there is deliberately no Terraform.
- [`docs/architecture.md`](docs/architecture.md) — the system shape and the design
  decisions that get questioned most often.
- [`docs/deployment.md`](docs/deployment.md) — customer-facing: the Windows support
  matrix, what tier B means in practice, headset pinning, WebView2, and the EDR
  conversation.
- [`docs/security.md`](docs/security.md) — the compliance and security requirements
  mapped to where they are implemented, with file paths, and what is outstanding.
- [`docs/asr-provider-selection.md`](docs/asr-provider-selection.md) — the ASR candidate
  shortlist as of September 2026: what each provider does and does not support, what it
  costs at floor scale, and why Cloud Speech-to-Text is not on the list.
- [`docs/phase-0-checklist.md`](docs/phase-0-checklist.md) — the discovery checklist and
  its go/no-go criterion.
- [`docs/open-decisions.md`](docs/open-decisions.md) — OPEN-1 to OPEN-8, what each
  blocks, and what the build has assumed in the meantime.
