# Architecture

Sentinel captures both sides of a collections call at the agent's Windows desktop, ships
them to a server as two separate audio channels, and analyses them there. This document
describes the shape of the system and then explains three design decisions in detail,
because they are the ones that get questioned in every review: why the client is two
processes, why no AI keys live on the endpoint, and why the two audio channels are never
mixed.

## The system

```
┌──────────────────────── Windows endpoint ────────────────────────┐
│                                                                   │
│  SentinelService.exe (SYSTEM)       SentinelAgent.exe (user)      │
│  ├─ updater                         ├─ auth (PKCE + token cache)  │
│  ├─ config sync                     ├─ capture (WASAPI)           │
│  ├─ watchdog          ◄──pipe──►    ├─ detector (state machine)   │
│  ├─ crash reporting                 ├─ encoder (Opus)             │
│  └─ named pipe host                 ├─ spool (SQLCipher)          │
│                                     ├─ uplink (WSS + mTLS)        │
│                                     └─ ui (WebView2)              │
└───────────────────────────────┬───────────────────────────────────┘
                                │  WSS / HTTPS, mTLS + bearer token
                     ┌──────────▼──────────┐
                     │  API + ingest       │   Go
                     │  gateway            │
                     └──────────┬──────────┘
                                │
               ┌────────────────┼─────────────────┐
               │                │                 │
        ┌──────▼─────┐   ┌──────▼──────┐   ┌──────▼─────┐
        │ Object     │   │ NATS        │   │ Postgres   │
        │ store (S3) │   │ JetStream   │   │ + pgvector │
        └──────┬─────┘   └──────┬──────┘   └──────▲─────┘
               │                │                 │
               └────────►┌──────▼───────────┐─────┘
                         │ Pipeline workers │   Python
                         │ ASR → analysis   │
                         │ → compliance     │
                         └──────────────────┘
                                │
                         ┌──────▼───────┐
                         │ Portal (SPA) │   React
                         └──────────────┘
```

**How much of this exists.** The gateway is real and runs. Postgres and its
row-level-security model are real and tested. The client's platform-neutral logic — the
call state machine, the spool, the wire codec — is real and tested, and the Windows
capture code is written but unexercised. Neither Windows binary in the top box exists
yet: there is no service, no agent executable, no named pipe, no PKCE flow, no Opus
encoder, no uplink client and no WebView2 shell. The pipeline is a set of libraries with
no worker loop and no NATS consumer. Object storage has a filesystem backend and no S3
adapter. See the README for the component-by-component state.

Data flows one way through the endpoint: two audio streams are captured, resampled to
16 kHz mono, encoded, framed with a sequence number and a call-relative timestamp,
written to the spool, and uploaded. Nothing is deleted from the spool until the server
acknowledges it. On the server, audio goes to object storage and metadata to Postgres;
the pipeline transcribes, analyses and evaluates compliance rules; the portal and the
widget read the results back through the same gateway.

## Why the client is two processes

The short answer is that a Windows service cannot capture a user's audio.

Windows services run in session 0, which has been isolated from interactive user
sessions since Windows Vista. WASAPI — the audio API Sentinel captures through — is
audio-session scoped, and those sessions belong to the interactive logon session. A
process in session 0 enumerating audio endpoints does not see the user's devices and
cannot open a capture stream on the user's softphone. There is no privilege that fixes
this; it is not a permissions problem but a namespace one.

So capture has to run in the user's session, as the user. That immediately gives up the
things a SYSTEM service is for: it cannot install its own updates, it cannot read
machine-scoped configuration that the user should not be able to edit, it cannot restart
itself after a crash, and it cannot write crash dumps to a location the user cannot
tamper with.

Hence two processes with a clean division:

**`SentinelService.exe`** runs as `LocalSystem`, `Automatic (Delayed Start)`, with
service recovery configured to restart after every failure. It stages and applies
updates, pulls tenant configuration, hosts the named pipe, watchdogs the agent, and ships
crash dumps. It must not touch audio at all — not as a policy preference but because it
cannot.

**`SentinelAgent.exe`** runs in the interactive session, as the logged-in user. It does
capture, call detection, encoding, spooling, upload, and the widget UI. One instance per
session, guarded by a session-scoped named mutex.

The service starts the agent from its session-change notification —
`SERVICE_CONTROL_SESSIONCHANGE` with `WTS_SESSION_LOGON` — rather than from a Run key. A
Run key is a registry value the user can delete in thirty seconds, and this is software
that scores the people it runs next to. On agent crash, the service relaunches with
exponential backoff and counts the restarts into the next heartbeat.

They talk over a named pipe, `\\.\pipe\magickvoice-sentinel-v1`, with a security
descriptor granting read and write to `BUILTIN\Users` and full control to `SYSTEM`,
carrying length-prefixed JSON.

One consequence worth spelling out, because it explains an otherwise odd choice
elsewhere: the two processes run as different principals, which is why the spool
encryption key is wrapped with DPAPI at **machine** scope rather than user scope. A
user-scoped wrap would leave one of the two unable to open the file.

## Why no AI keys live on the endpoint

Every model invocation is server-side. No endpoint holds a provider API key, and no
endpoint talks to a model provider directly. This is not a deployment convenience; there
are four separate reasons, and the fourth is the one that matters commercially.

**Key security.** A key shipped to 200 desktops is a key that has been published. It sits
in a binary or a configuration file on machines that agents have physical access to, that
leave the building in laptop bags, and that are re-imaged by people who keep copies.
Rotating it means redeploying to the whole fleet. There is no version of endpoint key
distribution that survives a bank's security review.

**Per-tenant cost accounting.** Sentinel is multi-tenant, and model spend is the
dominant variable cost. Attributing that spend correctly requires the invocation to
happen somewhere the tenant is known from a verified identity rather than asserted by a
client. Server-side invocation is also what makes the cost controls enforceable at all:
per-tenant monthly budgets with alerts at 70% and 90%, a per-call token ceiling, skipping
analysis for calls under 15 seconds, and a kill switch that drops to tier-1 rules only
when spend spikes. None of those can be enforced by a client that holds its own key.

**Model swaps without redeploying 200 desktops.** Providers get replaced, prompts get
tuned, a model version is deprecated with three months' notice. Server-side, that is a
configuration change — prompts and rule sets are stored in Postgres, versioned, with an
active-version pointer per tenant, so changing one creates a new version rather than
mutating the old. Client-side, every one of those is an MSI rollout through the
customer's SCCM or Intune, scheduled against a floor that runs three shifts. The
difference between a config change and a fleet rollout is the difference between
improving the product weekly and improving it quarterly.

**A single auditable log of every prompt and response.** This is the one that decides
deals. A bank asks how a specific compliance flag was decided on a specific call. With
server-side invocation there is one place that holds the prompt version, the model
version, the transcript span that was sent, and the verdict that came back — and the
schema requires the judge to return the transcript span it relied on, with an upheld
verdict lacking a span discarded rather than stored
(`server/pipeline/sentinel_pipeline/compliance/judge.py`). The answer is a record.

With endpoint invocation, the answer is that the flag was produced on a desktop that has
since been re-imaged, by a model version nobody recorded, from a prompt that may or may
not have been the current one. That is not an answer a bank accepts, and a compliance
product that cannot explain its own conclusions is not a compliance product.

## Why the two channels are never mixed

Sentinel captures two independent streams and keeps them separate end to end:

| Channel | Source | Contains |
|---|---|---|
| `0` (`far`) | Render loopback | The borrower's voice |
| `1` (`near`) | Microphone capture | The agent's voice |

They are never summed into a mono recording. Not on the endpoint, not in the spool, not
in the wire protocol — the frame header carries a channel byte
(`contracts/wire.md`, `client/sentinel-core/src/protocol.rs`) — not in object storage,
where `blob.SegmentKey` includes the channel, and not in the database, where
`media_segments` and `transcripts` are both keyed by `(call_id, channel)`.

The reason is speaker attribution. A mono recording of a two-party call requires
diarization: a model that partitions the audio into speakers and guesses which is which.
Diarization is a source of errors, and its errors are the expensive kind. It confuses
speakers during overlapping speech, which on a collections call is precisely the moment
you care about — the interruption, the raised voice, the threat talked over. It performs
worse on short turns, worse on telephony-band audio, and worse on code-mixed speech, all
three of which describe every call this product will ever see.

With separate channels the speaker is known, not inferred. Channel 1 is the agent because
it came from the agent's microphone. There is no confidence score attached to that fact
and no failure mode.

That is not an incremental quality improvement; it is what makes several features
possible at all. Conduct rules apply to the agent and not to the borrower — flagging a
borrower's own swearing as `abusive_language` would flood the compliance queue with
noise and destroy trust in the tool within a week — and the rule engine can only make
that distinction because it knows which channel it is reading
(`server/pipeline/sentinel_pipeline/compliance/engine.py`). Per-speaker sentiment, and
the open-minus-close delta that supervisors actually use, needs two curves. Talk ratio
and interruption counts need to know who was talking. Each of these would be a derived
guess on a mono recording.

There is no diarization step in this pipeline and there must never be one. Where the two
channels are brought back together — `transcriptTurns` in
`server/gateway/internal/store/queries.go`, which merges them into one time-ordered
transcript for display — the speaker travels with the turn rather than being recomputed.

Two implementation details keep this true in practice. Both channels derive their
timestamps from a single call-scoped monotonic clock, because channel drift breaks
transcript alignment and therefore breaks the evidence spans that compliance flags depend
on. And when the audio engine reports a glitch gap
(`AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`), silence is inserted rather than the gap being
closed up, so a dropout on one channel does not shift everything after it relative to the
other.
