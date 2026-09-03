# MagickSentinel

Call monitoring for Indian debt-collections floors: a Windows desktop agent captures call
audio, ships it over a binary WSS protocol to a Go gateway, and a Python pipeline produces
transcripts, analysis and RBI fair-practices compliance findings for a React portal.

**Compliance correctness wins over every other concern.** When a trade-off appears, pick the
option that keeps a compliance finding accurate and auditable.

Read [README.md](README.md) first — it holds the per-component state-of-play (what is real,
what is written but has never been executed) and the full rationale behind each test suite.
This file is only the rules that are not discoverable from the code.

## `contracts/` is the source of truth

An API or wire-format change **starts** in `contracts/` and then propagates outward. Never
change a shape in one implementation only.

| Artefact | Consumers that must be updated together |
|---|---|
| [contracts/openapi.yaml](contracts/openapi.yaml) | `server/gateway/internal/api`, `web/shared` API client |
| [contracts/wire.md](contracts/wire.md) | `client/sentinel-core/src/protocol.rs` **and** `server/gateway/internal/wire` |
| [contracts/schemas/*.json](contracts/schemas/) | `server/pipeline` analyzer, judge, rule engine |

[contracts/fixtures/wire_vectors.json](contracts/fixtures/wire_vectors.json) is read by both
the Rust and Go codec tests, so the two cannot drift silently. Changing the wire format means
changing the fixture and both codecs in one commit. Nothing checks OpenAPI or the JSON Schemas
against the implementations — that agreement is maintained by hand, so verify it deliberately.

## Build and test

Run the suite for the component you touched; there is no single root command.

```bash
cd client && cargo test                                   # platform-neutral Rust
cd client && cargo check --all-targets --target x86_64-pc-windows-gnu   # Windows-only code
cd server/gateway && go test ./...                        # skips DB-backed tests
bash db/test/gateway_it.sh                                # gateway + throwaway Postgres
bash db/test/rls_test.sh                                  # RLS acceptance — after ANY db/migrations/ change
cd server/pipeline && python -m pytest                    # needs `pip install -e '.[dev]'`
cd server/pipeline && bash tests/pg_integration.sh        # pipeline + Postgres; needs the psycopg extra
cd web && npm run typecheck && npm test && npm run build
```

Gotchas that will otherwise cost you a cycle:

- **The Windows cross-check is mandatory after touching `client/**/src/windows/`,
  `src/devicekey/cng.rs` or `src/spoolkey.rs`.** That code is `#[cfg(windows)]`-gated,
  compiled out on Linux, and has no tests — there are zero tests under
  `client/**/src/windows/`. `cargo check` for the `x86_64-pc-windows-gnu` target is the
  only thing catching breakage that has actually run. It needs mingw-w64 because
  `rusqlite` and `audiopus` build C sources. There is a `windows-latest` job in
  `.github/workflows/ci.yml` that would do more, but it has never run; do not treat it as
  cover.
- **`npm ci` works — use it.** The lockfile that predated the `shared` and `portal`
  workspaces has been regenerated and committed, and the CI web job now runs `npm ci` and
  gates on it instead of skipping itself. `npm install` still works; `npm ci` is what CI
  does and what catches a manifest the lockfile does not match.
- **The widget's vite build must stay single-file.** `client/installer/Sentinel.wxs`
  packages exactly one `widget.html`. `vite-plugin-singlefile` plus the inlining settings
  in `web/widget/vite.config.ts` are what make that honest; removing them produces an MSI
  that installs, reports healthy and renders a blank widget.
- **Do not run `cargo fmt` across the tree.** It is not rustfmt-clean, the check is advisory in
  CI on purpose, and a repo-wide reformat would bury real changes. Format only what you edit.
- Python: provider SDKs are optional extras. The test suite needs only `pytest` and
  `jsonschema` — do not add a hard dependency to make a test pass.
- The local stack lives in `deploy/`, not at the repository root:
  `bash deploy/gen-dev-secrets.sh`, then
  `docker compose -f deploy/compose.yaml up -d postgres` and
  `docker compose -f deploy/compose.yaml run --rm migrate`. The image is
  `pgvector/pgvector:pg16`; the schema declares `vector(1024)` columns. Every credential
  is `${VAR:?…}` with no default on purpose — see `deploy/README.md`. Note that none of
  this has been stood up: the compose file and the images are written, not verified.

## Invariants

These encode security, money or compliance guarantees. Breaking one hides a bug that matters.

**Tenant isolation lives in the database, not the application.** Every gateway query runs
inside `Store.AsIdentity` (or `AsSystem` for ingest) in
[server/gateway/internal/store/store.go](server/gateway/internal/store/store.go), which issues
transaction-scoped `set_config('sentinel.tenant_id', …, true)` so context cannot leak to the
next borrower of a pooled connection. The gateway connects as `sentinel_app`, which is
`NOBYPASSRLS`. Never add a query path that skips this wrapper, and never "fix" a missing-row
bug by loosening a policy — the missing-context case must return zero rows, not all of them.

The pipeline is under the same rule and enforces it from its own side: it connects as
`sentinel_pipeline` and [server/pipeline/sentinel_pipeline/db.py](server/pipeline/sentinel_pipeline/db.py)
calls `assert_rls_enforced` at boot, refusing to start if the connected role can bypass
RLS. A DSN pointed at the schema owner would otherwise see every tenant while every
application-level filter still passed and no test noticed. When a nightly job genuinely
needs to cross tenants — enumerating which tenants exist — the answer is a narrow
`SECURITY DEFINER` function (`sentinel_pipeline_tenants()`, `db/migrations/0008`), not a
loosened policy. There are exactly four such functions; keep it greppable.

**The finalize message is published from a transactional outbox, not from the handler.**
[server/gateway/internal/outbox](server/gateway/internal/outbox) drains
`call_finalize_outbox` (`db/migrations/0007`) into `sentinel.call.finalize`. Do not
"simplify" this into a publish at the end of the finalize handler: a publish that fails
after the database commit gives you a call that is captured, stored, billed and never
analysed, with no error anywhere and nothing that would notice. Delivery is at-least-once
and the consumer is built for it.

**Foreign-marked segments are stored and must never be transcribed.** On tier B the
loopback stream carries whatever the agent's speakers play. `media_segments.foreign_audio`
is checked twice in [server/pipeline/sentinel_pipeline/segments.py](server/pipeline/sentinel_pipeline/segments.py)
— once in SQL, once in Python — so that a future edit to either alone cannot turn the
filter off. Transcribing that audio does not degrade quality; it files RBI conduct
findings against an agent for words nobody said.

**Money is an integer number of paise, end to end.** `*_paise` int64 on the wire; rupees exist
only at the point of display. See [web/shared/src/money.ts](web/shared/src/money.ts). A rupee
value must never become a JavaScript float.

**Go owns the role/capability matrix; TypeScript mirrors it.** [web/shared/src/auth/roles.ts](web/shared/src/auth/roles.ts)
is a UI convenience copy of the Go matrix under `server/gateway/internal/auth`. Change Go
first. The client-side check is never the security boundary.

**The recording indicator has no dismiss control.** [web/shared/src/components/RecordingIndicator.tsx](web/shared/src/components/RecordingIndicator.tsx)
deliberately exposes no `onClose`. It disappears only when capture stops. Do not add one.

**Pipeline degradation order is deliberate.** In
[server/pipeline/sentinel_pipeline/worker.py](server/pipeline/sentinel_pipeline/worker.py):
ASR failure stops the call; **analysis failure must not stop compliance** (tier-1 rules run off
the transcript); judge failure leaves tier-1 findings standing unreviewed. Do not collapse these
into one try/except.

**A transcriber that cannot read the floor's language must not start.** The default
batch ASR provider is `gemini-3.5-transcribe`, chosen in
[server/pipeline/sentinel_pipeline/providers/registry.py](server/pipeline/sentinel_pipeline/providers/registry.py),
and it has **no Tamil at all**. A Tamil floor pointed at it would not fail — it would
transcribe Tamil audio as something else and hand a bank a clean-looking transcript
with no flags on it. So `registry.validate` raises on a configured language the chosen
provider does not support, and `build_batch_asr` never falls back to a different
provider silently. Do not soften either into a warning, and when you add an adapter,
declare its coverage in `CAPABILITIES` in the same commit.

**No PII in logs.** Structured logging only, on every tier.

## Language conventions

**Rust** (`client/`, edition 2021, rust-version 1.78) — deps are centralised in
`[workspace.dependencies]`; crates use `foo.workspace = true`. Windows code is gated with
`#[cfg(windows)]`, never `#[cfg(target_os = "windows")]`, with FFI deps under
`[target.'cfg(windows)'.dependencies]`. Libraries define `thiserror` enums plus a
`pub type Result<T>` alias; `anyhow::Result` appears only at binary entry points. Tests are
inline `#[cfg(test)] mod tests`, except cross-language fixture tests in `tests/`, which locate
the fixture via `concat!(env!("CARGO_MANIFEST_DIR"), "/../../contracts/…")`. **The client is
synchronous and blocking by design** — `tungstenite` and `ureq`, one single-threaded event loop
in `sentinel-agent/src/agent.rs`, all dependencies injected as traits. Do not introduce tokio.
`unsafe` is confined to Windows interop.

**Go** (`server/gateway`, Go 1.25, `pgx/v5`, `coder/websocket`) — errors go out as the
`httpx.Error` envelope (`code`, `message`, `request_id`); use `httpx.WriteError`, never
`http.Error`. Middleware order is request ID → recover → log → authenticate → authorise.
Integration tests `t.Skip` when `SENTINEL_TEST_DATABASE_URL` / `SENTINEL_TEST_ADMIN_DATABASE_URL`
are unset — keep that pattern so `go test ./...` stays runnable without a database.

**Python** (`server/pipeline`, ≥3.11, ruff line-length 100) — collaborators are `typing.Protocol`
interfaces injected into dataclasses, so tests run against fakes. Provider SDKs are imported
**inside** `__post_init__` with `# noqa: PLC0415` so a Sarvam-only deployment need not install
Anthropic or OpenAI. Schemas are resolved by path relative to the module and validated with
`Draft202012Validator`.

**TypeScript** (`web/`, npm workspaces `shared`/`widget`/`portal`, Node ≥22) — full strict mode
including `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes` and `verbatimModuleSyntax`.
Import shared code as `@sentinel/shared`, aliased to source (not build output) in
`web/vitest.config.ts`.

## Further reading

- [docs/architecture.md](docs/architecture.md) — system shape and the decisions most often questioned
- [docs/security.md](docs/security.md) — each compliance requirement mapped to its implementation
- [docs/local-setup-guide.md](docs/local-setup-guide.md) — running the stack locally
- [docs/asr-provider-selection.md](docs/asr-provider-selection.md) — ASR candidate shortlist, per-provider feature gaps and cost at floor scale
- [docs/open-decisions.md](docs/open-decisions.md) — OPEN-1..8; check before assuming a behaviour is settled
- [docs/deployment.md](docs/deployment.md) — Windows support matrix, tier B, headset pinning, EDR
- [deploy/README.md](deploy/README.md) — container images, the compose stack, the migration runner, observability
