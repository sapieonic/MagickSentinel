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

A PKCE implementation exists at `client/sentinel-agent/src/auth/pkce.rs`, though at the
time of writing the agent crate's root is still a placeholder and does not compile it in.
Either way, PKCE against Identity Platform is the same flow whether the upstream is
federated or not, so this decision's practical consequences — provisioning,
deprovisioning, MFA behaviour on a shared desktop — are still ahead.

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

**Working assumption:** India only. `contracts/openapi.yaml` names
`https://api.sentinel.magickvoice.com` as production and annotates it `ap-south-1`.
Nothing in the repository pins a region beyond that: there is no deployment
configuration, no Terraform, and the object-store layer has only a filesystem and an
in-memory backend (`server/gateway/internal/blob/blob.go`), so the S3 region has not yet
had to be chosen.

That is the whole point of raising it now. The cost of confirming India-only before the
infrastructure exists is a conversation; after it exists, it is a migration.

**What would settle it:** written confirmation from the bank client that all storage
stays in an India region, and that cross-region replication requires their prior written
approval. Written, from the bank, not from the BPO's understanding of the bank's
position.

---

## OPEN-5 — Air-gapped WebView2 install path

**Blocks:** the Phase 1 MSI.

**Working assumption:** none. `client/installer/` exists as an empty directory — no WiX
project — and there is no widget shell, so neither the Evergreen bootstrapper nor a
fixed-version runtime has been bundled or even chosen between.

The question is whether the Evergreen bootstrapper, which fetches the runtime from
Microsoft at install time, is acceptable, or whether a fixed-version runtime has to ship
inside the MSI for floors with no outbound access at install time. Fixed-version costs
package size and takes on the responsibility for updating the runtime; Evergreen costs
an internet dependency at the worst possible moment.

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

The purge job now exists too, at `server/pipeline/sentinel_pipeline/retention.py`. It
reads the two periods per tenant rather than hard-coding them, which is the right shape
for a value that is still open — but it runs against `Protocol` interfaces that nothing
implements, nothing schedules it, and it has no tests.

Do not read the two defaults as a decision. They have never been checked against a real
requirement, and the code that would enforce them has never deleted anything.

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
arithmetic belongs in that module. Nothing implements `CdrSource`, nothing populates
`coverage_daily`, and the module has no tests.

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
server reconciles against the dialer CDR instead — which is also OPEN-7, so the fallback
is currently unavailable too.

What is missing is one worked example: a real softphone, on a real desktop, with a
verified process name and a selector that actually reads the account reference out of
the dialer window.

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
| OPEN-2 | Does the customer run Entra ID | Phase 1 | None; token verification is provider-agnostic |
| OPEN-3 | Agent replay of own call audio | Phase 4 | Per-tenant flag, defaults to off |
| OPEN-4 | Data residency | Phase 1 infra | India only, asserted in the OpenAPI server list; no infrastructure yet |
| OPEN-5 | Air-gapped WebView2 path | Phase 1 MSI | None; `client/installer/` is empty |
| OPEN-6 | Audio and transcript retention | Phase 3 | Schema defaults of 30 and 365 days, explicitly placeholders; purge job written, unwired and untested |
| OPEN-7 | Dialer CDR export | Phase 4 | No format assumed; reconciliation arithmetic written behind a `CdrSource` nothing implements |
| OPEN-8 | Softphone names and UIA selectors | Phase 2 | Tenant config shape built; no worked example |
