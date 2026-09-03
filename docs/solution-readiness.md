# Solution readiness

What it would take to call Sentinel a finished, shippable solution, and where the
repository actually stands against that.

This document exists because "is it done?" has a different answer depending on who
asks. The architecture is mature and unusually well reasoned; for a long time the thing
that was missing was not design but every connection to the physical world. That gap has
now largely closed, and the honest summary has moved from *architecturally mature and
operationally unshipped* to **assembled but unproven**: the parts exist, they are wired
to each other, and almost none of it has run on a Windows desktop, in a container, or in
front of a customer.

Read `docs/open-decisions.md` alongside this. Several items below are blocked on
questions that are nobody's to answer inside this repository, and inventing an answer to
them is worse than leaving them open.

---

## The four questions this document started from

**Is there a pipeline that produces the `.exe` on changes?** Yes now, and it has never
run. `.github/workflows/ci.yml` gained a `windows-latest` job that builds with the MSVC
toolchain and runs the tests there, and `.github/workflows/release.yml` builds and signs
the MSI on a tag. Both are authored and unexecuted — there is no Windows runner in the
development container, so the first real execution will be the first tag.

**Is there telemetry, in Grafana, over OpenTelemetry?** Yes, end to end in code. All
three services emit OTLP and `deploy/observability/` provisions Collector, Tempo,
Prometheus, Loki and Grafana with a dashboard and alert rules. The collector's redaction
was exercised with a real payload. The stack has not been stood up.

**Is authentication stitched between all the services?** Yes, and this is the largest
change. Every hop that previously dead-ended now terminates: the gateway issues device
certificates, `POST /v1/oauth/token` exists, the desktop holds a non-exportable CNG key
and presents a real client certificate, the portal signs in, and the pipeline connects
under its own row-level-security identity.

**Is there a service map?** Yes, at `illustration/`, published to GitHub Pages. It is a
design map. The live one is a thing the OTel work above will produce once a collector
runs.

---

## Where each component stands

The distinction that matters throughout is between *written*, *tested*, and *run in
anger*. The repository has always been careful about this and the table keeps that habit.

| Area | State | The honest caveat |
|---|---|---|
| `contracts/` | Usable | Nothing checks the contract against the implementation; agreement is maintained by hand |
| `db/` | Strongest part of the repo | Eight migrations, RLS on every tenant-scoped table, 18 acceptance checks |
| `server/gateway` | Runs, and now complete enough to deploy | Never run against the real client, only the shared wire fixture |
| `server/pipeline` | Connected to Postgres, object storage and NATS | Never run against a live broker; Opus decode untested against real libopus |
| `client/sentinel-core` | Real and tested | — |
| `client/sentinel-capture` | Windows COM code type-checks | **Zero tests** under `src/windows/`; no hardware in the loop |
| `client/sentinel-service` | Substantial, unit-tested | CNG and DPAPI paths have never executed |
| `client/sentinel-agent` | Enrollment, mTLS, uplink, widget | Same: the Windows halves are unverified |
| `client/installer` | Full WiX package | No MSI has ever been built or signed |
| `web/` | Three workspaces, real sign-in | No DOM tests; the suite is deliberately pure-logic |
| `.github/`, `deploy/` | Authored | No image built, no stack stood up, no release produced |

---

## What still blocks a pilot

These are ordered by dependency, not by size. Items 1 and 2 are the ones that decide
whether a floor can run at all.

**1. Nothing has executed on Windows.** This is now the single largest risk in the
project, and it has grown rather than shrunk, because more Windows-only code exists than
before: CNG key generation, DPAPI machine-scope unwrapping, the COM capture paths, the
MSI, and the tier gate. All of it compiles for the target. None of it has run. The
Windows CI job will tell us something the first time it runs; hardware-in-the-loop
testing on a real headset will tell us the rest, and there is currently no test at all
under `client/**/src/windows/`.

**2. The client and the gateway have never spoken to each other.** They agree on the
wire fixture, which is why the codecs cannot drift silently, and that is not the same as
a successful mTLS handshake, an enrollment round trip, or a call reaching Postgres. The
first end-to-end run is ahead of us and it is where integration defects live.

**3. The OTLP relay endpoint does not exist.** The desktop posts telemetry to
`{api_base}/v1/telemetry/otlp/v1/logs` so that 200 collections desktops need no second
egress to a telemetry backend — a deliberate choice, since a second egress is a
security-review conversation nobody wants. The gateway does not route that path yet, so
today those batches 404 and are dropped. Endpoint telemetry is exactly the signal that
would reveal a floor silently not capturing, so this is worth more than its size
suggests.

**4. Judge model spend is not persisted.** The schema carries one cost column on
`analyses`, written before the tier-2 judge runs, so per-tenant totals are low by the
judge's share. The full figure reaches the `sentinel.model.spend` metric but not the
budget the kill switch reads. Per-tenant cost control is one of the four arguments in
`docs/architecture.md` for server-side model invocation, so an undercounted budget
undermines a claim the product makes commercially. Fixing it needs a schema decision: a
cost column on `flags`, or a spend ledger.

**5. Secrets, and who holds them.** The release workflow needs signing credentials, and
an EV certificate lives on a hardware token or in a cloud HSM. The deployment needs a
CA key, an Identity Platform API key, NATS credentials and object-store access. None of
that is provisioned, and the CA key in particular is the root of device identity for the
whole fleet.

---

## What blocks a bank, as distinct from a pilot

A pilot can start with these unresolved. A bank's security and compliance review cannot.

**Data residency, OPEN-4.** An S3 adapter now exists and defaults to `ap-south-1`, which
is a default and not a confirmation. The sharper problem is unchanged and is not about
storage: the default ASR provider is `gemini-3.5-transcribe`, reached through a global
Google endpoint, so borrower audio leaves India by default rather than by opt-in. The
exits are configuration (`SENTINEL_ASR_PROVIDER=sarvam` or `whisper`), which is why the
provider registry exists — but the question needs a written answer from the bank, not a
config flag held in reserve.

**Retention, OPEN-6.** The purge is implemented and tested and defaults to a dry run, so
it is now capable of enforcing a policy. It still enforces placeholder numbers — 30 days
for audio, 365 for transcripts — that have never been checked against a real
requirement, and the two must be settled separately.

**Coverage against the dialer, OPEN-7.** The reconciliation arithmetic and an adapter
registry exist; no bank's CDR format is assumed. Coverage percentage is the number that
substantiates the "100% of calls monitored" claim the purchase rests on, and without a
real export it remains unprovable.

**Endpoint protection.** `docs/deployment.md` is right that this is calendar time, not
engineering time. The agent's behaviour — capturing microphone and system audio,
uploading continuously, running a hidden window, relaunched by a SYSTEM service, writing
an encrypted local database — is feature-for-feature the signature of a keylogger, and
the better the detection product, the more confidently it flags it. Two to six weeks per
vendor is normal.

**Agent audio replay, OPEN-3.** The per-tenant flag exists and defaults to off. The
policy answer does not.

---

## What "done" would actually look like

A definition worth holding the work to, rather than a list of features:

1. A tagged commit produces a signed MSI, automatically, and its tier-C acceptance test
   passes on a real pre-1903 machine.
2. That MSI installs on a floor desktop, enrolls itself, captures a real call on both
   channels, and the call appears in the portal with a transcript and compliance
   findings — without anyone touching the machine afterwards.
3. Grafana shows that call: ingest lag, spool depth, model spend, coverage. A capture
   failure raises an alert before anyone on the floor notices.
4. Five full shifts on ten machines with zero EDR quarantines and no coverage gaps that
   the dialer CDR cannot explain.
5. Every OPEN decision is either answered in writing or explicitly accepted as a risk by
   someone empowered to accept it.

Items 1 through 3 are now engineering with a clear path. Item 4 is calendar time. Item 5
is other people's decisions, which is exactly why `docs/open-decisions.md` insists they
must not be invented away.
