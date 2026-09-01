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
                  sentinel-service, sentinel-agent, and an empty installer/
server/gateway/   Go: REST API and WSS ingest
server/pipeline/  Python: ASR, analysis, compliance, cost, retention, coverage
web/              React: shared components plus the widget and portal apps
docs/             Deployment, security, architecture, Phase 0, open decisions
```

`contracts/` is the source of truth. Client, gateway and web are all expected to
generate or hand-write types against it, and a change to an API shape starts there. The
wire protocol has a shared fixture, `contracts/fixtures/wire_vectors.json`, that both
the Rust codec test and the Go codec test read, so the two implementations cannot drift
without a test failing.

## Running the test suites

Every command below was run against this working tree and passes. Several work streams
are editing the repository at once, so if one of them fails for you, check `git log`
before assuming the command is wrong.

```
cd client && cargo test
```

Unit tests across the whole workspace: the call state machine, the spool, the wire
codec, the VAD, the resampler, device matching, tier classification, foreign-audio
suppression, the service's IPC codec and supervisor, plus an integration test against the
shared wire fixture. No audio hardware, no network, no database. This is the suite to run
while working on the client.

```
cd client && cargo check --target x86_64-pc-windows-gnu
```

Type-checks the Windows-only code — the COM work under
`client/sentinel-capture/src/windows/` and `client/sentinel-service/src/windows/` — which
is compiled out on Linux. The target and mingw-w64 are installed in the dev container;
mingw is needed because `rusqlite` and `audiopus` build C sources for the target rather
than only type-checking Rust. Add `--all-targets` to cover the test targets too. This
catches signature and feature-flag breakage in code that no test exercises; it does not
tell you the code works.

```
bash db/test/rls_test.sh
```

Row-level security acceptance tests. Boots a throwaway PostgreSQL 16 cluster, applies
every migration, and asserts the isolation properties directly against the database as
`sentinel_app` — the NOBYPASSRLS role the gateway actually connects as. Fourteen checks,
covering cross-tenant reads, agent-to-agent isolation, supervisor team scoping, the
client role's flagged-calls-only view, and the missing-context case (which must return
zero rows, not all of them). Run this after any change to `db/migrations/`.

`db/test/pgtest.sh` is the harness the two database scripts share. It re-executes the
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

The rule-engine, analysis, judge, cost and worker tests. `pip install -e '.[dev]'` from
`server/pipeline` is the documented setup and works on Python 3.11 and 3.12; in practice
the suite needs only `pytest` and `jsonschema`, because the provider SDKs are imported
inside their adapters and the rest of the declared runtime dependencies are not reached
by anything under test. A prepared virtualenv exists at `.venv-dev/` in this container,
so `.venv-dev/bin/python -m pytest` from `server/pipeline` works without further setup.

Two modules have no test coverage at all: `sentinel_pipeline/retention.py` and
`sentinel_pipeline/coverage.py`.

```
cd web && npm run typecheck && npm test && npm run build
```

Typechecks the three workspaces, runs the vitest suites, and builds the widget and the
portal with vite. All three ran green against an installed `web/node_modules` while this
was written. The workspace is being edited continuously, so expect transient failures
here in a way you should not expect from the Rust, Go and Python suites.

**`npm ci` does not work yet, and that is structural rather than transient.**
`web/package-lock.json` is out of sync with the workspaces — it predates
`@sentinel/shared` and `@sentinel/portal` — so a clean install refuses to run at all. The
fix is one command in `web/`, `npm install`, with the regenerated lockfile committed.
Until that lands, the CI web job detects the out-of-sync lockfile and skips with a notice
rather than failing. The guard is a real `npm ci --dry-run`, so the job starts enforcing
by itself the moment the lockfile is fixed.

### Advisory rather than gating

`cargo fmt --check` currently reports diffs across most of the client tree. It runs in
CI as an advisory step rather than a gate, because failing the build on formatting in a
tree that several work streams are editing simultaneously would block unrelated work;
drop `continue-on-error` from that step once a `cargo fmt` pass has landed. `cargo
clippy` does gate, but without `-D warnings` — it reports a handful of warnings and exits
zero, so add the flag when the tree is clean.

## What state each component is in

The repository is under active development across several parallel work streams, so
treat this as a snapshot rather than a fixed inventory. Everything below was read from
the tree at commit `3056427`, not inferred; where something is a scaffold it says so.

**`contracts/`** — usable. `openapi.yaml` is a valid OpenAPI 3.1 document, and every
path in it is currently routed by the gateway. `wire.md` specifies version 1 of the
binary ingest protocol and is implemented on both sides; `/v1/ingest` is described there
rather than in the OpenAPI document, which is the one place the two diverge. `schemas/`
holds three JSON Schema 2020-12 documents — `analysis.json`, `judge.json` and
`rule_set.json`. The CI contracts job checks that all of these parse and that the schemas
are valid; nothing checks the contract against the implementation, so that agreement is
maintained by hand.

**`db/`** — the most complete part of the repository. Five migrations build the schema,
enable row-level security on every tenant-scoped table, create the `sentinel_app` and
`sentinel_pipeline` roles as `NOBYPASSRLS`, install a default rule set, and add three
narrow `SECURITY DEFINER` functions for the only three operations that legitimately
precede a tenant context: consuming an enrollment token, registering the enrolled
device, and resolving a client certificate back to a device. The schema goes beyond the
spec's tables where the implementation needed it — `teams`, `enrollment_tokens`,
`ingest_watermarks`, `prompt_templates`, `device_events` and `default_rule_set`.

**`client/sentinel-core`** — the platform-neutral half, and it is real and tested: the
call state machine including hold-resume and mid-call sign-out, the spool with
ack-gated deletion and reported eviction, the wire codec, the policy types, and the
uplink's retry policy.

**`client/sentinel-capture`** — split. The testable parts (the `CaptureSource` trait,
the WAV replay source, VAD, the stateful resampler, container-ID device matching, tier
classification, foreign-audio suppression) are implemented and tested. The Windows COM
implementations — tier A process loopback, tier B endpoint loopback, softphone session
tracking, device-change notification, OS build detection from the registry — are written
and type-check for the Windows target, but nothing exercises them. There is no Windows CI
runner and no hardware-in-the-loop test, so treat them as unverified rather than as
working.

**`client/sentinel-service`** — substantial and unit-tested: the service control-manager
entry point, the named-pipe host and its length-prefixed JSON codec, the agent
supervisor with exponential backoff and restart counting, service recovery
configuration, config sync, update staging, crash-dump handling and device identity.
Like `sentinel-capture`, its Windows-specific halves type-check but do not run in CI.

**`client/sentinel-agent`** — a scaffold as of this writing. `src/lib.rs` is a
placeholder and `src/main.rs` is an empty `main`. A PKCE implementation exists at
`src/auth/pkce.rs` but is not yet reachable from the crate root, so it does not compile
as part of the build. The workspace manifest declares the dependencies the agent will
need — `tungstenite` and `rustls` for the uplink, `ureq` for the REST calls, `audiopus`
for the encoder — ahead of the code that uses them.

**`client/installer`** — the directory exists and is empty. There is no WiX project and
no signed artefact, so nothing in this repository produces something deployable.

**`server/gateway`** — a working service. It verifies Identity Platform ID tokens
against Google's JWKS, cross-checks the device certificate's tenant against the token's,
enforces the `/v1/me` namespace rule mechanically in middleware, runs every query inside
a transaction carrying the row-level-security context, and serves the WSS ingest
endpoint with cumulative per-channel acks and idempotent `(call_id, channel, seq)`
writes. Two gaps: the certificate authority is an interface with no production
implementation, so a real gateway answers `503 no_ca` to every enrollment (only the
integration test supplies a development CA); and object storage has filesystem and
in-memory backends but no S3 adapter, which is why `main.go` refuses to start without
`SENTINEL_BLOB_DIR`.

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
inside the adapter. `retention.py` and `coverage.py` implement the nightly purge and the
CDR reconciliation arithmetic, both against `Protocol` interfaces with no concrete
database implementation behind them yet, and neither is covered by a test.

**`web/`** — all three workspaces now exist and build. `web/shared/` holds the API
client, the role capability map, money formatting in paise, and presentational components
including the non-dismissible recording indicator; `web/widget/` and `web/portal/` are
vite applications with their own vitest suites. The one outstanding problem is the
out-of-sync lockfile described above.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — the system shape and the design
  decisions that get questioned most often.
- [`docs/deployment.md`](docs/deployment.md) — customer-facing: the Windows support
  matrix, what tier B means in practice, headset pinning, WebView2, and the EDR
  conversation.
- [`docs/security.md`](docs/security.md) — the compliance and security requirements
  mapped to where they are implemented, with file paths, and what is outstanding.
- [`docs/phase-0-checklist.md`](docs/phase-0-checklist.md) — the discovery checklist and
  its go/no-go criterion.
- [`docs/open-decisions.md`](docs/open-decisions.md) — OPEN-1 to OPEN-8, what each
  blocks, and what the build has assumed in the meantime.
