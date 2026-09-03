package ingest

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"time"

	"github.com/jackc/pgx/v5"

	"github.com/magickvoice/sentinel/server/gateway/internal/blob"
	"github.com/magickvoice/sentinel/server/gateway/internal/store"
	"github.com/magickvoice/sentinel/server/gateway/internal/telemetry"
	"github.com/magickvoice/sentinel/server/gateway/internal/wire"
)

// DBSink is the production Sink: audio to object storage, metadata to Postgres.
type DBSink struct {
	Store  *store.Store
	Blob   blob.Store
	Tenant string
	Ctx    context.Context
	// Metrics may be nil; every recorder tolerates it.
	Metrics *telemetry.Metrics

	// startedAt remembers each open call's start instant so PutSegment can report
	// ingest lag — how far behind live the floor is — without a query per segment.
	// A connection carries at most Config.MaxCallsPerConn calls, so the map is
	// bounded by that rather than by the shift's call volume.
	//
	// Guarded by a mutex even though ingest is single-goroutine per connection: a
	// Sink is handed to a Session by a closure in main.go, and the next thing that
	// wants a segment count will reach for it from somewhere else. A contended-free
	// mutex costs nothing and removes the question.
	mu        sync.Mutex
	startedAt map[string]time.Time
}

var _ Sink = (*DBSink)(nil)

func (d *DBSink) ctx() context.Context {
	if d.Ctx != nil {
		return d.Ctx
	}
	return context.Background()
}

func (d *DBSink) rememberStart(callID string, startedAt time.Time) {
	d.mu.Lock()
	defer d.mu.Unlock()
	if d.startedAt == nil {
		d.startedAt = map[string]time.Time{}
	}
	d.startedAt[callID] = startedAt
}

func (d *DBSink) startOf(callID string) (time.Time, bool) {
	d.mu.Lock()
	defer d.mu.Unlock()
	t, ok := d.startedAt[callID]
	return t, ok
}

func (d *DBSink) forgetStart(callID string) {
	d.mu.Lock()
	defer d.mu.Unlock()
	delete(d.startedAt, callID)
}

// EnsureCall creates the call row if it is new, and reports the durable ack
// watermarks so a reconnect resumes from the right place.
//
// The insert is ON CONFLICT DO NOTHING on a client-generated id, which is what makes
// the reconnect path idempotent without a second round trip to check first.
func (d *DBSink) EnsureCall(cs wire.CallStart, tenantID, deviceID string) (bool, map[uint8]uint32, error) {
	ctx := d.ctx()
	callUUID, err := callUUIDFromULID(cs.CallID)
	if err != nil {
		return false, nil, err
	}
	startedAt, err := time.Parse(time.RFC3339Nano, cs.StartedAt)
	if err != nil {
		return false, nil, fmt.Errorf("ingest: bad started_at %q: %w", cs.StartedAt, err)
	}

	var existed bool
	acked := map[uint8]uint32{}
	err = d.Store.AsSystem(ctx, tenantID, func(tx pgx.Tx) error {
		tag, err := tx.Exec(ctx,
			`INSERT INTO calls (id, tenant_id, device_id, user_uid, team_id, started_at,
			                    direction, account_ref, dialer_call_id, capture_tier, status)
			 SELECT $1, $2, $3, $4, u.team_id, $5, $6, $7, $8, $9, 'ingesting'
			   FROM users u WHERE u.firebase_uid = $4 AND u.tenant_id = $2
			 ON CONFLICT (id) DO NOTHING`,
			callUUID, tenantID, deviceID, cs.UserUID, startedAt,
			cs.Direction, cs.AccountRef, cs.DialerCallID, cs.Tier)
		if err != nil {
			return err
		}
		existed = tag.RowsAffected() == 0

		rows, err := tx.Query(ctx,
			`SELECT channel, through_seq FROM ingest_watermarks WHERE call_id = $1`, callUUID)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var ch int16
			var seq int32
			if err := rows.Scan(&ch, &seq); err != nil {
				return err
			}
			acked[uint8(ch)] = uint32(seq)
		}
		return rows.Err()
	})
	if err != nil {
		return false, nil, err
	}
	if existed && len(acked) == 0 {
		// The row exists but nothing is acked yet: still a resume, so the client is
		// told to start from zero rather than being handed a fresh call.
		acked = map[uint8]uint32{}
	}
	d.rememberStart(cs.CallID, startedAt)
	return existed, acked, nil
}

// PutSegment writes the audio then the row. That order matters: a row pointing at an
// object that does not exist is a broken call, whereas an object with no row is
// garbage the retention sweep collects.
func (d *DBSink) PutSegment(callID string, r wire.MediaRecord) error {
	ctx := d.ctx()
	callUUID, err := callUUIDFromULID(callID)
	if err != nil {
		return err
	}
	now := time.Now().UTC()
	day := now.Format("2006-01-02")
	key := blob.SegmentKey(d.Tenant, day, callID, r.Channel, r.Seq)
	if err := d.Blob.Put(ctx, key, r.Payload); err != nil {
		return fmt.Errorf("ingest: store segment audio: %w", err)
	}
	if err := d.Store.AsSystem(ctx, d.Tenant, func(tx pgx.Tx) error {
		_, err := tx.Exec(ctx,
			`INSERT INTO media_segments
			   (tenant_id, call_id, channel, seq, s3_key, bytes, duration_ms,
			    timestamp_ms, foreign_audio, silence_inserted)
			 VALUES ($1, $2, $3, $4, $5, $6, 1000, $7, $8, $9)
			 ON CONFLICT (call_id, channel, seq) DO NOTHING`,
			d.Tenant, callUUID, int16(r.Channel), int32(r.Seq), key, len(r.Payload),
			int64(r.TimestampMS), r.Flags.Foreign, r.Flags.SilenceInserted)
		return err
	}); err != nil {
		return err
	}

	// Recorded after the write, so the counter only ever counts audio that is
	// durable in both places. Counting on arrival would make a dead object store
	// look like a healthy floor, which is precisely the failure /readyz and these
	// metrics exist to surface.
	d.Metrics.IngestSegment(ctx, d.Tenant, r.Channel, len(r.Payload))
	if startedAt, ok := d.startOf(callID); ok {
		// Ingest lag: how old this audio was by the time it was stored. Derived
		// from the call's start instant plus the frame's call-relative timestamp,
		// which is the only pair available without a query per segment.
		//
		// It is an approximation and the error is worth naming: a desktop whose
		// clock is off shifts every one of its lag samples by that offset, and
		// collections desktops are not reliably time-synced (the same reason
		// auth.Verifier carries a leeway). What the number is actually good for is
		// the large signal — a floor draining a spool backlog after an outage
		// reports lag in hours, and clock skew does not reach hours. The
		// recorder clamps negatives, which is what skew in the other direction
		// looks like.
		captured := startedAt.Add(time.Duration(r.TimestampMS) * time.Millisecond)
		d.Metrics.IngestLag(ctx, d.Tenant, now.Sub(captured))
	}
	return nil
}

func (d *DBSink) SetWatermark(callID string, channel uint8, throughSeq uint32) error {
	ctx := d.ctx()
	callUUID, err := callUUIDFromULID(callID)
	if err != nil {
		return err
	}
	return d.Store.AsSystem(ctx, d.Tenant, func(tx pgx.Tx) error {
		_, err := tx.Exec(ctx,
			`INSERT INTO ingest_watermarks (call_id, channel, through_seq, updated_at)
			 VALUES ($1, $2, $3, now())
			 ON CONFLICT (call_id, channel)
			 DO UPDATE SET through_seq = GREATEST(ingest_watermarks.through_seq, excluded.through_seq),
			               updated_at = now()`,
			callUUID, int16(channel), int32(throughSeq))
		return err
	})
}

// FinishCall records call.end and, in the same transaction, queues the finalize
// message that hands the call to the pipeline.
//
// The two writes are one commit, and that is the whole design. Publishing to NATS
// here instead — after the transaction, in this function or in the caller — leaves a
// window in which the call is finalized in Postgres and the publish fails: broker
// restarting, network partition, this pod evicted between the two statements. The
// call then has audio in object storage, a row saying `transcribing`, minutes the
// customer is billed for, and nothing that will ever transcribe it. No error is
// raised, the request succeeded, and the call is simply absent from the compliance
// queue — indistinguishable from the thousands of calls that legitimately carry no
// findings. In a product sold on "100% of calls monitored", that is invisible data
// loss in the exact dimension the customer is buying.
//
// So the intent to publish is committed with the finalize, and
// internal/outbox drains it with retries. The argument in full, including why a
// retry loop around the publish is not equivalent, is in
// db/migrations/0007_finalize_outbox.up.sql.
func (d *DBSink) FinishCall(ce wire.CallEnd) error {
	ctx := d.ctx()
	callUUID, err := callUUIDFromULID(ce.CallID)
	if err != nil {
		return err
	}
	endedAt, err := time.Parse(time.RFC3339Nano, ce.EndedAt)
	if err != nil {
		return fmt.Errorf("ingest: bad ended_at %q: %w", ce.EndedAt, err)
	}
	var finalized bool
	if err := d.Store.AsSystem(ctx, d.Tenant, func(tx pgx.Tx) error {
		tag, err := tx.Exec(ctx,
			`UPDATE calls
			    SET ended_at = $2, end_reason = $3,
			        duration_ms = GREATEST(0, EXTRACT(epoch FROM ($2 - started_at)) * 1000)::int,
			        status = 'transcribing', updated_at = now()
			  WHERE id = $1 AND ended_at IS NULL`,
			callUUID, endedAt, ce.Reason)
		if err != nil {
			return err
		}
		// The `ended_at IS NULL` guard is what makes call.end idempotent, and the
		// enqueue rides on the same guard rather than repeating it: a replayed
		// call.end after a reconnect affects no row here and must not hand the
		// call to the pipeline a second time. (The outbox's primary key would
		// absorb it anyway; conditioning on the guard means the second attempt
		// does not even reach the table, so a call whose finalize has already been
		// published and pruned cannot be re-queued.)
		if tag.RowsAffected() == 0 {
			return nil
		}
		finalized = true
		return store.EnqueueCallFinalizeTx(ctx, tx, d.Tenant, callUUID, endedAt)
	}); err != nil {
		return err
	}
	if finalized {
		d.Metrics.CallFinalized(ctx, d.Tenant)
	}
	d.forgetStart(ce.CallID)
	return nil
}

var errBadCallID = errors.New("ingest: call id is not a ULID")

// callUUIDFromULID reinterprets the 128-bit ULID as a UUID, which is how the client's
// identifier lands in a uuid column without losing information or needing a mapping
// table.
func callUUIDFromULID(s string) (string, error) {
	b, err := parseULID(s)
	if err != nil {
		return "", fmt.Errorf("%w: %q", errBadCallID, s)
	}
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16]), nil
}
