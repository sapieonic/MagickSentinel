# Sentinel wire protocol — `WSS /v1/ingest`

**Version:** 1
**Status:** authoritative. Client (`client/sentinel-core/src/protocol.rs`) and server
(`server/gateway/internal/wire`) both implement this document; a change starts here.

---

## 1. Connection

```
GET /v1/ingest
Upgrade: websocket
Authorization: Bearer <firebase id token>
Sec-WebSocket-Protocol: sentinel.v1
```

Transport is TLS 1.3 with **mutual TLS**: the client presents the device certificate
issued by `POST /v1/devices/enroll`. The gateway derives:

- `tenant_id`, `device_id` from the **client certificate** (never from the payload),
- `user_uid`, `role`, `tenant_id` from the **bearer token**.

If the tenant in the certificate and the tenant in the token disagree, the gateway
MUST close with `4403`.

### Close codes

| Code | Meaning | Client behaviour |
|---|---|---|
| `1000` | normal | reconnect on next call |
| `1011` | server error | reconnect with backoff |
| `4401` | token expired / invalid | refresh token, reconnect |
| `4403` | device revoked, tenant mismatch, or role not permitted | STOP capture, surface error state |
| `4408` | idle timeout (no frames for 120 s) | reconnect lazily |
| `4429` | too many connections for this device | backoff, do not spin |

`4403` is terminal until the operator acts: the client MUST stop capture within 60 s of
receiving it (section 7.2 of the spec: device revocation).

---

## 2. Frame taxonomy

| WebSocket frame type | Meaning |
|---|---|
| text | control message, JSON, one object per frame |
| binary | one or more concatenated media records |

---

## 3. Control messages (JSON text frames)

Every control message has a `t` discriminator.

### 3.1 `call.start` — client → server

```json
{"t":"call.start","call_id":"01J8ZQ8H2Q7X9K3M4N5P6R7S8T",
 "started_at":"2026-09-01T10:14:02.113Z","user_uid":"KnA1...","device_id":"...",
 "tier":"A","account_ref":"LN-88213","dialer_call_id":null,
 "direction":"outbound","codec":"opus","rate":16000}
```

| Field | Type | Notes |
|---|---|---|
| `call_id` | ULID (Crockford base32, 26 chars) | **client-generated**. The server MUST NOT assign it — client generation is what makes retry-after-reconnect idempotent. |
| `started_at` | RFC3339 UTC, millisecond precision | wall clock at `IN_CALL` entry |
| `user_uid` | string | MUST equal the bearer token subject; mismatch ⇒ `call.error` |
| `device_id` | uuid | MUST equal the certificate device; mismatch ⇒ `call.error` |
| `tier` | `"A"` \| `"B"` | capture tier in effect for this call |
| `account_ref` | string \| null | best-effort UIA scrape; null is normal |
| `dialer_call_id` | string \| null | reserved for CDR reconciliation |
| `direction` | `"outbound"` \| `"inbound"` | |
| `codec` | `"opus"` | only value in v1 |
| `rate` | `16000` | only value in v1 |

Re-sending `call.start` for a `call_id` the server already knows is **not** an error: it
is the reconnect path. The server replies with `resume` (3.5) instead of creating a row.

### 3.2 `call.end` — client → server

```json
{"t":"call.end","call_id":"01J8...","ended_at":"2026-09-01T10:19:44.802Z",
 "reason":"hangup","last_seq":{"0":842,"1":842}}
```

`reason` ∈ `hangup | device_lost | signed_out | shutdown | revoked | error`.

`last_seq` is the highest sequence number the client produced per channel. The server
uses it to decide whether the call is complete or has holes, and holds finalization
until every sequence up to `last_seq` has arrived or the hole timeout (5 min) expires.

The client MUST NOT delete spooled segments on `call.end`; only on `ack`.

### 3.3 `ack` — server → client

```json
{"t":"ack","call_id":"01J8...","channel":0,"through_seq":840}
```

Cumulative per `(call_id, channel)`: every sequence ≤ `through_seq` is durable. The
server MUST ack at least every **2 s** or every **100 segments** per channel, whichever
comes first.

### 3.4 `call.error` — server → client

```json
{"t":"call.error","call_id":"01J8...","code":"tenant_mismatch","message":"...",
 "fatal":true}
```

`code` ∈ `tenant_mismatch | user_mismatch | device_mismatch | unknown_call |
bad_frame | quota_exceeded | internal`. `fatal:true` means the client MUST discard the
call's spool rows (they can never be accepted) and emit a `spool_eviction` event.

### 3.5 `resume` — server → client

Sent in response to a `call.start` for a call the server already has:

```json
{"t":"resume","call_id":"01J8...","acked":{"0":840,"1":839}}
```

The client resumes from `acked[channel] + 1` on each channel.

### 3.6 `heartbeat` / `heartbeat.ack`

```json
{"t":"heartbeat","sent_at":"...","capture_state":"IN_CALL","spool_depth":12}
{"t":"heartbeat.ack","server_time":"...","policy_version":7}
```

Sent on the ingest socket every 30 s while connected. It is a liveness and clock-skew
probe only — the authoritative heartbeat is `POST /v1/heartbeat`. If `policy_version`
differs from the one the client holds, the client MUST re-fetch `GET /v1/policy`.

---

## 4. Media records (binary frames)

Little-endian. Header is 34 bytes, followed by the Opus payload.

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 1 | `version` | = 1 |
| 1 | 1 | `channel` | 0 = far (borrower, render loopback), 1 = near (agent, mic) |
| 2 | 1 | `flags` | bit0 `foreign`, bit1 `silence_inserted`; bits 2–7 reserved, MUST be 0 |
| 3 | 1 | reserved | MUST be 0 |
| 4 | 4 | `seq` | u32, per `(call_id, channel)`, starts at 0, strictly increasing |
| 8 | 8 | `timestamp_ms` | u64, call-relative, from the single call-scoped clock shared by both channels |
| 16 | 16 | `call_id` | ULID in **binary** form (128-bit big-endian, as ULID canonical order) |
| 32 | 2 | `payload_len` | u16, length of the Opus payload that follows |
| 34 | n | `payload` | one 1-second segment = 50 × 20 ms Opus frames, length-delimited (see 4.1) |

Multiple records MAY be concatenated in a single WebSocket binary message. The client
batches **10 segments** (≈10 s) per message. A message MUST NOT be split across
WebSocket frames in a way that splits a record.

### 4.1 Segment payload framing

A segment payload is 50 consecutive Opus packets:

```
[u16 len][opus bytes] × 50
```

A packet length of 0 means "dropped frame" (glitch); the decoder inserts 20 ms of
silence. `silence_inserted` is set on the record when any packet in it was synthesised
so timestamps stay aligned across channels.

### 4.2 Flags

- `foreign` (bit0) — Tier B only. Loopback energy exceeded the VAD threshold while the
  softphone's audio session state was `Inactive`, so this is not call audio. The server
  **stores** the segment (for audit: we can prove what we discarded) but MUST NOT
  transcribe it. `media_segments.foreign_audio = true`.
- `silence_inserted` (bit1) — the record contains synthesised silence covering a
  `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY` gap.

---

## 5. Rules

1. `call_id` is client-generated. The server MUST NOT assign one.
2. Ingest is **idempotent on `(call_id, channel, seq)`**. Duplicate writes are dropped
   silently and still counted toward the next ack.
3. A record whose `call_id` has no `call.start` is buffered for 30 s awaiting one, then
   rejected with `call.error{code:"unknown_call"}`.
4. Sequence numbers MUST NOT be reused within a call. After a reconnect the client
   continues from `acked + 1`; it never restarts at 0.
5. Records with `flags.foreign = 1` are stored, never transcribed.
6. The server closes with `4408` after 120 s with no frames of any kind.
7. Maximum WebSocket message size is 1 MiB. Larger ⇒ `bad_frame`, connection closed
   with `1009`.

---

## 6. Reconnect

Exponential backoff with **full jitter**: `sleep = random(0, min(cap, base * 2^n))`,
base 1 s, cap 60 s, reset on a successful `call.start` round-trip.

On reconnect, for every call the spool still holds unacked segments for, the client:

1. sends `call.start` again (verbatim, same `call_id`),
2. waits for `resume`,
3. replays from `acked[channel] + 1`,
4. re-sends `call.end` if the call had already ended locally.

---

## 7. Sizing

Opus 16 kHz / 24 kbps / 20 ms ⇒ 60 bytes per frame per channel, ≈3 KB/s per channel,
≈6 KB/s for both. Header overhead is 34 bytes per 1 s segment (≈1%). 200 concurrent
agents ≈ 1.2 MB/s aggregate upstream.
