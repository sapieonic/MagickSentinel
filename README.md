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
contracts/     OpenAPI, the WSS binary protocol, JSON Schemas for AI output
db/            SQL migrations and the row-level-security acceptance tests
client/        Rust workspace: sentinel-core, sentinel-capture
server/gateway Go: REST API and WSS ingest
server/pipeline Python: compliance rules, analysis, ASR interfaces, cost controls
web/           React: shared components, widget and portal entry points
docs/          Deployment, security, architecture, Phase 0 checklist, open decisions
```

`contracts/` is the source of truth. Client, gateway and web are all expected to
generate or hand-write types against it, and a change to an API shape starts there. The
wire protocol has a shared fixture, `contracts/fixtures/wire_vectors.json`, that both
the Rust codec test and the Go codec test read, so the two implementations cannot drift
without a test failing.

## Running the test suites

Every command below was run against this working tree and passes.

```
cd client && cargo test
```

Unit tests for the call state machine, the spool, the wire codec, the VAD, the
resampler, device matching, tier classification and foreign-audio suppression, plus an
integration test against the shared wire fixture. No audio hardware, no network, no
database. This is the suite to run while working on the client.

```
cd client && cargo check --target x86_64-pc-windows-gnu
```

Type-checks the Windows-only capture code — the COM work under
`client/sentinel-capture/src/windows/` — which is compiled out on Linux. The target and
mingw-w64 are installed in the dev container. This catches signature and feature-flag
breakage in code that has no test coverage; it does not tell you the code works.

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

The rule-engine and pipeline tests. They need `pytest`; the package's declared runtime
dependencies are not imported by anything under test today, so
`pip install -e '.[dev]'` works but a bare `pip install pytest` is enough. A prepared
virtualenv exists at `.venv-dev/` in this container:
`.venv-dev/bin/python -m pytest` from `server/pipeline` works without further setup.

### Not yet runnable

`web/` is under active construction in a separate work stream. `npm ci` currently fails
because `package-lock.json` predates the `@sentinel/shared` workspace, `web/widget/` and
`web/portal/` contain a `tsconfig.json` and nothing else, and `npm run typecheck` reports
an error in `web/shared/src/money.ts`. The CI workflow has a web job that skips with a
notice until those three things are fixed; see the comment in
`.github/workflows/ci.yml`.

`cargo fmt --check` currently reports diffs across most of the client tree, and
`cargo clippy` reports two warnings. Both run in CI as advisory steps rather than gates,
for the same reason: the client is owned by another work stream. Turning them into gates
is a one-line change once the tree is clean.

## What state each component is in

**`contracts/`** — usable. `openapi.yaml` is a valid OpenAPI 3.1 document describing 25
paths. `wire.md` specifies version 1 of the binary ingest protocol and is implemented on
both sides. `schemas/` holds three JSON Schema 2020-12 documents: `analysis.json`,
`judge.json` and `rule_set.json`. Two documented endpoints, `GET /v1/teams/{id}/live`
and `POST /v1/compliance/exports`, are not yet routed by the gateway.

**`db/`** — the furthest along. Five migrations build the full schema, enable row-level
security on every tenant-scoped table, create the `sentinel_app` and `sentinel_pipeline`
roles, install a default rule set, and add three narrow `SECURITY DEFINER` functions for
the only three operations that legitimately precede a tenant context (consuming an
enrollment token, registering the enrolled device, resolving a client certificate to a
device). The schema goes beyond the spec's tables where the implementation needed it:
`teams`, `enrollment_tokens`, `ingest_watermarks`, `prompt_templates`, `device_events`
and `default_rule_set`.

**`client/sentinel-core`** — the platform-neutral half, and it is real: the call state
machine including hold-resume and mid-call sign-out, the SQLCipher-backed spool with
ack-gated deletion and reported eviction, the wire codec, the policy types, and the
uplink's retry policy. All of it is unit-tested.

**`client/sentinel-capture`** — split. The testable parts (the `CaptureSource` trait, the
WAV replay source, VAD, the stateful resampler, container-ID device matching, tier
classification, foreign-audio suppression) are implemented and tested. The Windows COM
implementations of tier A process loopback, tier B endpoint loopback, softphone session
tracking, device-change notification and OS build detection are written and type-check
for the Windows target, but nothing exercises them — there is no Windows CI runner and
no hardware-in-the-loop test. Treat them as unverified.

**Not present in `client/`** — there is no `sentinel-agent` binary, no `sentinel-service`
binary, no WiX installer project, no named-pipe IPC, no PKCE login, no WebView2 widget
shell, no Opus encoder (the framing that carries Opus packets exists; the encoder does
not; `audiopus` is declared in the workspace manifest but unused), and no WebSocket
uplink client. The workspace is two libraries.

**`server/gateway`** — a working service. It verifies Identity Platform ID tokens against
Google's JWKS, cross-checks the device certificate's tenant against the token's, enforces
the `/v1/me` namespace rule mechanically in middleware, runs every query inside a
transaction carrying the RLS context, and serves the WSS ingest endpoint with cumulative
per-channel acks and idempotent `(call_id, channel, seq)` writes. Gaps: the certificate
authority is an interface with no production implementation (the integration test
supplies a development CA), object storage has a filesystem and an in-memory backend but
no S3 adapter, and `main.go` refuses to start without `SENTINEL_BLOB_DIR` for that
reason.

**`server/pipeline`** — libraries, not workers. All ten tier-1 rules are implemented and
tested against a fixture corpus, the tier-2 judge validates against
`contracts/schemas/judge.json` and discards an upheld verdict with no evidence span, the
analyzer validates against `contracts/schemas/analysis.json`, the ASR module defines the
provider interfaces and computes WER and numeric-entity error rate separately, and the
cost controls are implemented. There are no real provider adapters — only a deterministic
fake — and no NATS consumer or worker entry point, so nothing runs end to end yet.

**`web/`** — in progress elsewhere. `web/shared/` has the API client, the role
capability map, money formatting in paise, and six presentational components including
the non-dismissible recording indicator. `web/widget/` and `web/portal/` are empty apart
from a `tsconfig.json`.

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
