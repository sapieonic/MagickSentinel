# deploy/

Container images, a local/dev stack, a migration runner and an observability stack.

Owned by the deployment work stream. Nothing here edits `db/`, `server/` or `client/` —
the Dockerfiles build *from* those directories with the repository root as the build
context, and the migration runner mounts `db/migrations` read-only.

```
containers/     Dockerfiles for the two server services, plus the gateway's health probe
migrate/        The migration runner: applies db/migrations/*.up.sql in order, once each
nats/           NATS server config: JetStream, and authentication that did not exist before
minio/          One-shot bucket and least-privilege-user bootstrap
observability/  OTel Collector -> Tempo / Prometheus / Loki -> Grafana, provisioned as code
compose.yaml    The local/dev stack
.env.example    Every credential, with no working defaults. See below.
gen-dev-secrets.sh  Writes deploy/.env with random values
```

## Getting it up

```sh
bash deploy/gen-dev-secrets.sh
docker compose -f deploy/compose.yaml up -d
docker compose -f deploy/compose.yaml run --rm migrate
docker compose -f deploy/compose.yaml logs -f gateway pipeline
```

With telemetry:

```sh
docker compose -f deploy/compose.yaml -f deploy/observability/compose.yaml up -d
# Grafana on http://127.0.0.1:3000 ; the dashboard is "Sentinel — capture health"
```

The two scheduled jobs, as one-shot containers of the pipeline image:

```sh
docker compose -f deploy/compose.yaml run --rm retention   # dry run; pass --commit yourself
docker compose -f deploy/compose.yaml run --rm coverage
```

## Why there are no default credentials

Every credential in `compose.yaml` is `${VAR:?message}` — no default, so compose fails
with a named error rather than substituting an empty string. `postgres/postgres` works
everywhere, which is exactly how it reaches a host that is not a laptop. This is a
product whose database holds borrower call records behind row-level security; the
credential that reaches it is not a thing to be convenient about.

`gen-dev-secrets.sh` makes the friction one command. It refuses to overwrite an existing
`.env`, because rotating `POSTGRES_PASSWORD` against a volume that was initialised with
the old one just fails to authenticate, and the only way through is `down -v`, which
discards the database.

## What the local stack can and cannot do

It stands up Postgres with pgvector, NATS with JetStream **and authentication**, MinIO,
the gateway and the pipeline; migrates the database; and runs both services against
MinIO through the gateway's real S3 adapter.

What it cannot do is complete a device enrolment: `main.go` needs `SENTINEL_CA_CERT` and
`SENTINEL_CA_KEY` and there is no development CA committed — only the gateway
integration test supplies one. Without a device certificate, `/v1/policy`,
`/v1/heartbeat` and `/v1/ingest` all refuse. `bash db/test/gateway_it.sh` is the faster
route to seeing the API answer for six different roles.

Nothing in the repository generates load, so the observability dashboards stay empty
until something emits.

## The pieces, and the one thing to know about each

**`migrate/migrate.sh`** — idempotent, records a SHA-256 per applied file in
`deploy_schema_migrations`, and refuses to proceed if a file that has already been
applied has changed since. Picks up new `NNNN_*.up.sql` automatically, and validates the
four-digit prefix, because a file named `7_` sorts after `0010_` and that is how a
migration silently runs out of order. It also detects `db/test/pgtest.sh`'s **stub**
pgvector by probing for the `<->` operator: the stub appears in
`pg_available_extensions` exactly like the real thing, and under it embedding columns
become opaque text, no index is built, similarity search returns nothing, and nothing
raises an error.

`0003_roles.up.sql` creates `sentinel_app` and `sentinel_pipeline` with LOGIN and no
password — correct for a file in git, and useless for a TCP-connecting deployment. The
runner sets both from the environment, passed as a psql variable rather than
interpolated into SQL text, so a password containing a quote cannot change what the
statement does.

**`nats/nats.conf`** — there was no NATS authentication anywhere in this repository
before. Three credentials with three permission sets: the gateway can publish
`sentinel.call.finalize` and cannot subscribe to it, the pipeline can consume and cannot
mint, and the system account is reachable by neither. Draining the queue is the
expensive attack: a finalize message represents a call whose audio the endpoint agent
has already deleted, because the gateway acked it.

One parsing trap, documented in the file: nats-server substitutes `$NAME` in *unquoted*
values and treats `"$JS.API.>"` as a literal. Getting that backwards produces two
different confusing errors.

**`containers/gateway.Dockerfile`** — distroless static, non-root, and the HEALTHCHECK
requires **both** `/healthz` and `/readyz`. Requiring readiness is the deliberate part:
a gateway that is up and cannot reach its object store answers the ingest WebSocket,
takes the audio, fails the blob write, and loses the call — silently, because the
segment goes unacked and sits in the spool until the 72-hour bound evicts it. Distroless
has no shell, so the probe is a small Go binary built in the same builder stage
(`containers/healthprobe/`) rather than a reason to put a shell in a production image.

**`containers/pipeline.Dockerfile`** — the pipeline resolves its JSON Schemas with
`Path(__file__).resolve().parents[4] / "contracts" / "schemas"`, so the package must sit
four directories below a root that also contains `contracts/`. A plain `pip install .`
puts it in site-packages, the image builds, the container starts, and the first call
that reaches the analyser dies on a missing schema. So the source tree is copied to
`/app/server/pipeline` with `/app/contracts` beside it and installed **editable**, and
the build asserts the relationship rather than trusting it.

Its HEALTHCHECK is `sentinel-pipeline check`, which is a *dependency* check — it proves
the database, NATS, the object store and the ASR selection are reachable and correct. It
is not a liveness check for the consume loop, because the consumer exposes no liveness
surface; a wedged consumer passes it. What actually catches a stall is
`SentinelIngestStopped` in the alert rules.

**`observability/`** — see the header of `otel-collector.yaml` for the metric-name
contract the three emitters must produce and the dashboard and alerts read, and for why
the collector redacts rather than trusting every emitter.

## Residency, in one place

OPEN-4 asserts India-only (`ap-south-1`) and is **undecided**; what would settle it is
written confirmation from the bank client. Everything here is arranged so that nothing
answers it by accident:

- `SENTINEL_S3_REGION` defaults to `ap-south-1` and MinIO's bucket is created in it, so
  nobody develops against a `us-east-1` default. The region a bucket was created in is
  the one fact about an object store that cannot be changed later.
- No managed telemetry backend and no default region for one. Grafana Cloud, Datadog,
  New Relic and Honeycomb all resolve to a US or EU region if you do not choose.
- `SENTINEL_S3_SSE` and `SENTINEL_S3_KMS_KEY_ID` are left unset, because the key that
  would encrypt borrower audio at rest has a residency of its own.
- The ASR default reaches Gemini through a global endpoint, so it is borrower audio
  leaving India. That is processing rather than storage, and it is the default rather
  than something someone opted into. The exits — `SENTINEL_ASR_PROVIDER=sarvam` or
  `=whisper` — are configuration, and the image's `PIPELINE_EXTRAS` has to match.

## Not here, deliberately

**No Kubernetes manifests, no Terraform, no Helm chart.** OPEN-4 is unresolved and the
target platform has not been chosen. IaC written now would encode a region and a cloud
into the repository, which is precisely the thing docs/open-decisions.md says costs a
conversation before the infrastructure exists and a migration afterwards. The Dockerfiles
are the portable part and are written to be orchestrator-agnostic: fixed uid 65532, a
read-only-friendly layout, and separate liveness and readiness endpoints on the gateway.

**No Alertmanager routing.** Which alerts wake someone at 3am and which land in a queue
depends on who is on call at the BPO and at MagickVoice. The rules encode the
engineering judgement — which conditions matter, and why — and the routing on top of
them is a deployment decision. `SentinelSpoolEviction` and
`SentinelHealthyButNotCapturing` are the two that should page: both mean audio is being
lost right now, and both are silent from every other vantage point.

**No object-lock / WORM on the audio bucket.** It is the right instinct for a compliance
product and it interacts directly with OPEN-6, whose retention periods are documented
placeholders that "have never been checked against a real requirement". Object lock is
the one storage setting that cannot be undone; turning it on before the period is decided
would make a placeholder permanent.
