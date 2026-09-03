# illustration/

Animated technical illustrations of how Sentinel works, published as GitHub Pages.

Three files, no build step, no dependencies:

| File | What it is |
|---|---|
| `index.html` | Structure and the static SVG furniture for each diagram. |
| `styles.css` | Tokens and layout. Dark by default with a light theme and a manual toggle. |
| `diagrams.js` | Every animation, hand-written against the DOM. |

Open `index.html` directly, or serve it:

```sh
npx http-server illustration -p 8099 -c-1
```

## The eight diagrams

| # | Section | What it shows |
|---|---|---|
| 01 | Service map | Who calls whom, and why the finalize edge publishes from a transactional outbox rather than from the handler. |
| 02 | Building the `.exe` | `cargo` → sign → `wix` → sign, and why the order is that way. |
| 03 | Two processes | Session 0 vs the interactive session, and the relaunch backoff — click to crash the agent. |
| 04 | Call detection | A live simulation of the state machine against scripted audio energy. |
| 05 | Audio path | WASAPI buffer to a 34-byte record header, with the byte counts. |
| 06 | Ingest protocol | Sequence diagram: `call.start`, media, cumulative ack, a dropped connection, `resume`. |
| 07 | End-to-end | Sequence diagram: boot, enrollment, sign-in, a call, finalize, the pipeline, the portal. |
| 08 | Tenant isolation | The same `SELECT` under six different row-level-security contexts. |

## Keeping it honest

Every threshold, close code, byte count and file path on the page was read out of the
source, not out of a design document. The ones most likely to drift:

| On the page | Source of truth |
|---|---|
| 300 ms / 20 s / 8 s / 3 s detection thresholds | `client/sentinel-core/src/config.rs` → `VadConfig::default()` |
| 1, 2, 4 … 60 s relaunch backoff, 120 s healthy run | `client/sentinel-service/src/supervisor.rs` |
| 34-byte header, 50 frames per segment | `client/sentinel-core/src/protocol.rs`, `contracts/wire.md` |
| Ack cadence, close codes, resume semantics | `contracts/wire.md` |
| The RLS fixture and its six answers | `db/test/rls_test.sh` |
| Build order and the tier gate | `client/installer/build.ps1`, `client/installer/Sentinel.wxs`, `client/installer/README.md` |
| The finalize edge, the outbox and the four fields on the bus | `db/migrations/0007_finalize_outbox.up.sql`, `server/gateway/internal/outbox/`, `server/pipeline/sentinel_pipeline/consumer.py` |
| Non-exportable CNG device key, DPAPI machine scope | `client/sentinel-service/src/devicekey/`, `client/sentinel-service/src/spoolkey.rs` |

The simulation in diagram 04 is a direct transcription of the Rust detector's rules into
JavaScript. If you change the state machine, change it there too — or the page will
confidently show behaviour the product no longer has.

The same applies to what the page marks as unbuilt. Diagram 01 carried a dashed
"no publisher yet" edge for a long time, and it was right at the time; when the outbox
landed it became the page confidently showing a gap the product no longer had, which is
the worse direction for a diagram to be wrong in — a reader trusts it and stops looking.
The dashed-edge machinery is still in `diagrams.js` (`kind: 'todo'`) for the next honest
gap. Nothing currently uses it, and that is a claim about the tree, not an aspiration:
before adding a dashed edge, read the source; before removing one, read the source again.

## Publishing

`.github/workflows/pages.yml` deploys this directory on every push to `main` that touches
it. The workflow asks GitHub to enable Pages, but if the repository has never had Pages
turned on the first run may need **Settings → Pages → Source: GitHub Actions** set by
hand once.
