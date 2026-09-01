# Security and compliance: requirements and where they live

The specification lists ten security and compliance requirements, several of which gate
a bank's security review. This document maps each one to the code that implements it in
this repository, names the file, and says plainly what is not built yet.

It is written for someone who will check the claims. Every file path below can be opened
and read. Where something is partial, it is described as partial; where nothing exists,
it says so.

A summary table is at the end. Read the sections first — the table cannot carry the
qualifications.

---

## 1. Code signing

**Requirement:** an EV certificate on all binaries and the MSI.

**Status: not implemented, and cannot be until there is something to sign.**

There is no installer in this repository. `client/` is a two-crate Rust workspace —
`sentinel-core` and `sentinel-capture` — with no `sentinel-agent` binary, no
`sentinel-service` binary and no WiX project. There is no signing step in the build and
no CI job that produces a shippable artefact.

Two things to line up before this becomes urgent: obtaining the EV certificate itself,
which involves an identity verification process with a lead time of its own, and
deciding where signing happens, since an EV certificate on a hardware token does not sit
naturally in a cloud CI runner.

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

What is not implemented is the process that would consume this: there is no uplink
client, so nothing currently acks anything in production. The spool's logic is right and
untested against a real server.

## 3. Encryption

**Requirement:** SQLCipher at rest, TLS 1.3 in transit, mTLS for device authentication.

**At rest — partial.** `client/sentinel-core/Cargo.toml` declares a `sqlcipher` feature
that switches `rusqlite` to `bundled-sqlcipher`, and `Spool::from_connection` applies
`PRAGMA key` under that feature. Without the feature — which is how CI on Linux runs —
the database is plain SQLite and the key is ignored, so the spool's logic can be tested
without the SQLCipher toolchain. The comment in the file states that production builds
must enable the feature.

That comment is currently the only thing enforcing it. Nothing in the repository builds
with `--features sqlcipher`, and there is no release build to enforce it in. The key
management the spec describes — a per-machine key generated at enrollment and wrapped
with DPAPI at machine scope, machine scope because the service and the agent run as
different principals — is documented in the module header and not implemented anywhere.

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

**Certificate issuance — not implemented.** `CertificateAuthority` in
`server/gateway/internal/api/enroll.go` is an interface. `main.go` never sets
`Server.CA`, so a production gateway returns `503 no_ca` to every enrollment. Only the
integration test supplies an implementation, and it is a development CA. The enrollment
handler around it is complete: it validates the CSR, consumes the single-use token
atomically before signing so a retry cannot mint a second certificate, and rejects a
capture tier outside A or B on the grounds that a tier C machine reaching enrollment
means the installer was bypassed.

## 4. Visible recording indicator

**Requirement:** always present in the widget while capture is active, non-dismissible.

**Status: the component exists; the widget does not.**

`web/shared/src/components/RecordingIndicator.tsx` renders only when `active` is true
and has deliberately no dismiss control and no `onClose` prop. The comment in the file
states the reasoning: the only way to make it disappear is for capture to stop, and
adding a close button would be a compliance regression rather than a UX improvement. It
also takes a `tierB` flag and labels the indicator differently on tier B, which is what
surfaces the mixed-audio situation to the agent.

The widget shell that would host it does not exist. There is no WebView2 host, no
`window.chrome.webview` bridge, and `web/widget/` contains a `tsconfig.json` and nothing
else. The requirement is therefore met at the component level and unmet at the product
level.

## 5. Data residency

**Requirement:** all storage in an India region; no cross-region replication without
written tenant approval.

**Status: asserted, not enforced.** `contracts/openapi.yaml` names the production server
and annotates it `ap-south-1`. Nothing else in the repository pins a region: there is no
deployment configuration, no infrastructure-as-code, and
`server/gateway/internal/blob/blob.go` provides only a filesystem backend and an
in-memory one. `main.go` refuses to start unless `SENTINEL_BLOB_DIR` is set, with the
message that it is required "until the S3 adapter is configured".

This is OPEN-4. The right time to get the bank client's written confirmation is now,
while the infrastructure does not exist and the cost of the answer is a conversation
rather than a migration.

## 6. Retention

**Requirement:** separate retention periods for audio and transcripts, audio purging much
sooner, implemented as a nightly job with an audit entry per purge batch.

**Status: the configuration exists; the job does not.**

`db/migrations/0001_init.up.sql` gives `tenants` an `audio_retention_days` defaulting to
30 and a `transcript_retention_days` defaulting to 365. The gateway reads both and
returns them in the policy snapshot (`server/gateway/internal/api/handlers.go`).
`blob.SegmentKey` partitions object keys as `audio/{tenant}/{day}/{call}/{channel}/...`
specifically so a retention sweep can delete a day by prefix rather than row by row.

Nothing sweeps. There is no scheduled job in the gateway, none in the pipeline, and no
audit entries of a purge kind. Nothing in this repository deletes anything on a schedule.

The two default numbers are placeholders — OPEN-6 — and should not be quoted to a
customer as the product's retention policy.

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

Exactly three operations legitimately precede a tenant context, because they are how the
tenant is established: consuming an enrollment token, registering the enrolled device,
and resolving a presented client certificate back to a device. Rather than punching holes
in the policies, `db/migrations/0005_bootstrap_functions.up.sql` exposes those three as
narrow `SECURITY DEFINER` functions and grants the application role `EXECUTE` on them and
nothing more. The set of tenant-crossing operations is therefore exactly three, and it is
greppable.

`calls.account_ref` is indexed per tenant, so lookup by account reference is cheap. But
there is no data-subject export endpoint, no deletion path, and no handler for either.
`POST /v1/compliance/exports` appears in `contracts/openapi.yaml` and is not routed by
`server/gateway/internal/api/server.go`.

## 8. Audit log on read

**Requirement:** an audit entry on every read or export of call content, not just writes.

**Status: implemented for single-call reads; there is a gap on listings.**

`Store.GetCall` (`server/gateway/internal/store/queries.go`) writes a `call.read` audit
row inside the same transaction that returns the call, its transcript and its flags — so
the read and its audit entry commit together or not at all. Writes are audited too:
`call.confirm`, `flag.update`, `flag.agent_response`, `device.revoke`, `user.update`,
`rule_set.publish` and `enrollment_token.create` all go through `auditTx` in
`server/gateway/internal/store/store.go`.

**The gap.** `Store.ListCalls` is not audited, and the `callSelect` query it shares with
the team and compliance listings returns `a.summary` and `c.account_ref`. That is call
content. A reviewer who pages through `GET /v1/compliance/flags` or
`GET /v1/teams/{id}/calls` reads borrower call summaries, and the audit log will not show
it. The requirement as written — the product must be able to answer "who listened to this
borrower's call" — is not fully met while a listing can show a summary without leaving a
trace.

Two ways to close it, and they are not equivalent: audit the listing as a query (one row
per request, recording the filter), or remove summary text from list responses and make
the detail endpoint the only route to content. The second is cleaner and changes the UI.

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
partly built — `device_events` and `coverage_daily` exist in the schema, the heartbeat
handler records events and touches the device — and the agent that would send the
heartbeats does not exist yet.

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

What this does not have is a test. Nothing asserts that no log line contains a
transcript, and nothing prevents a future handler from logging one. A grep-based CI check
over log call sites, or a structured logger that only accepts an allowlist of field
names, would turn a convention into a guarantee.

---

## Summary

| # | Requirement | Status | Primary location |
|---|---|---|---|
| 1 | Code signing (EV) | Not started; nothing to sign yet | — |
| 2 | No local retention | Implemented and tested | `client/sentinel-core/src/spool.rs` |
| 3 | SQLCipher at rest | Behind an unused feature flag; key management not built | `client/sentinel-core/Cargo.toml`, `src/spool.rs` |
| 3 | TLS 1.3 | Implemented | `server/gateway/cmd/gateway/main.go` |
| 3 | mTLS device auth | Implemented; certificate issuance is an unimplemented interface | `server/gateway/internal/api/middleware.go`, `internal/api/enroll.go` |
| 4 | Recording indicator | Component built and non-dismissible; no widget to host it | `web/shared/src/components/RecordingIndicator.tsx` |
| 5 | Data residency | Asserted in the contract; no infrastructure to enforce it (OPEN-4) | `contracts/openapi.yaml` |
| 6 | Retention | Configuration present; no purge job anywhere (OPEN-6) | `db/migrations/0001_init.up.sql` |
| 7 | DPDP: tenant isolation | Implemented in the database and tested | `db/migrations/0002_rls.up.sql`, `db/test/rls_test.sh` |
| 7 | DPDP: subject export and deletion | Not built; the documented export route is not implemented | — |
| 8 | Audit on read | Single-call reads audited; listings return summaries unaudited | `server/gateway/internal/store/queries.go` |
| 9 | EDR allowlisting | Process requirement, documented | `docs/deployment.md`, `docs/phase-0-checklist.md` |
| 10 | No PII in logs | Enforced in the gateway's HTTP layer; no test guards it | `server/gateway/internal/httpx/httpx.go` |
