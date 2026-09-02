package ingest

import (
	"context"
	"errors"
	"fmt"
	"time"

	"github.com/jackc/pgx/v5"

	"github.com/magickvoice/sentinel/server/gateway/internal/blob"
	"github.com/magickvoice/sentinel/server/gateway/internal/store"
	"github.com/magickvoice/sentinel/server/gateway/internal/wire"
)

// DBSink is the production Sink: audio to object storage, metadata to Postgres.
type DBSink struct {
	Store  *store.Store
	Blob   blob.Store
	Tenant string
	Ctx    context.Context
}

var _ Sink = (*DBSink)(nil)

func (d *DBSink) ctx() context.Context {
	if d.Ctx != nil {
		return d.Ctx
	}
	return context.Background()
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
	day := time.Now().UTC().Format("2006-01-02")
	key := blob.SegmentKey(d.Tenant, day, callID, r.Channel, r.Seq)
	if err := d.Blob.Put(ctx, key, r.Payload); err != nil {
		return fmt.Errorf("ingest: store segment audio: %w", err)
	}
	return d.Store.AsSystem(ctx, d.Tenant, func(tx pgx.Tx) error {
		_, err := tx.Exec(ctx,
			`INSERT INTO media_segments
			   (tenant_id, call_id, channel, seq, s3_key, bytes, duration_ms,
			    timestamp_ms, foreign_audio, silence_inserted)
			 VALUES ($1, $2, $3, $4, $5, $6, 1000, $7, $8, $9)
			 ON CONFLICT (call_id, channel, seq) DO NOTHING`,
			d.Tenant, callUUID, int16(r.Channel), int32(r.Seq), key, len(r.Payload),
			int64(r.TimestampMS), r.Flags.Foreign, r.Flags.SilenceInserted)
		return err
	})
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
	return d.Store.AsSystem(ctx, d.Tenant, func(tx pgx.Tx) error {
		_, err := tx.Exec(ctx,
			`UPDATE calls
			    SET ended_at = $2, end_reason = $3,
			        duration_ms = GREATEST(0, EXTRACT(epoch FROM ($2 - started_at)) * 1000)::int,
			        status = 'transcribing', updated_at = now()
			  WHERE id = $1 AND ended_at IS NULL`,
			callUUID, endedAt, ce.Reason)
		return err
	})
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
