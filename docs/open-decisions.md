# Open decisions

Eight decisions in the specification are marked OPEN. They are not oversights; they are
questions whose answers depend on the customer, the bank client, or measurement that has
not been done yet.

**An OPEN item must not be invented away.** If one of these is on your critical path,
raise it and block on it. Writing code that quietly assumes an answer converts an open
question into a hidden one, and hidden ones surface during a bank's security review or a
pilot, which are the two worst times to find them.

Where the build has had to proceed anyway, this document records what it assumed, so
that the assumption is visible and reversible rather than buried in a commit. An
assumption recorded here is still not a decision.

---

## OPEN-1 — Native agent language: Rust or C#

**Blocks:** Phase 1 start.

**Status: settled in practice.** The client is built in Rust with `windows-rs`. The
workspace at `client/Cargo.toml` has four members — `sentinel-core`,
`sentinel-capture`, `sentinel-service` and `sentinel-agent` — and produces the
`SentinelService` and `SentinelAgent` binaries. The recommendation was Rust for a single
static binary with no runtime dependency in the MSI and one fewer question in the bank's
security review, and that is what exists.

Reversing this now means rewriting all four crates, including the COM work under
`client/sentinel-capture/src/windows/` and `client/sentinel-service/src/windows/`. Treat
it as decided unless something forces the question open again.

**What would settle it formally:** a note in the architecture record confirming the
choice. There is no outstanding evidence to gather.

---

## OPEN-2 — Does the customer run Entra ID?

**Blocks:** Phase 1. It decides whether agents sign in through corporate SSO or whether
Sentinel owns an email-and-password flow for the whole floor.

**Working assumption:** none in the code. The gateway verifies Google Cloud Identity
Platform ID tokens against Google's JWKS
(`server/gateway/internal/auth/auth.go`) and reads `tenant_id`, `role` and `team_id`
from the verified claims. That works identically whether the upstream is a SAML
federation to Entra ID or a password credential, so the build has not had to guess.

The PKCE implementation at `client/sentinel-agent/src/auth/pkce.rs` is now reachable and
compiled in — an earlier version of this entry said the agent crate's root was a
placeholder that did not build it, and that is no longer true. The desktop runs the flow
in the system browser, and the exchange it dead-ended at before now exists:
`POST /v1/oauth/token` (`server/gateway/internal/api/token.go`, `internal/idp`) brokers
the authorization-code exchange server-side, so the desktop stays a public client per RFC
8252 and neither an OAuth client secret nor an Identity Platform API key ships in an MSI.
The tenant's OIDC endpoints and client id now live in `LocalConfig`
(`client/sentinel-core/src/config.rs`), written by the installer per tenant, with no
default and a named error for each missing value rather than a shared fallback.

None of that decides this question. PKCE against Identity Platform is the same flow
whether the upstream is federated to Entra ID or a password credential, which is exactly
why the build has been able to proceed without an answer. The practical consequences —
provisioning, deprovisioning, MFA behaviour on a shared desktop where three shifts sign in
and out of the same machine — are all still ahead, and the installer property that names
the tenant's authorize endpoint is the place where the answer will eventually land.

**What would settle it:** the identity provider inventory in
`docs/phase-0-checklist.md` — the provider and tenant, whether agents have individual
accounts, and whether MFA is enforced. Get it from whoever administers the directory.

---

## OPEN-3 — May agents replay their own call audio?

**Blocks:** Phase 4, the agent self-view.

**Working assumption:** the decision is deferred to the customer per tenant, and the
mechanism exists. `tenants.allow_agent_audio_playback` defaults to `false`
(`db/migrations/0001_init.up.sql`), the gateway includes it in the policy snapshot
(`server/gateway/internal/api/handlers.go`), and the web role map exposes it as a
policy-gated capability rather than a role-derived one
(`web/shared/src/auth/roles.ts`).

Defaulting to no is deliberate: turning playback on later is a configuration change,
turning it off after agents have had it is a fight.

**What would settle it:** a policy answer from the BPO, ideally with the bank client's
view on it, since borrower audio is the bank's borrower's audio. Ask whether agents are
permitted to hear their own calls for self-review, and whether that changes for calls
that carry a compliance flag.

---

## OPEN-4 — Data residency confirmation

**Blocks:** Phase 1 infrastructure.

**Working assumption:** India only, and the code now defaults to it in three places
rather than asserting it in one. `contracts/openapi.yaml` names
`https://api.sentinel.magickvoice.com` as production and annotates it `ap-south-1`. There
is an S3 backend, `server/gateway/internal/blob/s3.go`, whose `DefaultRegion` is
`ap-south-1`. And `deploy/` creates MinIO's development bucket in the same region, so that
nobody develops against a `us-east-1` default — the region a bucket was created in is the
one fact about an object store that cannot be changed afterwards.

Two deliberate omissions keep the question from being answered by accident.
`SENTINEL_S3_SSE` and `SENTINEL_S3_KMS_KEY_ID` are left unset, because the key that would
encrypt borrower audio at rest has a residency of its own. And there is still no
Terraform, no Helm chart and no Kubernetes manifest: `deploy/README.md` records that
infrastructure-as-code written now would encode a region and a cloud into the repository
before this decision is made, which is precisely the cost this entry exists to avoid.
`deploy/observability/` likewise names no managed telemetry backend, because Grafana
Cloud, Datadog, New Relic and Honeycomb all resolve to a US or EU region if nobody
chooses.

**A default is not a confirmation.** Nothing in the repository has been deployed, and
nobody at the bank has put India-only in writing. The cost of the answer is still a
configuration change rather than a migration, but that window is narrower than when this
was raised: the storage layer now exists and has a region baked into its default.

**What would settle it:** written confirmation from the bank client that all storage
stays in an India region, and that cross-region replication requires their prior written
approval. Written, from the bank, not from the BPO's understanding of the bank's
position.

**Since raised, the ASR default has taken a position on this.** The batch ASR provider
selected in `server/pipeline/sentinel_pipeline/providers/registry.py` is
`gemini-3.5-transcribe`, reached through the global Gemini API endpoint
(`generativelanguage.googleapis.com`). That is *processing*, not storage, but it is
borrower audio leaving India, and it is now the default rather than an option someone
opted into. Google Cloud Speech-to-Text was not the escape hatch it looks like:
`asia-south1` supports exactly one language/model combination, `en-US` with
`telephony_short`, so Cloud STT cannot transcribe Indian-language calls in India
either. If the bank's answer is India-only in the strict sense, the exits are
`SENTINEL_ASR_PROVIDER=sarvam` (India-hosted, at the cost of per-word evidence spans)
or `SENTINEL_ASR_PROVIDER=whisper` (self-hosted, at the cost of running it ourselves).
Both are configuration rather than code, which is why the registry exists — but the
question is now more urgent than when it was raised, not less.
See `docs/asr-provider-selection.md`.

---

## OPEN-5 — Air-gapped WebView2 install path

**Blocks:** the Phase 1 MSI.

**Working assumption: Evergreen only, marked in the WiX source as not decided.**
`client/installer/` is no longer empty — it holds a complete WiX v4 package — so this
entry's old text, that there is no WiX project and nothing has been chosen between, is
out of date. What `Sentinel.wxs` actually does is install the Evergreen bootstrapper and
nothing else, with a comment at the component saying in as many words that OPEN-5 is **not
decided** and that the air-gapped case is unresolved. It searches HKLM for an existing
per-machine runtime first, and if the bootstrapper fails the install is allowed to
continue rather than failing the whole package, because leaving a floor with capture and
no widget is a better failure than leaving it with neither — the missing runtime is
reported in the heartbeat instead.

The release workflow fetches that bootstrapper rather than committing it
(`.github/scripts/fetch-webview2.ps1`), verifies it against a SHA-256 pinned outside the
script in a repository variable, checks the Authenticode signature names Microsoft, and
fails closed on any of those. That is a supply-chain control, not an answer to this
question.

The question is unchanged: whether the Evergreen bootstrapper, which fetches the runtime
from Microsoft at install time, is acceptable, or whether a fixed-version runtime has to
ship inside the MSI for floors with no outbound access at install time. Fixed-version
costs package size and takes on the responsibility for updating the runtime; Evergreen
costs an internet dependency at the worst possible moment. Nothing has been installed
anywhere — no MSI has been built — so neither branch has been tried. The `.wxs` comment
records both exits, so that whichever way this is settled it is an edit in one place
rather than an archaeology exercise.

**What would settle it:** the network egress answer from Phase 0 — specifically whether
desktops can reach Microsoft's WebView2 distribution endpoints during installation, and
whether the customer's deployment tooling permits a package of the size a fixed-version
runtime implies.

---

## OPEN-6 — Retention periods for audio and transcripts

**Blocks:** Phase 3.

**Working assumption: the schema defaults are placeholders and are documented as such.**
`tenants.audio_retention_days` defaults to 30 and
`tenants.transcript_retention_days` to 365 (`db/migrations/0001_init.up.sql`). The
gateway returns both in the policy snapshot, and `blob.SegmentKey` partitions object
keys by day specifically so that a retention sweep can delete a day's audio by prefix
rather than row by row (`server/gateway/internal/blob/blob.go`).

The purge job at `server/pipeline/sentinel_pipeline/retention.py` reads the two periods
per tenant rather than hard-coding them, which is the right shape for a value that is
still open, and it is no longer the sketch this entry used to describe. The `Protocol`
interfaces now have concrete implementations (`persistence.py`, `blobstore.py`) against
Postgres and object storage; there is an entry point, `sentinel-pipeline retention`, and a
one-shot container for it in `deploy/compose.yaml`; and it is covered by
`tests/test_retention_jobs.py`, which for a job whose failure mode is deleting the wrong
data was the qualification that mattered most.

Two properties keep the still-open answer from being pre-empted. **It defaults to a dry
run** — `RetentionJob(dry_run=True)`, and the CLI requires an explicit `--commit` — and a
dry-run pass writes an audit entry marked as such, so it cannot be mistaken for a purge
that happened. **And nothing schedules it:** no cron, no timer, no orchestrator, and the
compose stack it lives in has never been stood up.

Do not read the two defaults as a decision. They have never been checked against a real
requirement. What has changed is only that the code which would enforce them is now
tested; it still has not deleted a production row or object, and there is deliberately no
object-lock or WORM setting on the audio bucket, because object lock is the one storage
setting that cannot be undone and turning it on now would make a placeholder permanent.

**What would settle it:** the bank client's retention requirement and the BPO's, which
are often different, plus whatever the applicable RBI guidance requires for recovery-call
records. The two periods must be settled separately — audio should purge much sooner than
transcripts, and a single number for both is a sign the question was not really asked.

---

## OPEN-7 — Dialer CDR export format and delivery

**Blocks:** Phase 4, coverage reconciliation.

**Working assumption:** no format is assumed, and that is deliberate.
`coverage_daily` exists in the schema with columns for dialer calls, captured calls,
dialer minutes, captured minutes and a gap reason
(`db/migrations/0001_init.up.sql`). `server/pipeline/sentinel_pipeline/coverage.py`
implements the reconciliation arithmetic behind a `CdrSource` protocol, with the
docstring recording that the format and delivery differ per bank and that only the
arithmetic belongs in that module. It now has tests (`tests/test_coverage.py`) and an
entry point (`sentinel-pipeline coverage`).

`CdrSource` also has one implementation now — but read what it is before reading it as
progress. `sentinel_pipeline/cdr.py` is an adapter *registry* keyed by
`SENTINEL_CDR_ADAPTER`, with a CSV reader as the reference entry: configurable column
names, a configurable path template, and an error naming this decision when an unknown
adapter is asked for. It is written against no customer's file. A registry with one
reference adapter is the shape you build when you expect the real format to be different,
which is the same thing as saying the question is open. Nothing populates `coverage_daily`
from real data, because no dialer export has been supplied.

This one is worth pressing on early despite blocking a later phase. Coverage percentage
against the dialer's own record of calls is the metric that turns tamper detection from
an arms race into a management conversation, and it is also how you demonstrate the "100%
of calls monitored" claim that justifies the purchase. Without the CDR export there is
nothing to reconcile against, and the number is unprovable.

**What would settle it:** a sample export file from the customer's dialer, the schedule
and mechanism by which it can be delivered, and confirmation of which fields identify the
agent and the call time — since reconciliation matches on `(agent_id, started_at)` when
the account reference is absent.

---

## OPEN-8 — Softphone process names and UIA selectors

**Blocks:** Phase 2.

**Working assumption: it is tenant configuration, and the shape of that configuration is
built.** `SoftphoneConfig` in `client/sentinel-core/src/config.rs` carries
`process_names` as an ordered preference list and `uia_account_ref_selector` as an
optional string, delivered by `GET /v1/policy`. The comment on that field records that
it is OPEN-8. When the selector is absent the account reference is left null and the
server reconciles against the dialer CDR instead — which is also OPEN-7. A reference CSV
adapter now exists there, but no customer's export format does, so the fallback is still
unavailable in practice.

What is missing is one worked example: a real softphone, on a real desktop, with a
verified process name and a selector that actually reads the account reference out of
the dialer window. Nothing in the recent work moves this. The COM capture code that would
resolve a process name to an audio session still has zero tests, the `windows-latest` CI
job that would at least compile and lint it has never run, and no build of this software
has been installed on a machine with a softphone on it. This remains a configuration
shape with no observation behind it.

**What would settle it:** the softphone identification item in
`docs/phase-0-checklist.md`, confirmed by watching a live call rather than by reading
vendor documentation. Note the child-process case specifically — if the audio lives in a
process other than the one with the recognisable name, the process name list is wrong in
a way that only shows up when capture silently never arms.

---

## Summary

| ID | Decision | Blocks | Working assumption in this repository |
|---|---|---|---|
| OPEN-1 | Rust or C# for the native agent | Phase 1 start | Settled: Rust, in `client/` |
| OPEN-2 | Does the customer run Entra ID | Phase 1 | None; token verification is provider-agnostic, and PKCE plus the gateway's `/v1/oauth/token` broker work either way |
| OPEN-3 | Agent replay of own call audio | Phase 4 | Per-tenant flag, defaults to off |
| OPEN-4 | Data residency | Phase 1 infra | India only: `ap-south-1` in the OpenAPI server list and as the S3 default, deliberately no IaC — **still unconfirmed in writing, and the ASR default sends audio to a global Google endpoint** |
| OPEN-5 | Air-gapped WebView2 path | Phase 1 MSI | Evergreen bootstrapper only, marked NOT DECIDED in `Sentinel.wxs`; no MSI has been built |
| OPEN-6 | Audio and transcript retention | Phase 3 | Schema defaults of 30 and 365 days, explicitly placeholders; purge job implemented and tested, dry run by default, unscheduled, has never deleted anything |
| OPEN-7 | Dialer CDR export | Phase 4 | No customer format assumed; reconciliation arithmetic tested, behind a `CdrSource` registry whose one entry is a reference CSV reader |
| OPEN-8 | Softphone names and UIA selectors | Phase 2 | Tenant config shape built; no worked example, and the capture code has never run |
