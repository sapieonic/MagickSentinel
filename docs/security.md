# Security and compliance: requirements and where they live

The specification lists ten security and compliance requirements, several of which gate
a bank's security review. This document maps each one to the code that implements it in
this repository, names the file, and says plainly what is not built yet.

It is written for someone who will check the claims. Every file path below can be opened
and read. Where something is partial, it is described as partial; where nothing exists,
it says so. Verified against commit `2a2bb02`.

One distinction is load-bearing throughout this document and has become more so: a great
deal of Windows-only and deployment code is now **written and has never been executed**.
Written code that type-checks is not evidence that a control works. Where that is the
situation it is called out in those words rather than being folded into "implemented".

A summary table is at the end. Read the sections first — the table cannot carry the
qualifications. The repository is under active development, so this is a snapshot; the
file paths are stable but the "not built yet" judgements may age faster than the rest.

---

## 1. Code signing

**Requirement:** an EV certificate on all binaries and the MSI.

**Status: the packaging and the signing steps are written; nothing has ever been signed.**

`client/installer/` now holds a complete WiX v4 package — `Sentinel.wxs`,
`Sentinel.wixproj` and `build.ps1` — and `build.ps1` puts the two signing calls in the
order that matters: both binaries are signed *before* the MSI is built, and the MSI is
signed after, because an MSI's signature covers the files inside it and re-signing a
binary afterwards invalidates it. `.github/workflows/release.yml` runs the same order on
a three-part semver tag, refuses a four-part version (the MSI ignores the fourth field
when deciding upgrades, so a four-part version silently breaks fleet upgrades), and
refuses to publish an unsigned MSI as a release.

None of the signing has run. No MSI has been built, no binary has been signed, and no
release has been produced. A reviewer should read the signing chain as authored intent
with the mechanics visible for inspection, not as a control that has been demonstrated.

One adjacent control *is* demonstrated, and it belongs here because it is a
supply-chain property rather than a build convenience:
`.github/scripts/assert-no-stray-dll-deps.ps1` runs on every Windows CI job and fails
the build when the shipping binaries import a DLL the package does not carry. It has
already fired in earnest, on `vcruntime140.dll` — a redistributable absent from a clean
Windows 10 image, which would have produced an MSI that installs onto machines where
the service can never start. The binaries now link the MSVC runtime and OpenSSL
statically, so there is no third-party DLL beside the two EV-signed executables for EDR
to object to or for an attacker to plant.

Two things still to line up: obtaining the EV certificate itself, which involves an
identity verification process with a lead time of its own, and deciding where signing
happens, since an EV certificate on a hardware token does not sit naturally in a cloud CI
runner. The release workflow is written against a signtool that asks CNG for the key, so
where the key physically lives is the KSP's problem rather than the workflow's — but that
arrangement has not been tested against a real token either.

Note also that code signing helps with reputation-based antivirus heuristics and does not
prevent a behavioural detection. See the EDR section below.

## 2. No local retention

**Requirement:** audio exists on disk only in the encrypted spool, and only until it is
acknowledged by the server.

**Status: implemented in `client/sentinel-core/src/spool.rs`.**

The invariant the module is built around is that a segment is deleted only after the
server acks it — never before, never on `call.end`, never on shutdown. `Spool::ack`
takes a cumulative `through_seq` per `(call_id, channel)` and deletes only up to that
watermark; the accompanying tests assert that acks never move backwards, that the two
channels ack independently, and that a duplicate push is idempotent.

The other half of the requirement is that the spool is bounded, and that hitting the
bound is never silent. `enforce_limits` evicts oldest-first at 2 GB or 72 hours,
whichever comes first, and returns a `spool_eviction` event carrying the count of lost
segments. A compliance product that quietly drops audio is worse than one that admits it
did, and the tests cover that path specifically.

The process that consumes this now exists. `sentinel-agent` owns the uplink, and
`client/sentinel-agent/tests/replay_end_to_end.rs` drives the whole client path —
detection, encode, spool, uplink — against a WebSocket server that speaks the real wire
protocol from `contracts/wire.md`, decodes the media records byte for byte, acks
cumulatively per `(call_id, channel)`, and can be told to drop the connection mid-call so
the resume path is exercised rather than described.

That test server is not the Go gateway. The two implementations are held together by the
shared fixture at `contracts/fixtures/wire_vectors.json`, which both codec tests read, so
they cannot drift silently — but the client and the gateway have not been run against each
other, and the ack-then-delete invariant has therefore never been exercised across the
real pair.

## 3. Encryption

**Requirement:** SQLCipher at rest, TLS 1.3 in transit, mTLS for device authentication.

**At rest — partial.** `client/sentinel-core/Cargo.toml` declares a `sqlcipher` feature
that switches `rusqlite` to `bundled-sqlcipher`, and `Spool::from_connection` applies
`PRAGMA key` under that feature. Without the feature — which is how CI on Linux runs —
the database is plain SQLite and the key is ignored, so the spool's logic can be tested
without the SQLCipher toolchain. The comment in the file states that production builds
must enable the feature.

The key management the spec describes is now implemented.
`client/sentinel-service/src/spoolkey.rs` generates the key, wraps it with DPAPI at
**machine** scope — machine rather than user because the service and the agent run as
different principals and a user-scoped wrap would leave one of them unable to open the
file — and `sentinel-agent`'s `spool_key()` unwraps it at startup. There is deliberately
no default, no constant and no `"unconfigured"` string: the shape this replaced,
`env::var(..).unwrap_or_else(|_| "unconfigured".into())`, produced a spool that looked
encrypted while every machine on the floor used the same key, which is worse than no
encryption at all because it survives review. The `SENTINEL_SPOOL_KEY` environment
variable is gone with it, and a failure to unwrap blocks capture rather than degrading to
a known key.

Two builds now ask for the feature. `client/installer/build.ps1` compiles with
`--features sentinel-core/sqlcipher`, and the `rust-windows` job in
`.github/workflows/ci.yml` does the same for its release step specifically so that the
`PRAGMA key` branch and its wrong-key probe are executed rather than merely compiled out.
**Neither has run.** No MSI has been built and the Windows CI job has never executed, so
as of this commit nothing anywhere has ever run the SQLCipher path.

This remains the most checkable gap in this document, and the check has not changed:
anyone reviewing the encryption-at-rest claim should confirm that the *shipped* binary was
built with the feature enabled. What has changed is that the build which would do it now
exists and can be read.

**In transit — implemented.** `server/gateway/cmd/gateway/main.go` sets
`tls.Config{MinVersion: tls.VersionTLS13}`. There is no path that negotiates lower.

**mTLS — implemented, with one deliberate subtlety.** The gateway sets
`ClientAuth: tls.VerifyClientCertIfGiven` rather than `RequireAndVerifyClientCert`. That
is not a weakening: `POST /v1/devices/enroll` is reached by a machine that has no
certificate yet, which is the entire point of the exchange, and the portal is reached by
browsers that have none at all. Routes that need a device sit behind
`api.RequireDevice` (`server/gateway/internal/api/middleware.go`), which checks that a
verified device was actually attached — `/v1/policy`, `/v1/heartbeat` and `/v1/ingest`
all do. A valid user token alone is not enough to read a tenant's capture configuration.

`Authenticate` in the same file also cross-checks the two identities: if the certificate
resolves to one tenant and the token asserts another, the request is refused with
`tenant_mismatch` rather than being resolved in favour of either.

**Certificate issuance — implemented.** `CertificateAuthority` in
`server/gateway/internal/api/enroll.go` is still an interface, but there is now a
production implementation behind it: `server/gateway/internal/ca` loads a PEM intermediate
— certificate, key and optional chain — and signs device certificates against it.
`cmd/gateway/main.go` refuses to start without `SENTINEL_CA_CERT` and `SENTINEL_CA_KEY`
rather than booting into a gateway that answers `503 no_ca` to every enrollment; that
branch survives as a defensive one rather than as the normal case.

Three properties of that package are worth a reviewer's attention, because each is the
answer to a question a bank asks. It **cannot create a CA**, only load one, and it refuses
a certificate not marked as a CA — the root's key belongs in an HSM or an offline safe,
and the thing that has to be online is the intermediate that signs a few hundred device
certificates a year. It refuses a validity window shorter than the renewal lead time plus
a margin (`MinValidity`, cross-checked against `RENEW_WHEN_REMAINING` in
`client/sentinel-service/src/device.rs`), because a certificate shorter than that puts the
fleet into a renewal loop that first shows up as an unexplained enrollment-token burn
rate. And it caps a request at fifteen months, so a unit bug in a caller's `notAfter`
cannot mint a credential that outlives the product.

The device's half of the exchange is stronger than a keypair in a file: the private key is
generated **non-exportably in CNG** (`client/sentinel-service/src/devicekey/cng.rs`) and
the agent signs with it through `NCryptSignHash`, so the key is never on disk and never in
the process's address space. The software-key fallback exists for Linux CI, refuses to
construct itself in a release build without an explicitly typed feature, and the kind of
key that signed a given CSR travels with the credential rather than being assumed. As with
everything else `#[cfg(windows)]` in this repository, the CNG path type-checks and has
never been executed.

The enrollment handler around all of this is complete: it validates the CSR, consumes the
single-use token atomically before signing so a retry cannot mint a second certificate,
and rejects a capture tier outside A or B on the grounds that a tier C machine reaching
enrollment means the installer was bypassed.

## 4. Visible recording indicator

**Requirement:** always present in the widget while capture is active, non-dismissible.

**Status: the component exists, the widget hosts it, and nothing has rendered it on a
real desktop.**

`web/shared/src/components/RecordingIndicator.tsx` renders only when `active` is true
and has deliberately no dismiss control and no `onClose` prop. The comment in the file
states the reasoning: the only way to make it disappear is for capture to stop, and
adding a close button would be a compliance regression rather than a UX improvement. It
also takes a `tierB` flag and labels the indicator differently on tier B, which is what
surfaces the mixed-audio situation to the agent.

The widget shell now exists. `web/widget/` is a full vite application whose `Armed`,
`InCall` and `Wrap` views each render `<RecordingIndicator active …/>`, so the indicator is
present in every state in which capture is running rather than being available for a
future screen to forget. It reaches the native side through
`window.chrome.webview.hostObjects` (`web/widget/src/host/`), and those calls are
time-bounded, because a host that never answers used to leave the widget hanging rather
than degrading it. `client/sentinel-agent/src/widget/webview2.rs` is the host on the other
side of that bridge.

Two things keep this short of demonstrated. The WebView2 host is `#[cfg(windows)]` code
that has never run, and the bundle that ships is produced by a vite configuration
(`web/widget/vite.config.ts`) that has to emit exactly one self-contained file, because
`client/installer/Sentinel.wxs` packages exactly one — `widget.html`, staged from the
build's single `index.html` by `.github/scripts/stage-widget.ps1`, which refuses to stage
a bundle that left anything un-inlined. That configuration is in place and produces a
single file on this tree; it has not yet produced a bundle inside an MSI on a real
machine. The failure it guards against is specifically a compliance one: the install would
succeed, the service would report healthy, and the widget — indicator included — would
render blank.

## 5. Data residency

**Requirement:** all storage in an India region; no cross-region replication without
written tenant approval.

**Status: asserted and defaulted, still not confirmed — and the ASR default now
contradicts it.**

`contracts/openapi.yaml` names the production server and annotates it `ap-south-1`. There
is now an S3 backend, `server/gateway/internal/blob/s3.go`, whose `DefaultRegion` is
`ap-south-1`, and `deploy/`'s MinIO bucket is created in the same region so that nobody
develops against a `us-east-1` default — the region a bucket was created in is the one
fact about an object store that cannot be changed afterwards. `SENTINEL_S3_SSE` and
`SENTINEL_S3_KMS_KEY_ID` are deliberately left unset, because the key that would encrypt
borrower audio at rest has a residency of its own. There is still no
infrastructure-as-code, deliberately: `deploy/README.md` records that writing Terraform
now would encode a region and a cloud into the repository before OPEN-4 is answered.

A sensible default is not enforcement and it is not an answer. Two things a reviewer
should hold onto. Nothing has confirmed India-only in writing from the bank client, which
is what OPEN-4 asks for. And the default batch ASR provider,
`gemini-3.5-transcribe` in `server/pipeline/sentinel_pipeline/providers/registry.py`, is
reached through the global Gemini endpoint — that is processing rather than storage, but
it is borrower audio leaving India, by default rather than by anyone's choice. See
`docs/asr-provider-selection.md` and OPEN-4.

This is still OPEN-4, and the right time to get the written confirmation is still now:
the storage layer exists but nothing is deployed, so the answer is a configuration change
rather than a migration. That window is narrower than it was when this section was
written.

## 6. Retention

**Requirement:** separate retention periods for audio and transcripts, audio purging much
sooner, implemented as a nightly job with an audit entry per purge batch.

**Status: the configuration exists, the job exists and is tested, and it has never
deleted anything outside a test.**

`db/migrations/0001_init.up.sql` gives `tenants` an `audio_retention_days` defaulting to
30 and a `transcript_retention_days` defaulting to 365. The gateway reads both and
returns them in the policy snapshot (`server/gateway/internal/api/handlers.go`).
`blob.SegmentKey` partitions object keys as `audio/{tenant}/{day}/{call}/{channel}/...`
specifically so a retention sweep can delete a day by prefix rather than row by row.

The purge itself is implemented in `server/pipeline/sentinel_pipeline/retention.py`.
`RetentionJob.purge_tenant` reads the two periods per tenant rather than hard-coding
them, deletes each audio object before its database row — so a failed object delete
leaves the row for the next run to retry rather than orphaning audio no sweep can ever
find again — and writes one `retention.purge` audit entry per tenant per run carrying
counts and cutoff dates only, never a call id or an account reference.

All three of the qualifications this section used to carry have moved. The `Protocol`
interfaces (`RetentionStore`, `BlobStore`) now have concrete implementations behind them —
`sentinel_pipeline/persistence.py` and `sentinel_pipeline/blobstore.py`, against Postgres
and object storage, connecting as the `NOBYPASSRLS` `sentinel_pipeline` role. There is an
entry point: `sentinel-pipeline retention`, and `deploy/compose.yaml` defines it as a
one-shot container. And the module is covered by `tests/test_retention_jobs.py`, which
matters more here than anywhere else in the repository, because the failure mode of this
particular job is deleting the wrong data.

Three things a reviewer should still weigh. **The default is a dry run.**
`RetentionJob(dry_run=True)` is the safe default at the entry point and the CLI requires an
explicit `--commit`; a run that did not purge cannot be mistaken for one that did, because
the audit entry records `dry_run`. **Nothing schedules it.** The one-shot container exists;
no cron, timer or orchestrator invokes it, and the compose stack it lives in has not been
stood up. **It has therefore still never deleted a production row or object.** Being
tested is not the same as having run.

The two default numbers are placeholders — OPEN-6 — and should not be quoted to a
customer as the product's retention policy. That is also why there is no object-lock or
WORM setting on the audio bucket: object lock is the one storage setting that cannot be
undone, and turning it on before the period is decided would make a placeholder permanent.

## 7. DPDP alignment

**Requirement:** the BPO is the data fiduciary and MagickVoice is the data processor;
build a per-tenant data-subject export and deletion path by `account_ref`.

**Status: the tenancy model supports it; the export and deletion path is not built.**

The fiduciary/processor split is the reason multi-tenancy is enforced in the database
rather than in application code. Every tenant-scoped table has row-level security
enabled (`db/migrations/0002_rls.up.sql`), the gateway connects as `sentinel_app`, a
`NOBYPASSRLS` role (`db/migrations/0003_roles.up.sql`), and every query runs inside a
transaction that first sets `sentinel.tenant_id`, `sentinel.user_uid` and
`sentinel.role` from the verified token claims (`server/gateway/internal/store/store.go`).
When those settings are absent the RLS predicates evaluate false and queries return
nothing, so a bug that forgets the context leaks zero rows rather than all of them —
which `db/test/rls_test.sh` asserts directly.

Three operations legitimately precede a tenant context, because they are how the tenant is
established: consuming an enrollment token, registering the enrolled device, and resolving
a presented client certificate back to a device. Rather than punching holes in the
policies, `db/migrations/0005_bootstrap_functions.up.sql` exposes those three as narrow
`SECURITY DEFINER` functions and grants the application role `EXECUTE` on them and nothing
more.

There is now a fourth, and it is worth naming rather than letting the count drift.
`db/migrations/0008_pipeline_jobs.up.sql` adds `sentinel_pipeline_tenants()`, granted to
`sentinel_pipeline` alone, because a nightly job has to know which tenants to visit and
under `tenants_self` — `id = sentinel_tenant()` — the query that would produce that list
is itself a query needing a tenant context. It got the same answer as 0005 rather than a
loosened policy: loosening `tenants_self` would let every role that can reach the database
enumerate the customer list, whereas a function returning three columns to one role is
auditable in a way a policy is not. The set of tenant-crossing operations is therefore
exactly four, and it is still greppable. If it grows again without a paragraph like this
one, that is the thing to object to.

`calls.account_ref` is indexed per tenant, so lookup by account reference is cheap, and
`retention.py` defines a `SubjectRequest` dataclass carrying the tenant, the account
reference, whether the request is an export or a deletion, who asked, and when. Its
docstring states the position correctly: MagickVoice acts on the BPO's instruction rather
than on the borrower's directly, and the path has to exist per tenant.

That dataclass is the whole of it. Nothing fulfils a `SubjectRequest`, and there is no
per-tenant subject export and no deletion path by account reference.

`POST /v1/compliance/exports` is a different thing and should not be mistaken for one:
it is the compliance queue's evidence pack, it is routed and audited
(`evidence.export`), it refuses a request containing a flag the caller cannot see so
that the endpoint cannot be used as an oracle for flag ids in other teams, and it gates
audio inclusion on the tenant's playback policy. What it does today is return a job id.
Nothing produces the pack. A DPDP request arriving today would have to be
serviced by hand against the database.

## 8. Audit log on read

**Requirement:** an audit entry on every read or export of call content, not just writes.

**Status: implemented for single-call reads and for call listings; there is a remaining
gap on flag listings.**

`Store.GetCall` (`server/gateway/internal/store/queries.go`) writes a `call.read` audit
row inside the same transaction that returns the call, its transcript and its flags — so
the read and its audit entry commit together or not at all. Writes are audited too:
`call.confirm`, `flag.update`, `flag.agent_response`, `device.revoke`, `user.update`,
`rule_set.publish` and `enrollment_token.create` all go through `auditTx` in
`server/gateway/internal/store/store.go` — as does `evidence.export`, which records the
flag ids and whether audio was requested.

`Store.ListCalls` is audited too, which this document previously said it was not.
`auditList` writes a `call.list` row inside the same transaction, carrying the count, the
call ids (capped at 200, which is a full page) and the shape of the filter. It
deliberately does **not** record the free-text search term: a QA user searching for a
borrower by name would otherwise write that name into a table every admin in the tenant
can read, which is the leak the audit log exists to detect rather than to cause. That
covers `GET /v1/calls`, `GET /v1/me/calls` and `GET /v1/teams/{id}/calls`.

**The gap that remains.** `Store.ListFlags` is not audited, and it returns
`evidence_text` and `judge_rationale` — the transcript span a finding rests on and the
model's reasoning about it. That is call content by any reading. A compliance reviewer
paging `GET /v1/compliance/flags`, or an agent reading `GET /v1/me/flags`, leaves no
trace. The requirement as written — the product must be able to answer "who read this
borrower's call content" — is not fully met while that listing is silent.

Two ways to close it, and they are not equivalent: audit the flag listing the way
`auditList` audits calls (one row per request, recording flag ids and the filter but not
free text), or drop `evidence_text` from the list response and make the detail endpoint
the only route to it. The first is a smaller change and the pattern already exists in the
file next to it.

## 9. EDR allowlisting

**Requirement:** coordinate with the customer's AV vendor before the pilot; budget real
calendar time.

**Status: a process requirement, not a code one.** It is documented for the customer in
`docs/deployment.md` and listed as a Phase 0 item in `docs/phase-0-checklist.md`.

The reasoning belongs here too, because a security reviewer will ask why the product
needs an exclusion. The agent captures microphone and system audio, uploads it
continuously to a remote server, runs a window that is not in the taskbar, is relaunched
by a SYSTEM service when it exits, and writes an encrypted local database the user cannot
read. That is the behavioural signature of a keylogger with audio capture, and a
detection engine flagging it is the engine working correctly rather than failing.

Allowlisting takes weeks of calendar time that no amount of engineering compresses, and
the Phase 2 acceptance gate requires zero quarantines across five full shifts. The
conversation starts in Phase 0.

The related design decision is that the agent does not fight back. Tamper handling is
detect-and-report: heartbeats every 30 seconds carrying capture state and spool depth,
restart counting by the service, a server-side alert on "device online, user signed in,
dialer session active, no capture", and a nightly coverage reconciliation that makes the
gap a supervisor's conversation rather than an arms race. The endpoint side of that is
largely built on the server: `device_events` and `coverage_daily` exist in the schema,
the heartbeat handler records events and touches the device, and
`server/pipeline/sentinel_pipeline/coverage.py` does the reconciliation arithmetic behind
a `CdrSource` interface — now with tests (`tests/test_coverage.py`), an entry point
(`sentinel-pipeline coverage`) and one reference adapter, the CSV reader in
`sentinel_pipeline/cdr.py`. The endpoint half is `client/sentinel-agent/src/heartbeat.rs`,
now driven from the agent's event loop (`src/agent.rs`) rather than sitting unused.

None of that yet produces a coverage figure from real data, and the reasons are
different from each other. The heartbeat code has never run on a Windows desktop. And
the CSV adapter is a *reference* implementation against a format no customer has supplied
— OPEN-7 is open precisely because the export format and its delivery differ per bank, so
having an adapter registry is not the same as having an adapter that reads the customer's
file. Until a real export arrives, the "100% of calls monitored" claim has nothing to be
reconciled against.

The alert rules in `deploy/observability/rules/sentinel.rules.yml` encode the server-side
half of tamper detection, including `SentinelHealthyButNotCapturing`. They have been
written and validated as configuration; no alert has ever fired, because the stack has
never been stood up.

## 10. No PII in logs

**Requirement:** transcripts, account references and borrower names must not appear in
application logs at any level.

**Status: implemented in the gateway, structurally rather than by convention.**

`server/gateway/internal/httpx/httpx.go` states the rule in its package comment and
enforces the part that matters. `LogRequests` logs `r.Pattern` — the matched route
pattern — rather than the raw request path, because a raw path can carry an account
reference in a query string. Each request gets an ID, echoed in the `X-Request-Id`
header and in the error body, so a customer can report a failure by quoting an ID rather
than quoting call content. `Recover` logs the panic value and the request ID and not the
payload.

The heartbeat handler adds a second layer at the data level: the comment on the
device-event loop in `server/gateway/internal/api/handlers.go` records that event detail
is machine state only, because anything resembling call content would be a PII leak into
a table every admin in the tenant can read.

Two things have been added around it. The request log line now also carries `trace_id`
and `span_id` from the OpenTelemetry span
(`server/gateway/internal/httpx/httpx.go`), which are identifiers rather than content and
are the mechanism by which a customer can report a failure without quoting a call. And
`deploy/observability/otel-collector.yaml` redacts at the collector rather than trusting
every emitter to have been careful: an exact-key strip, then pattern-based value
redaction, with the *count* of redacted keys recorded and never the values. The reasoning
is written at the head of that file, and the redaction path was exercised against a real
payload — one of the few things in `deploy/` that was actually executed rather than only
validated as configuration.

What this still does not have is a test in the gateway. Nothing asserts that no log line
contains a transcript, and nothing prevents a future handler from logging one. The
collector's redaction is a second line of defence and should not be read as the first: it
sees only what is exported to it, and a log written to stdout on a host nobody ships from
is outside it entirely. A grep-based CI check over log call sites, or a structured logger
that only accepts an allowlist of field names, would turn the convention into a guarantee
at the point where it matters.

---

## Summary

| # | Requirement | Status | Primary location |
|---|---|---|---|
| 1 | Code signing (EV) | WiX package and signing steps written; **never executed** — no MSI built, nothing signed, no EV certificate | `client/installer/build.ps1`, `.github/workflows/release.yml` |
| 2 | No local retention | Implemented and tested, including against a wire-protocol test gateway; never run against the real gateway | `client/sentinel-core/src/spool.rs`, `client/sentinel-agent/tests/replay_end_to_end.rs` |
| 3 | SQLCipher at rest | Key management implemented (DPAPI, machine scope); the feature is enabled only by builds that have **never run** | `client/sentinel-service/src/spoolkey.rs`, `client/installer/build.ps1` |
| 3 | TLS 1.3 | Implemented | `server/gateway/cmd/gateway/main.go` |
| 3 | mTLS device auth | Implemented; certificate issuance now has a production CA, and the device key is non-exportable in CNG (untested on Windows) | `server/gateway/internal/ca/ca.go`, `client/sentinel-service/src/devicekey/cng.rs` |
| 4 | Recording indicator | Non-dismissible, and rendered by the widget in every capturing state; never rendered on a real desktop | `web/shared/src/components/RecordingIndicator.tsx`, `web/widget/src/views/` |
| 5 | Data residency | S3 backend defaults to `ap-south-1`; still unconfirmed in writing, and the ASR default sends audio to a global endpoint (OPEN-4) | `server/gateway/internal/blob/s3.go`, `server/pipeline/sentinel_pipeline/providers/registry.py` |
| 6 | Retention | Purge job implemented and tested against real storage; dry run by default, unscheduled, has never deleted anything (OPEN-6) | `server/pipeline/sentinel_pipeline/retention.py`, `tests/test_retention_jobs.py` |
| 7 | DPDP: tenant isolation | Implemented in the database and tested; four `SECURITY DEFINER` functions cross tenants, by design | `db/migrations/0002_rls.up.sql`, `db/test/rls_test.sh` |
| 7 | DPDP: subject export and deletion | A request dataclass and nothing that fulfils it | `server/pipeline/sentinel_pipeline/retention.py` |
| 8 | Audit on read | Single-call reads and call listings audited; flag listings return evidence text unaudited | `server/gateway/internal/store/queries.go` |
| 9 | EDR allowlisting | Process requirement, documented | `docs/deployment.md`, `docs/phase-0-checklist.md` |
| 10 | No PII in logs | Enforced in the gateway's HTTP layer and again at the collector; no test guards the gateway | `server/gateway/internal/httpx/httpx.go`, `deploy/observability/otel-collector.yaml` |
