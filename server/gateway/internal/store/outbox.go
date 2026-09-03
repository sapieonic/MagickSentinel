package store

import (
	"context"
	"time"

	"github.com/jackc/pgx/v5"
)

// The finalize outbox. See db/migrations/0007_finalize_outbox.up.sql for why the
// gateway does not simply publish to NATS at the end of the finalize handler.

// OutboxEntry is one queued finalize message.
//
// It carries exactly the four fields the message payload carries and nothing else.
// That is deliberate: nothing about a borrower travels on the bus. The pipeline
// re-reads the transcript, the analysis and the account reference from Postgres under
// its own row-level-security context, so a compromised broker — or a NATS operator at
// the customer, or an accidentally-durable stream nobody purges — holds a list of
// identifiers rather than a list of debt collection calls.
type OutboxEntry struct {
	// CallID is the UUID rendering. The message payload carries the ULID; the
	// conversion is the publisher's job because the ULID is a wire-protocol
	// concern, not a schema one.
	CallID      string
	TenantID    string
	FinalizedAt time.Time
	Attempt     int
}

// EnqueueCallFinalizeTx queues a finalize message inside a transaction the caller
// already owns.
//
// It takes a pgx.Tx rather than opening one, and that signature is the whole point of
// the outbox: the only correct caller is the one that is in the middle of committing
// the finalize itself, so the message and the `calls` update land in the same commit.
// A convenience wrapper that opened its own transaction would reintroduce exactly the
// two-phase failure the table exists to eliminate, so there is not one.
//
// ON CONFLICT DO NOTHING makes it idempotent on call id: a client that replays
// call.end after a reconnect cannot enqueue a second message.
func EnqueueCallFinalizeTx(ctx context.Context, tx pgx.Tx, tenantID, callUUID string, finalizedAt time.Time) error {
	_, err := tx.Exec(ctx,
		`INSERT INTO call_finalize_outbox (call_id, tenant_id, finalized_at)
		 VALUES ($1, $2, $3)
		 ON CONFLICT (call_id) DO NOTHING`,
		callUUID, tenantID, finalizedAt)
	return err
}

// ClaimFinalize takes a batch of due messages, advancing each row's attempt count and
// backoff as it claims it.
//
// These four calls go through the SECURITY DEFINER functions in migration 0007
// rather than through AsIdentity, because a queue drainer is inherently
// cross-tenant: it is one goroutine serving every tenant on the deployment, and
// making it iterate tenants would first require enumerating `tenants`, which is
// itself a read no tenant context permits. The functions are narrow enough that this
// is not a hole — a claim returns four identifiers and cannot be used to browse.
func (s *Store) ClaimFinalize(ctx context.Context, limit int, now time.Time) ([]OutboxEntry, error) {
	rows, err := s.pool.Query(ctx,
		`SELECT call_id::text, tenant_id::text, finalized_at, attempt
		   FROM sentinel_outbox_claim($1, $2)`, limit, now)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []OutboxEntry
	for rows.Next() {
		var e OutboxEntry
		if err := rows.Scan(&e.CallID, &e.TenantID, &e.FinalizedAt, &e.Attempt); err != nil {
			return nil, err
		}
		out = append(out, e)
	}
	return out, rows.Err()
}

// MarkFinalizePublished records that the broker acked the message.
func (s *Store) MarkFinalizePublished(ctx context.Context, callUUID string, now time.Time) error {
	_, err := s.pool.Exec(ctx, `SELECT sentinel_outbox_published($1, $2)`, callUUID, now)
	return err
}

// MarkFinalizeFailed records the transport failure. The row stays claimable: the
// backoff was already applied when it was claimed, and nothing here gives up on a
// message, because a finalize we stop retrying is a call that is captured, billed and
// never analysed.
func (s *Store) MarkFinalizeFailed(ctx context.Context, callUUID, reason string) error {
	_, err := s.pool.Exec(ctx, `SELECT sentinel_outbox_failed($1, $2)`, callUUID, reason)
	return err
}

// OutboxDepth reports how many messages are waiting and how old the oldest is.
//
// Both numbers, not just the count: a busy floor's queue is briefly non-empty all the
// time, so depth alone is a noisy alarm. A queue whose oldest entry is twenty minutes
// old means the pipeline has stopped receiving work, and that is worth waking someone
// for. oldest is the zero time when the queue is empty.
func (s *Store) OutboxDepth(ctx context.Context) (pending int64, oldest time.Time, err error) {
	var oldestPtr *time.Time
	err = s.pool.QueryRow(ctx,
		`SELECT pending, oldest_unpublished FROM sentinel_outbox_depth()`,
	).Scan(&pending, &oldestPtr)
	if err != nil {
		return 0, time.Time{}, err
	}
	if oldestPtr != nil {
		oldest = *oldestPtr
	}
	return pending, oldest, nil
}
