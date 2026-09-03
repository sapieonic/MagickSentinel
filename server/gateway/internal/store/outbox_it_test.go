package store_test

import (
	"context"
	"fmt"
	"os"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"

	"github.com/magickvoice/sentinel/server/gateway/internal/store"
)

// The finalize outbox against a real Postgres with the real row-level-security
// policies and the real SECURITY DEFINER functions from
// db/migrations/0007_finalize_outbox.up.sql.
//
// These have to run against a database rather than a fake, because the properties
// worth checking are the ones the SQL provides: that the claim advances the attempt
// counter and the backoff atomically, that the primary key makes the enqueue
// idempotent, and that the application role — which is NOBYPASSRLS — can actually
// execute the functions it was granted and nothing more.
//
// Skipped, not failed, when no database is available, matching the pattern in
// internal/api/integration_test.go so `go test ./...` stays runnable without one.

const (
	outboxTenant = "31111111-1111-1111-1111-111111111111"
	outboxDevice = "3ddddddd-0000-0000-0000-000000000001"
	outboxCallA  = "3c000000-0000-0000-0000-00000000000a"
	outboxCallB  = "3c000000-0000-0000-0000-00000000000b"
)

type outboxFixture struct {
	t     *testing.T
	store *store.Store
	admin *pgxpool.Pool
}

func newOutboxFixture(t *testing.T) *outboxFixture {
	t.Helper()
	dsn := os.Getenv("SENTINEL_TEST_DATABASE_URL")
	if dsn == "" {
		t.Skip("SENTINEL_TEST_DATABASE_URL not set; run via db/test/gateway_it.sh")
	}
	adminDSN := os.Getenv("SENTINEL_TEST_ADMIN_DATABASE_URL")
	if adminDSN == "" {
		t.Skip("SENTINEL_TEST_ADMIN_DATABASE_URL not set; run via db/test/gateway_it.sh")
	}
	ctx := context.Background()

	pool, err := pgxpool.New(ctx, dsn)
	if err != nil {
		t.Fatalf("connect as the application role: %v", err)
	}
	t.Cleanup(pool.Close)
	admin, err := pgxpool.New(ctx, adminDSN)
	if err != nil {
		t.Fatalf("connect as the schema owner: %v", err)
	}
	t.Cleanup(admin.Close)

	f := &outboxFixture{t: t, store: store.New(pool), admin: admin}
	f.seed(ctx)
	return f
}

func (f *outboxFixture) seed(ctx context.Context) {
	f.t.Helper()
	// Only this test's own rows are reset rather than the schema being truncated:
	// the api package's integration fixture truncates, and the two suites run in
	// the same `go test ./...` invocation against the same database.
	//
	// Deleting the calls is what resets the outbox, because call_finalize_outbox
	// cascades from them. The tenant, user and device are left in place and
	// re-inserted idempotently — `users.tenant_id` has no ON DELETE CASCADE, so
	// removing the tenant would be a four-statement dance for no benefit.
	stmts := []string{
		fmt.Sprintf(`INSERT INTO tenants (id, name, idp_tenant_id)
		     VALUES ('%s','Outbox BPO','outbox-bpo') ON CONFLICT (id) DO NOTHING`, outboxTenant),
		fmt.Sprintf(`INSERT INTO users (firebase_uid, tenant_id, role, display_name)
		     VALUES ('outbox-agent','%s','agent','Outbox Agent')
		     ON CONFLICT (firebase_uid) DO NOTHING`, outboxTenant),
		fmt.Sprintf(`INSERT INTO devices (id, tenant_id, machine_guid, hw_fingerprint,
		            cert_fingerprint, os_build, capture_tier, agent_version)
		     VALUES ('%s','%s','outbox-mg','outbox-hw','outbox-cf','10.0.22631','A','1.0.0')
		     ON CONFLICT (id) DO NOTHING`, outboxDevice, outboxTenant),
		fmt.Sprintf(`DELETE FROM calls WHERE tenant_id = '%s'`, outboxTenant),
		fmt.Sprintf(`INSERT INTO calls (id, tenant_id, device_id, user_uid, started_at,
		            capture_tier, status)
		     VALUES ('%[1]s','%[3]s','%[4]s','outbox-agent','2026-09-01T10:00:00Z','A','ingesting'),
		            ('%[2]s','%[3]s','%[4]s','outbox-agent','2026-09-01T11:00:00Z','A','ingesting')`,
			outboxCallA, outboxCallB, outboxTenant, outboxDevice),
	}
	for _, s := range stmts {
		if _, err := f.admin.Exec(ctx, s); err != nil {
			f.t.Fatalf("seed: %v\n%s", err, s)
		}
	}
}

// enqueue runs EnqueueCallFinalizeTx the way ingest does: inside a transaction that
// already carries the tenant context.
func (f *outboxFixture) enqueue(ctx context.Context, callID string, at time.Time) error {
	return f.store.AsSystem(ctx, outboxTenant, func(tx pgx.Tx) error {
		return store.EnqueueCallFinalizeTx(ctx, tx, outboxTenant, callID, at)
	})
}

func (f *outboxFixture) rowState(ctx context.Context, callID string) (attempt int, published bool, lastError *string) {
	f.t.Helper()
	err := f.admin.QueryRow(ctx,
		`SELECT attempt, published_at IS NOT NULL, last_error
		   FROM call_finalize_outbox WHERE call_id = $1`, callID,
	).Scan(&attempt, &published, &lastError)
	if err != nil {
		f.t.Fatalf("read back the outbox row: %v", err)
	}
	return attempt, published, lastError
}

// claimClock is the reference instant the claim assertions use.
//
// It has to be anchored to the real clock rather than to a fixed date, because
// call_finalize_outbox.next_attempt_at defaults to the *database's* now(): a fixture
// timestamp in the past would leave every row permanently not-yet-due and every claim
// empty. Truncated to the second so a value that has been through a timestamptz
// column — microsecond precision — compares equal to the one that went in.
func claimClock() time.Time {
	return time.Now().UTC().Truncate(time.Second).Add(time.Second)
}

// --------------------------------------------------------------------- tests

func TestEnqueueingAFinalizeIsIdempotentOnCallId(t *testing.T) {
	// A reconnect that replays call.end must not hand the same call to the pipeline
	// twice: duplicate work costs model tokens against the tenant's budget.
	f := newOutboxFixture(t)
	ctx := context.Background()
	at := claimClock().Add(-time.Hour)

	for i := 0; i < 3; i++ {
		if err := f.enqueue(ctx, outboxCallA, at); err != nil {
			t.Fatalf("enqueue %d: %v", i+1, err)
		}
	}
	var n int
	if err := f.admin.QueryRow(ctx,
		`SELECT count(*) FROM call_finalize_outbox WHERE call_id = $1`, outboxCallA,
	).Scan(&n); err != nil {
		t.Fatal(err)
	}
	if n != 1 {
		t.Fatalf("%d rows for one call", n)
	}
}

func TestClaimingAdvancesTheAttemptAndTheBackoffAtomically(t *testing.T) {
	// The counter and the backoff move when a row is *claimed*, not when a publish
	// fails. That is what makes a crash between claiming and publishing safe: the
	// row is still unpublished, so nothing is lost, and its next attempt is
	// already pushed out, so it does not spin.
	f := newOutboxFixture(t)
	ctx := context.Background()
	now := claimClock()
	if err := f.enqueue(ctx, outboxCallA, now.Add(-time.Minute)); err != nil {
		t.Fatal(err)
	}

	first, err := f.store.ClaimFinalize(ctx, 10, now)
	if err != nil {
		t.Fatalf("claim: %v", err)
	}
	if len(first) != 1 {
		t.Fatalf("claimed %d rows, want 1", len(first))
	}
	if first[0].Attempt != 1 {
		t.Errorf("attempt %d on the first claim, want 1", first[0].Attempt)
	}
	if first[0].TenantID != outboxTenant {
		t.Errorf("tenant %q", first[0].TenantID)
	}
	if !first[0].FinalizedAt.Equal(now.Add(-time.Minute)) {
		t.Errorf("finalized_at %s", first[0].FinalizedAt)
	}

	// Immediately re-claiming at the same instant must find nothing: the backoff
	// has moved the row out of the way.
	again, err := f.store.ClaimFinalize(ctx, 10, now)
	if err != nil {
		t.Fatal(err)
	}
	if len(again) != 0 {
		t.Fatalf("the backoff was not applied; claimed %d rows again", len(again))
	}

	// Past the backoff, it comes back with the attempt incremented.
	later, err := f.store.ClaimFinalize(ctx, 10, now.Add(time.Minute))
	if err != nil {
		t.Fatal(err)
	}
	if len(later) != 1 || later[0].Attempt != 2 {
		t.Fatalf("second claim: %+v", later)
	}
}

func TestABackoffThatGrowsIsStillCappedSoAStuckMessageKeepsBeingRetried(t *testing.T) {
	// No message is ever abandoned: a finalize we stop retrying is a call that is
	// captured, stored, billed and never analysed. The cap is what makes "retried
	// forever" also mean "retried often enough that restarting the broker fixes
	// it".
	f := newOutboxFixture(t)
	ctx := context.Background()
	now := claimClock()
	if err := f.enqueue(ctx, outboxCallA, now); err != nil {
		t.Fatal(err)
	}

	// Claim repeatedly, always jumping far enough forward to be due again, until
	// the attempt count is well past the point where the exponential term exceeds
	// the cap.
	for i := 0; i < 12; i++ {
		claimed, err := f.store.ClaimFinalize(ctx, 10, now.Add(time.Duration(i)*time.Hour))
		if err != nil {
			t.Fatalf("claim %d: %v", i+1, err)
		}
		if len(claimed) != 1 {
			t.Fatalf("claim %d found %d rows", i+1, len(claimed))
		}
	}

	var next time.Time
	if err := f.admin.QueryRow(ctx,
		`SELECT next_attempt_at FROM call_finalize_outbox WHERE call_id = $1`, outboxCallA,
	).Scan(&next); err != nil {
		t.Fatal(err)
	}
	lastClaim := now.Add(11 * time.Hour)
	if gap := next.Sub(lastClaim); gap > 5*time.Minute+time.Second {
		t.Fatalf("the backoff grew to %s; it is meant to cap at five minutes", gap)
	}
}

func TestMarkingPublishedIsIdempotentAndClearsTheLastError(t *testing.T) {
	f := newOutboxFixture(t)
	ctx := context.Background()
	now := claimClock()
	if err := f.enqueue(ctx, outboxCallA, now); err != nil {
		t.Fatal(err)
	}
	if _, err := f.store.ClaimFinalize(ctx, 10, now); err != nil {
		t.Fatal(err)
	}
	if err := f.store.MarkFinalizeFailed(ctx, outboxCallA, "no responders for JetStream"); err != nil {
		t.Fatal(err)
	}
	if _, _, lastError := f.rowState(ctx, outboxCallA); lastError == nil {
		t.Fatal("the failure was not recorded")
	}

	if err := f.store.MarkFinalizePublished(ctx, outboxCallA, now.Add(time.Second)); err != nil {
		t.Fatal(err)
	}
	_, published, lastError := f.rowState(ctx, outboxCallA)
	if !published {
		t.Fatal("the row was not marked published")
	}
	if lastError != nil {
		t.Errorf("last_error %q survived a successful publish", *lastError)
	}

	// A duplicate mark — which is what happens when a publish succeeds, the mark
	// fails, the row is claimed again and republished — must not rewrite the
	// delivery time recorded for the first one.
	var before time.Time
	if err := f.admin.QueryRow(ctx,
		`SELECT published_at FROM call_finalize_outbox WHERE call_id = $1`, outboxCallA,
	).Scan(&before); err != nil {
		t.Fatal(err)
	}
	if err := f.store.MarkFinalizePublished(ctx, outboxCallA, now.Add(time.Hour)); err != nil {
		t.Fatal(err)
	}
	var after time.Time
	if err := f.admin.QueryRow(ctx,
		`SELECT published_at FROM call_finalize_outbox WHERE call_id = $1`, outboxCallA,
	).Scan(&after); err != nil {
		t.Fatal(err)
	}
	if !after.Equal(before) {
		t.Fatalf("published_at moved from %s to %s on a duplicate mark", before, after)
	}
	// And a published row is never claimed again.
	claimed, err := f.store.ClaimFinalize(ctx, 10, now.Add(24*time.Hour))
	if err != nil {
		t.Fatal(err)
	}
	for _, e := range claimed {
		if e.CallID == outboxCallA {
			t.Fatal("a published row was claimed again")
		}
	}
}

func TestFailingAMessageDoesNotAbandonIt(t *testing.T) {
	// sentinel_outbox_failed records the reason and nothing else. It must not set
	// a terminal state, because there is no acceptable terminal state for a
	// finalize that never reached the pipeline.
	f := newOutboxFixture(t)
	ctx := context.Background()
	now := claimClock()
	if err := f.enqueue(ctx, outboxCallA, now); err != nil {
		t.Fatal(err)
	}
	if _, err := f.store.ClaimFinalize(ctx, 10, now); err != nil {
		t.Fatal(err)
	}
	if err := f.store.MarkFinalizeFailed(ctx, outboxCallA, "broker unreachable"); err != nil {
		t.Fatal(err)
	}
	claimed, err := f.store.ClaimFinalize(ctx, 10, now.Add(time.Hour))
	if err != nil {
		t.Fatal(err)
	}
	if len(claimed) != 1 || claimed[0].CallID != outboxCallA {
		t.Fatalf("a failed message was not retried: %+v", claimed)
	}
}

func TestTheDepthGaugeReportsBothPendingAndTheOldestEntry(t *testing.T) {
	// Depth alone is a noisy alarm — a busy floor's queue is briefly non-empty all
	// the time. A queue whose oldest entry is twenty minutes old means the pipeline
	// has stopped receiving work, and that is the one worth waking someone for.
	f := newOutboxFixture(t)
	ctx := context.Background()
	now := claimClock()

	// Whatever else is in the table from other tests, this asserts on the delta
	// and on the oldest entry being no newer than what we just inserted.
	basePending, _, err := f.store.OutboxDepth(ctx)
	if err != nil {
		t.Fatalf("depth: %v", err)
	}

	oldest := now.Add(-30 * time.Minute)
	if err := f.enqueue(ctx, outboxCallA, oldest); err != nil {
		t.Fatal(err)
	}
	if err := f.enqueue(ctx, outboxCallB, now); err != nil {
		t.Fatal(err)
	}

	pending, reportedOldest, err := f.store.OutboxDepth(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if pending != basePending+2 {
		t.Fatalf("pending %d, want %d", pending, basePending+2)
	}
	if reportedOldest.After(oldest) {
		t.Fatalf("oldest reported as %s, want no later than %s", reportedOldest, oldest)
	}

	// Publishing both empties our contribution to the queue.
	for _, id := range []string{outboxCallA, outboxCallB} {
		if err := f.store.MarkFinalizePublished(ctx, id, now); err != nil {
			t.Fatal(err)
		}
	}
	pending, _, err = f.store.OutboxDepth(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if pending != basePending {
		t.Fatalf("pending %d after publishing both, want %d", pending, basePending)
	}
}

func TestTheOutboxIsTenantScopedToEveryNormalQueryPath(t *testing.T) {
	// The drainer crosses tenants through SECURITY DEFINER functions, which is
	// argued for in the migration. Ordinary queries must not: the table is
	// tenant-scoped like everything else, and the application role is NOBYPASSRLS,
	// so a query under one tenant's context sees only that tenant's rows.
	f := newOutboxFixture(t)
	ctx := context.Background()
	now := claimClock()
	if err := f.enqueue(ctx, outboxCallA, now); err != nil {
		t.Fatal(err)
	}

	otherTenant := "32222222-2222-2222-2222-222222222222"
	if _, err := f.admin.Exec(ctx,
		`INSERT INTO tenants (id, name, idp_tenant_id) VALUES ($1,'Other','outbox-other')
		 ON CONFLICT (id) DO NOTHING`, otherTenant); err != nil {
		t.Fatal(err)
	}

	var visible int
	if err := f.store.AsSystem(ctx, otherTenant, func(tx pgx.Tx) error {
		return tx.QueryRow(ctx, `SELECT count(*) FROM call_finalize_outbox`).Scan(&visible)
	}); err != nil {
		t.Fatal(err)
	}
	if visible != 0 {
		t.Fatalf("another tenant's context saw %d outbox rows", visible)
	}
}

func TestEnqueueingForTheWrongTenantIsRefusedByThePolicy(t *testing.T) {
	// The WITH CHECK clause is the half that matters for a write path: a bug that
	// passed the wrong tenant id would otherwise file a call under a tenant that
	// does not own it, and the pipeline would then read it under that tenant's
	// row-level-security context and find nothing.
	f := newOutboxFixture(t)
	ctx := context.Background()
	now := claimClock()

	otherTenant := "32222222-2222-2222-2222-222222222222"
	if _, err := f.admin.Exec(ctx,
		`INSERT INTO tenants (id, name, idp_tenant_id) VALUES ($1,'Other','outbox-other')
		 ON CONFLICT (id) DO NOTHING`, otherTenant); err != nil {
		t.Fatal(err)
	}

	err := f.store.AsSystem(ctx, otherTenant, func(tx pgx.Tx) error {
		// The transaction's context says otherTenant; the row claims
		// outboxTenant.
		return store.EnqueueCallFinalizeTx(ctx, tx, outboxTenant, outboxCallA, now)
	})
	if err == nil {
		t.Fatal("a row was filed under a tenant the transaction context did not name")
	}
}
