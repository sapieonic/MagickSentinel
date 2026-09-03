// Package outbox drains the finalize queue into NATS JetStream.
//
// The gateway is the producer for `sentinel.call.finalize`, the one subject the
// Python pipeline consumes (server/pipeline/sentinel_pipeline/consumer.py). Until
// this package existed there was no producer at all: the consumer was written and
// tested, the stream constants were agreed, and nothing ever published, so the
// pipeline could not receive work.
//
// Why the queue is in Postgres rather than a publish call at the end of the finalize
// handler is argued at length in db/migrations/0007_finalize_outbox.up.sql. The short
// version: a publish that fails after the database commit means a call is captured,
// stored, billed and never analysed, with no error anywhere and nothing that would
// notice. One commit for both the finalize and the intent to publish is the only
// construction that closes that window.
//
// # What travels on the bus
//
// Four fields, and the set is fixed:
//
//	{"call_id": "<ulid>", "tenant_id": "<uuid>", "attempt": 1, "finalized_at": "..."}
//
// No transcript, no audio, no summary, no account reference, nothing about a
// borrower. The pipeline re-reads everything it needs from Postgres under its own
// row-level-security context, which means the broker holds a list of identifiers
// rather than a list of debt-collection calls. That matters concretely: JetStream
// streams are durable and replicated, they are frequently operated by the customer
// rather than by us, and a stream nobody purges is outside the retention regime that
// server/pipeline/sentinel_pipeline/retention.py enforces over rows and objects.
// Adding a field here is a data-residency and retention decision, not a convenience.
package outbox

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"time"

	"github.com/google/uuid"
	"github.com/oklog/ulid/v2"

	"github.com/magickvoice/sentinel/server/gateway/internal/store"
	"github.com/magickvoice/sentinel/server/gateway/internal/telemetry"
)

// Stream and Subject mirror STREAM and SUBJECT_FINALIZE in
// server/pipeline/sentinel_pipeline/consumer.py. They are duplicated because the two
// halves are in different languages; changing one without the other silently stops
// the pipeline receiving work, so they are named as constants on both sides rather
// than spelled inline.
const (
	Stream  = "SENTINEL"
	Subject = "sentinel.call.finalize"
	// SubjectFilter is what the stream must capture. It has to include the
	// dead-letter subject as well as the finalize one, because consumer.py
	// republishes an undeliverable message to `sentinel.call.dlq` through the same
	// JetStream context — a stream that only captured the finalize subject would
	// make that republish fail and the poison message would loop forever.
	SubjectFilter = "sentinel.call.>"
)

// message is the payload. Field names and the set of fields are the contract with
// consumer.py; there is no struct tag here that can be changed safely.
type message struct {
	CallID      string `json:"call_id"`
	TenantID    string `json:"tenant_id"`
	Attempt     int    `json:"attempt"`
	FinalizedAt string `json:"finalized_at"`
}

// Publisher is the transport. An interface so the drain logic — which is the part
// with the retry semantics worth testing — runs without a broker.
type Publisher interface {
	// Publish must not return nil until the broker has durably accepted the
	// message. A fire-and-forget publish would put the outbox's entire reason for
	// existing back on the floor: the row would be marked published on the strength
	// of a write to a socket buffer.
	Publish(ctx context.Context, subject, dedupeID string, payload []byte) error
	Close()
}

// Queue is the persistence the drainer needs. Implemented by *store.Store.
type Queue interface {
	ClaimFinalize(ctx context.Context, limit int, now time.Time) ([]store.OutboxEntry, error)
	MarkFinalizePublished(ctx context.Context, callUUID string, now time.Time) error
	MarkFinalizeFailed(ctx context.Context, callUUID, reason string) error
}

// Drainer moves claimed rows onto the bus.
type Drainer struct {
	Queue     Queue
	Publisher Publisher
	Log       *slog.Logger
	Metrics   *telemetry.Metrics
	// Interval is how often an empty queue is re-checked. Batch is how many rows
	// one pass claims.
	Interval time.Duration
	Batch    int
	Now      func() time.Time
}

func (d *Drainer) now() time.Time {
	if d.Now != nil {
		return d.Now()
	}
	return time.Now()
}

func (d *Drainer) interval() time.Duration {
	if d.Interval <= 0 {
		return 2 * time.Second
	}
	return d.Interval
}

func (d *Drainer) batch() int {
	if d.Batch <= 0 {
		return 64
	}
	return d.Batch
}

func (d *Drainer) log() *slog.Logger {
	if d.Log != nil {
		return d.Log
	}
	return slog.Default()
}

// Run drains until ctx is cancelled.
//
// The loop keeps going after every failure, including a failure to reach the database
// at all. There is no error return and no exit condition other than cancellation,
// because the process this goroutine belongs to is a gateway: stopping the drainer
// while continuing to accept ingest would go back to accumulating calls that are
// captured and never analysed, which is the invisible failure the whole design is
// built to avoid. A stuck drainer is visible instead — sentinel.outbox.oldest_age
// climbs, and that is the number to alarm on.
func (d *Drainer) Run(ctx context.Context) {
	t := time.NewTicker(d.interval())
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			// Drain until a pass comes back short, so a backlog that built up
			// during a broker outage clears at the broker's pace rather than at
			// one batch per tick.
			for {
				n, err := d.DrainOnce(ctx)
				if err != nil {
					d.log().Error("outbox: drain", "error", err)
					break
				}
				if n < d.batch() {
					break
				}
				if ctx.Err() != nil {
					return
				}
			}
		}
	}
}

// DrainOnce claims one batch and publishes it, returning how many rows it claimed.
//
// The return value is the claim count rather than the publish count on purpose: the
// caller uses it to decide whether more work is waiting, and a batch where every
// publish failed still means the queue was full.
func (d *Drainer) DrainOnce(ctx context.Context) (int, error) {
	now := d.now()
	entries, err := d.Queue.ClaimFinalize(ctx, d.batch(), now)
	if err != nil {
		return 0, fmt.Errorf("outbox: claim: %w", err)
	}

	published := int64(0)
	for _, e := range entries {
		if ctx.Err() != nil {
			// Rows already claimed but not published are not lost: their backoff
			// was applied at claim time, so the next process to run picks them up.
			break
		}
		if err := d.publish(ctx, e); err != nil {
			continue
		}
		published++
	}
	d.Metrics.OutboxPublished(ctx, published)
	return len(entries), nil
}

func (d *Drainer) publish(ctx context.Context, e store.OutboxEntry) error {
	callULID, err := ulidFromUUID(e.CallID)
	if err != nil {
		// A call id that is not a 128-bit value cannot happen: the column is uuid
		// and the ingest layer wrote it. If it ever does, retrying forever would
		// wedge the queue behind one unusable row, so it is marked and skipped —
		// and it is loud, because it means something wrote a call id we did not
		// mint.
		d.Metrics.OutboxFailure(ctx, "encode")
		d.log().Error("outbox: call id is not a 128-bit identifier", "error", err)
		return d.fail(ctx, e, "unusable call id")
	}
	payload, err := json.Marshal(message{
		CallID:   callULID,
		TenantID: e.TenantID,
		Attempt:  e.Attempt,
		// RFC3339 with second precision, matching what the pipeline parses. UTC
		// rather than the tenant's Asia/Kolkata: a timestamp on a message bus is
		// an instant, and the display timezone belongs at the point of display.
		FinalizedAt: e.FinalizedAt.UTC().Format(time.RFC3339),
	})
	if err != nil {
		d.Metrics.OutboxFailure(ctx, "encode")
		return d.fail(ctx, e, "unencodable payload")
	}

	// The call id is the deduplication key. JetStream collapses a repeat of the
	// same Nats-Msg-Id inside the stream's duplicate window, which turns the
	// outbox's at-least-once delivery into effectively-once for the common case —
	// a publish that succeeded and whose row we then failed to mark. The consumer
	// is idempotent regardless (consumer.py's docstring), so this is an
	// optimisation rather than a correctness requirement, and it must stay that
	// way: the duplicate window is measured in minutes and a row can be retried
	// for hours.
	if err := d.Publisher.Publish(ctx, Subject, callULID, payload); err != nil {
		d.Metrics.OutboxFailure(ctx, "publish")
		// No call id in the log line beyond the identifier itself, and no payload:
		// the broker's error can carry a server name and a subject, neither of
		// which is call content, but the habit is worth keeping.
		d.log().Warn("outbox: publish failed, will retry",
			"attempt", e.Attempt, "error", err)
		return d.fail(ctx, e, err.Error())
	}

	if err := d.Queue.MarkFinalizePublished(ctx, e.CallID, d.now()); err != nil {
		// Published but not marked. The row will be claimed again and republished;
		// the dedupe id above usually absorbs it and the consumer absorbs the rest.
		// Counted separately from a publish failure because the two mean different
		// things operationally: this one is a database problem, not a broker one.
		d.Metrics.OutboxFailure(ctx, "mark")
		d.log().Error("outbox: published but could not mark the row", "error", err)
		return err
	}
	return nil
}

func (d *Drainer) fail(ctx context.Context, e store.OutboxEntry, reason string) error {
	if err := d.Queue.MarkFinalizeFailed(ctx, e.CallID, reason); err != nil {
		d.log().Error("outbox: could not record the failure", "error", err)
	}
	return errors.New(reason)
}

// ulidFromUUID re-renders a call id from its uuid column as the ULID the wire
// protocol uses.
//
// The two are the same 128 bits: the client mints a ULID, and
// ingest.callUUIDFromULID reinterprets it as a UUID so it lands in a uuid column
// without a mapping table (see internal/ingest/sink.go). This is the inverse, and it
// is needed because the message payload has to carry the identifier the *client* and
// the pipeline both use. Publishing the UUID rendering instead would produce
// messages the pipeline accepts and then cannot join to anything a supervisor can
// search for.
func ulidFromUUID(s string) (string, error) {
	u, err := uuid.Parse(s)
	if err != nil {
		return "", err
	}
	return ulid.ULID(u).String(), nil
}
