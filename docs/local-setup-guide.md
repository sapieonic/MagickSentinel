# Local setup guide

How to get Sentinel running on a development machine and poke at the interesting parts.
Every command here was run against this repository.

## What you can run locally

| Part | Locally | |
|---|---|---|
| Compliance rules, analysis, cost controls | Yes | Deterministic fake providers, no API keys |
| Database and row-level security | Yes | Throwaway cluster or Docker Postgres |
| Gateway (Go) | Yes | Serves `/healthz`; the REST API needs a real ID token — see below |
| Widget UI | Yes | Ships a browser mock of the native host |
| Portal UI | Builds and serves | Screens need a signed-in session |
| Windows capture agent | No | WASAPI, the service, and enrollment are Windows-only. On Linux/macOS you can compile-check them |

**The one real limit.** The gateway verifies Google Identity Platform ID tokens against
Google's JWKS. There is no development bypass, deliberately — a flag that accepts
unsigned tokens is a flag that can reach production. So a hand-rolled `curl` against
`/v1/...` will get a `401`, and the way to exercise the API end to end is
`db/test/gateway_it.sh`, which mints its own tokens against a test key and drives the
whole surface against a real database.

## Prerequisites

| | Version | Needed for |
|---|---|---|
| Rust | stable | `client/` |
| Go | 1.25+ | `server/gateway/` |
| Python | 3.11+ | `server/pipeline/` |
| Node | 22+ | `web/` |
| PostgreSQL | 16 client + server binaries | database tests, local gateway |

Optional: `gcc-mingw-w64-x86-64` and the `x86_64-pc-windows-gnu` Rust target, to
compile-check the Windows-only client code. Docker, if you would rather not install a
Postgres server.

```sh
git clone https://github.com/sapieonic/MagickSentinel.git
cd MagickSentinel
```

## Run the test suites

This is the fastest way to confirm the checkout is healthy, and the suites are readable
as documentation.

```sh
cd client && cargo test                       # state machine, spool, wire codec, VAD, resampler
cd server/gateway && go test ./...            # codec, ingest session, token verifier
cd server/pipeline && .venv/bin/python -m pytest   # rules, analysis, judge, cost, worker
cd web && npm test                            # widget and portal
bash db/test/rls_test.sh                      # tenant isolation, against a real Postgres
bash db/test/migrations_test.sh               # migrations apply, roll back, and re-apply
bash db/test/gateway_it.sh                    # the gateway's API surface against a real Postgres
```

First-time setup for the two that need it:

```sh
cd server/pipeline && python -m venv .venv && .venv/bin/pip install -e '.[dev]'
cd web && npm ci
```

`npm ci` is the right command now. It used to refuse to run at all, because
`web/package-lock.json` predated the `shared` and `portal` workspaces; the lockfile has
been regenerated and committed, so a clean install works and the CI web job gates on it
rather than skipping itself with a notice. Use `npm install` only when you are
deliberately changing dependencies, and commit the lockfile when you do.

The three `db/test/*.sh` scripts each boot a throwaway PostgreSQL 16 cluster, apply every
migration, and tear it down. If pgvector is missing they install a stub `vector` type so
the schema still applies. Run them as your normal user, or under `sudo` — not as a bare
root shell, since `initdb` refuses to run as root and the script then needs a
`$SUDO_USER` to drop to.

## Play with it

### 1. The compliance engine

The tier-1 rules are the part customers buy, they run on 100% of calls, and they need
no model provider at all. Save this as `server/pipeline/demo_compliance.py`:

```python
from datetime import datetime, timezone

from sentinel_pipeline.compliance.engine import RuleEngine, load_default_rule_set
from sentinel_pipeline.models import CallContext, Channel, ChannelTranscript, Transcript, Word


def spoken(text: str, start_ms: int = 0, wpm: int = 150) -> list[Word]:
    """Lay a line out on a timeline — the time-based rules need real spans."""
    step = int(60_000 / wpm)
    return [
        Word(text=tok, start_ms=start_ms + i * step, end_ms=start_ms + (i + 1) * step)
        for i, tok in enumerate(text.split())
    ]


agent = "listen you thief pay today or we will send the police to your house"
borrower = "please give me one week"

call = Transcript(
    context=CallContext(
        call_id="01JBDEMO0000000000000000",
        tenant_id="demo",
        user_uid="agent-a",
        started_at=datetime(2026, 1, 5, 22, 30, tzinfo=timezone.utc),  # 04:00 IST
        duration_ms=120_000,
        account_ref="LN-1",
    ),
    channels={
        Channel.NEAR: ChannelTranscript(
            channel=Channel.NEAR, text=agent, words=spoken(agent), language="en"
        ),
        Channel.FAR: ChannelTranscript(
            channel=Channel.FAR, text=borrower, words=spoken(borrower, 12_000), language="en"
        ),
    },
)

for f in RuleEngine(load_default_rule_set()).evaluate(call):
    print(f"{f.severity.value:8} {f.rule_id:24} {f.evidence_text!r}")
```

```sh
cd server/pipeline && .venv/bin/python demo_compliance.py
```

```
critical abusive_language         'listen you thief pay today or we will send the police'
critical threat_of_violence       'listen you thief pay today or we will send the police to your house'
high     outside_call_hours       'call placed at 04:00 IST'
medium   missing_identification   'listen you thief pay today or we will send the police to your house'
medium   no_purpose_disclosure    'listen you thief pay today or we will send the police to your house'
```

Things worth trying: move `started_at` into business hours and watch `outside_call_hours`
go; add an identification line to the near channel; set `interruptions=20`. The rules and
their term lists come from `db/migrations/0004_default_rules.up.sql`, which is also what a
new tenant is seeded with — edit there, not in Python.

`sentinel_pipeline/providers/fake.py` has deterministic fake ASR, analysis and judge
providers if you want to drive `worker.py`'s whole finalize sequence.

### 2. The database and tenant isolation

A persistent database you can keep poking at:

```sh
docker run -d --name sentinel-db -p 5432:5432 \
  -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=sentinel pgvector/pgvector:pg16

for f in db/migrations/*.up.sql; do
  PGPASSWORD=postgres psql -h localhost -U postgres -d sentinel -v ON_ERROR_STOP=1 -q -f "$f"
done
```

Seed a couple of tenants:

```sql
INSERT INTO tenants (id, name, idp_tenant_id) VALUES
  ('11111111-1111-1111-1111-111111111111','Acme BPO','acme'),
  ('22222222-2222-2222-2222-222222222222','Rival BPO','rival');
INSERT INTO teams (id, tenant_id, name) VALUES
  ('aaaaaaaa-0000-0000-0000-000000000001','11111111-1111-1111-1111-111111111111','Team North');
INSERT INTO users (firebase_uid, tenant_id, role, team_id, display_name) VALUES
  ('agent-a','11111111-1111-1111-1111-111111111111','agent','aaaaaaaa-0000-0000-0000-000000000001','Agent A'),
  ('agent-b','11111111-1111-1111-1111-111111111111','agent','aaaaaaaa-0000-0000-0000-000000000001','Agent B'),
  ('sup-north','11111111-1111-1111-1111-111111111111','supervisor','aaaaaaaa-0000-0000-0000-000000000001','Sup North');
INSERT INTO devices (id, tenant_id, machine_guid, hw_fingerprint, cert_fingerprint,
                     os_build, capture_tier, agent_version) VALUES
  ('dddddddd-0000-0000-0000-000000000001','11111111-1111-1111-1111-111111111111',
   'mg-1','hw-1','cf-1','10.0.22631','A','1.0.0');
INSERT INTO calls (id, tenant_id, device_id, user_uid, team_id, started_at, capture_tier) VALUES
  ('c0000000-0000-0000-0000-00000000000a','11111111-1111-1111-1111-111111111111',
   'dddddddd-0000-0000-0000-000000000001','agent-a',
   'aaaaaaaa-0000-0000-0000-000000000001', now(), 'A'),
  ('c0000000-0000-0000-0000-00000000000b','11111111-1111-1111-1111-111111111111',
   'dddddddd-0000-0000-0000-000000000001','agent-b',
   'aaaaaaaa-0000-0000-0000-000000000001', now(), 'A');
```

Now become the role the gateway actually connects as, and hand it a caller:

```sql
SET ROLE sentinel_app;                       -- NOBYPASSRLS, so the policies apply
SET sentinel.tenant_id = '11111111-1111-1111-1111-111111111111';
SET sentinel.user_uid  = 'agent-a';
SET sentinel.role      = 'agent';

SELECT user_uid FROM calls;                  -- agent-a
```

Change `sentinel.user_uid` to `sup-north` and `sentinel.role` to `supervisor` and the same
unfiltered query returns both calls. Point `sentinel.tenant_id` at the rival tenant and it
returns nothing. Unset them entirely and it still returns nothing — a query that forgets
its tenant filter leaks zero rows rather than all of them, which is the property the whole
design rests on. The gateway sets these three with `SET LOCAL` per transaction
(`internal/store/store.go`); `SET` without `LOCAL` is just more convenient in psql.

`db/test/rls_test.sh` asserts eighteen of these properties if you would rather read than
type.

### 3. The gateway

```sh
cd server/gateway
SENTINEL_DATABASE_URL='postgres://postgres:postgres@localhost:5432/sentinel' \
SENTINEL_GCP_PROJECT='sentinel-local' \
SENTINEL_BLOB_DIR=/tmp/sentinel-blobs \
  go run ./cmd/gateway
```

```
$ curl -s localhost:8080/healthz
{"status":"ok","version":"dev"}

$ curl -s -H 'Authorization: Bearer junk' localhost:8080/v1/me/calls
{"code":"unauthorized","message":"token rejected","request_id":"..."}
```

`SENTINEL_BLOB_DIR` is required because there is no S3 adapter yet; the filesystem backend
stands in. `SENTINEL_GCP_PROJECT` can be any string until you have a real project — it only
sets the expected issuer and audience.

To see the API actually answer, run `bash db/test/gateway_it.sh`. It boots a database,
mints tokens for six roles against a test key, and asserts that one request produces six
correctly different answers. `internal/api/integration_test.go` is the best available
description of what each endpoint returns.

### 4. The widget

```sh
cd web && npm run dev:widget      # http://localhost:5173
```

With no WebView2 host present the widget falls back to a browser mock and exposes it on
the console. Click **Sign in** — the mock accepts it — then drive the states from there:

```js
__sentinelMock.simulateCall()          // ARMED → IN_CALL → WRAP → confirmation prompt
__sentinelMock.setTier('B')            // tier badge and recording indicator for degraded capture
__sentinelMock.simulateError({ cause: 'headset_missing' })  // or offline_past_grace, device_revoked
__sentinelMock.clearError()
```

The mock returns `null` from `getToken`, so the history tab shows an honest
"no credentials" error rather than pretending. Everything driven by native state — the
non-dismissible recording indicator, the capture states, the post-call confirmation —
works.

### 5. The portal

```sh
cd web && npm run dev:portal      # http://localhost:5173
VITE_API_BASE_URL=http://localhost:8080 npm run dev:portal   # to point at a local gateway
```

Both apps default to port 5173, so run one at a time or let vite pick the next free port.

The portal boots, then asks the gateway for a session and shows "Could not start a session"
without one. The token seam is a single function in `web/portal/src/main.tsx`, reading
`window.__SENTINEL_PORTAL_TOKEN__`; with an Identity Platform project you can set that from
the console and the screens light up. Until then the portal's behaviour is best read from
`web/portal/src/**/*.test.ts` and the role capability map in `web/shared/src/auth/roles.ts`.

## Windows client

Capture, the service, and device enrollment only run on Windows 10 1903+ (see
[`deployment.md`](deployment.md) for the tier matrix). From any platform you can still
type-check them:

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt-get install -y gcc-mingw-w64-x86-64        # rusqlite and audiopus build C for the target
cd client && cargo check --all-targets --target x86_64-pc-windows-gnu
```

This catches signature and feature-flag breakage in code no test exercises. It does not
tell you the code works — there is no Windows runner and no hardware-in-the-loop test.

## Troubleshooting

**`initdb: cannot be run as root`** — run the `db/test/*.sh` scripts as a normal user, or
under `sudo` from one. From a bare root shell they need an unprivileged account that can
read the checkout.

**`pgtest: no unprivileged user can read …`** — the account being dropped to cannot
traverse into your checkout. Run under `sudo` from your own user, or set
`PGTEST_USER` to an account that can.

**`pgtest: pgvector missing, installing stub`** — informational. The stub makes embedding
columns behave as opaque text; vector search is the only thing that needs the real
extension.

**`npm ci` fails on the lockfile** — no longer expected; this was a known breakage and
is fixed. If you see it, your checkout is behind or a dependency change landed without
its lockfile.

**`SENTINEL_BLOB_DIR is required`** — the gateway refuses to start without object storage
rather than dropping audio on the floor.

**Everything returns `401`** — expected without an Identity Platform token. Use
`db/test/gateway_it.sh`.
