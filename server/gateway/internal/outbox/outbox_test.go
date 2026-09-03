package outbox

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"sync"
	"testing"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/store"
)

// fakeQueue is an in-memory stand-in for the outbox table, including the part that
// matters most: the claim advances the attempt counter and the backoff, so a row
// claimed and not published comes back later rather than immediately.
type fakeQueue struct {
	mu   sync.Mutex
	rows []*row
	// failClaim, failMark and failFail make each of the three database calls fail
	// independently, because the drainer has to survive each one differently.
	failClaim bool
	failMark  bool
	failFail  bool
	claims    int
}

type row struct {
	callID      string
	tenantID    string
	finalizedAt time.Time
	attempt     int
	publishedAt time.Time
	nextAttempt time.Time
	lastError   string
}

func (q *fakeQueue) add(callID, tenantID string, finalizedAt time.Time) {
	q.mu.Lock()
	defer q.mu.Unlock()
	q.rows = append(q.rows, &row{callID: callID, tenantID: tenantID, finalizedAt: finalizedAt})
}

func (q *fakeQueue) ClaimFinalize(_ context.Context, limit int, now time.Time) ([]store.OutboxEntry, error) {
	q.mu.Lock()
	defer q.mu.Unlock()
	// Counted before the failure check, so a test can tell "the drainer stopped
	// trying" from "the drainer tried and the database refused".
	q.claims++
	if q.failClaim {
		return nil, errors.New("database is away")
	}
	var out []store.OutboxEntry
	for _, r := range q.rows {
		if len(out) >= limit {
			break
		}
		if !r.publishedAt.IsZero() || r.nextAttempt.After(now) {
			continue
		}
		r.attempt++
		// The real function applies 2s * 2^attempt capped at five minutes; the
		// shape is what the test cares about, not the curve.
		r.nextAttempt = now.Add(time.Duration(r.attempt) * 2 * time.Second)
		out = append(out, store.OutboxEntry{
			CallID: r.callID, TenantID: r.tenantID,
			FinalizedAt: r.finalizedAt, Attempt: r.attempt,
		})
	}
	return out, nil
}

func (q *fakeQueue) MarkFinalizePublished(_ context.Context, callID string, now time.Time) error {
	q.mu.Lock()
	defer q.mu.Unlock()
	if q.failMark {
		return errors.New("database is away")
	}
	for _, r := range q.rows {
		if r.callID == callID && r.publishedAt.IsZero() {
			r.publishedAt = now
			r.lastError = ""
		}
	}
	return nil
}

func (q *fakeQueue) MarkFinalizeFailed(_ context.Context, callID, reason string) error {
	q.mu.Lock()
	defer q.mu.Unlock()
	if q.failFail {
		return errors.New("database is away")
	}
	for _, r := range q.rows {
		if r.callID == callID {
			r.lastError = reason
		}
	}
	return nil
}

func (q *fakeQueue) pending() int {
	q.mu.Lock()
	defer q.mu.Unlock()
	n := 0
	for _, r := range q.rows {
		if r.publishedAt.IsZero() {
			n++
		}
	}
	return n
}

func (q *fakeQueue) find(callID string) *row {
	q.mu.Lock()
	defer q.mu.Unlock()
	for _, r := range q.rows {
		if r.callID == callID {
			return r
		}
	}
	return nil
}

// fakePublisher records what reached the bus and can be made to reject.
type fakePublisher struct {
	mu       sync.Mutex
	sent     []sentMessage
	failWith error
	// failFirstN rejects the first N publishes and then starts accepting, which is
	// what a broker restart looks like.
	failFirstN int
	attempts   int
}

type sentMessage struct {
	subject  string
	dedupeID string
	payload  []byte
}

func (p *fakePublisher) Publish(_ context.Context, subject, dedupeID string, payload []byte) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.attempts++
	if p.failWith != nil {
		return p.failWith
	}
	if p.attempts <= p.failFirstN {
		return errors.New("no responders available for JetStream")
	}
	p.sent = append(p.sent, sentMessage{subject, dedupeID, append([]byte(nil), payload...)})
	return nil
}

func (p *fakePublisher) Close() {}

func (p *fakePublisher) messages() []sentMessage {
	p.mu.Lock()
	defer p.mu.Unlock()
	return append([]sentMessage(nil), p.sent...)
}

func discardLog() *slog.Logger { return slog.New(slog.NewTextHandler(io.Discard, nil)) }

var (
	t0 = time.Date(2026, 9, 1, 10, 0, 0, 0, time.UTC)
	// The uuid rendering of ULID 01J8ZQ8H2Q7X9K3M4N5P6R7S8T, which is what the
	// calls table holds for a call the client minted: the same 128 bits in the
	// other notation (internal/ingest/sink.go's callUUIDFromULID).
	callUUID = "01923f74-4457-3f53-31d0-952d8d83e51a"
	tenant   = "11111111-1111-1111-1111-111111111111"
)

func newDrainer(q Queue, p Publisher) *Drainer {
	return &Drainer{
		Queue: q, Publisher: p, Log: discardLog(),
		Batch: 4, Now: func() time.Time { return t0 },
	}
}

// ------------------------------------------------------------------- payload

func TestThePublishedPayloadCarriesExactlyTheFourAgreedFields(t *testing.T) {
	// The payload is the contract with server/pipeline/sentinel_pipeline/consumer.py
	// and nothing may be added to it: no transcript, no audio, no borrower data on
	// the bus. This test fails if a field is added, which is the point.
	q := &fakeQueue{}
	q.add(callUUID, tenant, t0.Add(-90*time.Second))
	p := &fakePublisher{}
	if _, err := newDrainer(q, p).DrainOnce(context.Background()); err != nil {
		t.Fatal(err)
	}

	msgs := p.messages()
	if len(msgs) != 1 {
		t.Fatalf("published %d messages, want 1", len(msgs))
	}
	if msgs[0].subject != "sentinel.call.finalize" {
		t.Fatalf("subject %q", msgs[0].subject)
	}

	var raw map[string]any
	if err := json.Unmarshal(msgs[0].payload, &raw); err != nil {
		t.Fatalf("payload is not JSON: %v", err)
	}
	want := map[string]any{
		// The ULID rendering, not the uuid one: this is the identifier the client
		// minted and the identifier a supervisor can search for.
		"call_id":      "01J8ZQ8H2Q7X9K3M4N5P6R7S8T",
		"tenant_id":    tenant,
		"attempt":      float64(1),
		"finalized_at": t0.Add(-90 * time.Second).Format(time.RFC3339),
	}
	if len(raw) != len(want) {
		t.Fatalf("payload has %d fields, want exactly %d: %s", len(raw), len(want), msgs[0].payload)
	}
	for k, v := range want {
		if raw[k] != v {
			t.Errorf("%s = %v, want %v", k, raw[k], v)
		}
	}
	// And the deduplication key is the call id, so a republish inside the stream's
	// duplicate window collapses rather than double-analysing a call.
	if msgs[0].dedupeID != "01J8ZQ8H2Q7X9K3M4N5P6R7S8T" {
		t.Fatalf("dedupe id %q", msgs[0].dedupeID)
	}
}

func TestAnUnusableCallIdIsSkippedRatherThanWedgingTheQueueBehindIt(t *testing.T) {
	// Cannot happen — the column is uuid and ingest wrote it — but a row that can
	// never be encoded must not be retried forever in front of every other call on
	// the floor.
	q := &fakeQueue{}
	q.add("not-a-uuid", tenant, t0)
	q.add(callUUID, tenant, t0)
	p := &fakePublisher{}
	if _, err := newDrainer(q, p).DrainOnce(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := len(p.messages()); got != 1 {
		t.Fatalf("published %d messages, want the one good row", got)
	}
	if r := q.find("not-a-uuid"); r.lastError == "" {
		t.Fatal("the unusable row was not marked")
	}
}

// -------------------------------------------------------------------- draining

func TestDrainOnceBehaviour(t *testing.T) {
	cases := []struct {
		name string
		// setup arranges the queue and publisher.
		setup func(*fakeQueue, *fakePublisher)
		// wantClaimed is DrainOnce's return: how many rows it took, which is what
		// the caller uses to decide whether more work is waiting.
		wantClaimed int
		wantSent    int
		wantPending int
		wantErr     bool
	}{
		{
			name:  "an empty queue is a no-op",
			setup: func(*fakeQueue, *fakePublisher) {},
		},
		{
			name: "a full batch is published and marked",
			setup: func(q *fakeQueue, _ *fakePublisher) {
				for i := 0; i < 3; i++ {
					q.add(callIDN(i), tenant, t0)
				}
			},
			wantClaimed: 3, wantSent: 3, wantPending: 0,
		},
		{
			name: "the claim is capped at the batch size so one pass cannot starve the loop",
			setup: func(q *fakeQueue, _ *fakePublisher) {
				for i := 0; i < 10; i++ {
					q.add(callIDN(i), tenant, t0)
				}
			},
			wantClaimed: 4, wantSent: 4, wantPending: 6,
		},
		{
			name: "a broker that rejects everything leaves every row pending",
			setup: func(q *fakeQueue, p *fakePublisher) {
				q.add(callUUID, tenant, t0)
				p.failWith = errors.New("broker unreachable")
			},
			wantClaimed: 1, wantSent: 0, wantPending: 1,
		},
		{
			name: "one bad row does not stop the rest of the batch",
			setup: func(q *fakeQueue, p *fakePublisher) {
				q.add(callIDN(0), tenant, t0)
				q.add(callIDN(1), tenant, t0)
				q.add(callIDN(2), tenant, t0)
				p.failFirstN = 1
			},
			wantClaimed: 3, wantSent: 2, wantPending: 1,
		},
		{
			name: "a database that cannot be claimed from is an error, not a silent success",
			setup: func(q *fakeQueue, _ *fakePublisher) {
				q.add(callUUID, tenant, t0)
				q.failClaim = true
			},
			wantErr: true,
		},
		{
			name: "a publish that succeeds and cannot be marked leaves the row for a retry",
			setup: func(q *fakeQueue, _ *fakePublisher) {
				q.add(callUUID, tenant, t0)
				q.failMark = true
			},
			// The message did reach the broker; the row is still pending, so it
			// will be published again. At-least-once, and the safe direction.
			wantClaimed: 1, wantSent: 1, wantPending: 1,
		},
		{
			name: "a failure that cannot even be recorded does not crash the drain",
			setup: func(q *fakeQueue, p *fakePublisher) {
				q.add(callUUID, tenant, t0)
				q.failFail = true
				p.failWith = errors.New("broker unreachable")
			},
			wantClaimed: 1, wantSent: 0, wantPending: 1,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			q, p := &fakeQueue{}, &fakePublisher{}
			tc.setup(q, p)
			claimed, err := newDrainer(q, p).DrainOnce(context.Background())
			if tc.wantErr {
				if err == nil {
					t.Fatal("expected an error")
				}
				return
			}
			if err != nil {
				t.Fatalf("drain: %v", err)
			}
			if claimed != tc.wantClaimed {
				t.Errorf("claimed %d, want %d", claimed, tc.wantClaimed)
			}
			if got := len(p.messages()); got != tc.wantSent {
				t.Errorf("published %d, want %d", got, tc.wantSent)
			}
			if got := q.pending(); got != tc.wantPending {
				t.Errorf("%d rows still pending, want %d", got, tc.wantPending)
			}
		})
	}
}

func TestAClaimedRowCarriesItsAttemptNumberIntoThePayload(t *testing.T) {
	// A consumer that sees attempt > 1 knows the handover was delayed on our side,
	// which is otherwise indistinguishable from a call that simply ran long.
	q := &fakeQueue{}
	q.add(callUUID, tenant, t0)
	p := &fakePublisher{failWith: errors.New("broker unreachable")}
	d := newDrainer(q, p)

	// First pass fails. The backoff has moved the row out of the way, so the
	// second pass has to be at a later clock reading to pick it up — which is the
	// backoff working.
	if _, err := d.DrainOnce(context.Background()); err != nil {
		t.Fatal(err)
	}
	if _, err := d.DrainOnce(context.Background()); err != nil {
		t.Fatal(err)
	}
	if r := q.find(callUUID); r.attempt != 1 {
		t.Fatalf("attempt %d after a retry at the same instant; the backoff was not applied", r.attempt)
	}

	p.failWith = nil
	d.Now = func() time.Time { return t0.Add(time.Minute) }
	if _, err := d.DrainOnce(context.Background()); err != nil {
		t.Fatal(err)
	}
	msgs := p.messages()
	if len(msgs) != 1 {
		t.Fatalf("published %d messages, want 1", len(msgs))
	}
	var got struct {
		Attempt int `json:"attempt"`
	}
	if err := json.Unmarshal(msgs[0].payload, &got); err != nil {
		t.Fatal(err)
	}
	if got.Attempt != 2 {
		t.Fatalf("attempt %d in the payload, want 2", got.Attempt)
	}
}

func TestABacklogIsDrainedAcrossPassesRatherThanOneBatchPerTick(t *testing.T) {
	// What a broker outage leaves behind: more rows than one batch. The loop has to
	// keep going while passes come back full, or a floor that was offline for a
	// shift catches up at four calls every two seconds.
	q := &fakeQueue{}
	for i := 0; i < 10; i++ {
		q.add(callIDN(i), tenant, t0)
	}
	p := &fakePublisher{}
	d := newDrainer(q, p)
	for {
		n, err := d.DrainOnce(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		if n < d.batch() {
			break
		}
	}
	if q.pending() != 0 {
		t.Fatalf("%d rows left pending after draining to a short batch", q.pending())
	}
	if got := len(p.messages()); got != 10 {
		t.Fatalf("published %d, want 10", got)
	}
}

func TestRunStopsWhenTheContextIsCancelledAndNothingIsLost(t *testing.T) {
	q := &fakeQueue{}
	q.add(callUUID, tenant, t0)
	p := &fakePublisher{}
	d := newDrainer(q, p)
	d.Interval = time.Millisecond

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		d.Run(ctx)
		close(done)
	}()

	deadline := time.After(2 * time.Second)
	for q.pending() != 0 {
		select {
		case <-deadline:
			t.Fatal("the drainer never published the queued row")
		default:
			time.Sleep(time.Millisecond)
		}
	}
	cancel()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("Run did not return on cancellation")
	}
}

func TestRunKeepsGoingAfterADatabaseFailure(t *testing.T) {
	// A drainer that exited on error would go back to accumulating calls that are
	// captured and never analysed, which is the whole failure this package exists
	// to prevent. It has to survive the database being away.
	q := &fakeQueue{failClaim: true}
	q.add(callUUID, tenant, t0)
	p := &fakePublisher{}
	d := newDrainer(q, p)
	d.Interval = time.Millisecond

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go d.Run(ctx)

	// Let it fail a few times, then let the database come back.
	deadline := time.After(2 * time.Second)
	for {
		q.mu.Lock()
		claims := q.claims
		q.mu.Unlock()
		if claims >= 2 {
			break
		}
		select {
		case <-deadline:
			t.Fatal("the drainer stopped claiming after the first failure")
		default:
			time.Sleep(time.Millisecond)
		}
	}
	q.mu.Lock()
	q.failClaim = false
	q.mu.Unlock()

	deadline = time.After(2 * time.Second)
	for q.pending() != 0 {
		select {
		case <-deadline:
			t.Fatal("the drainer did not recover once the database returned")
		default:
			time.Sleep(time.Millisecond)
		}
	}
}

// callIDN builds distinct, valid uuid renderings of call ids.
func callIDN(i int) string {
	const hex = "0123456789abcdef"
	return "01923f74-4457-3f53-31d0-952d8d" + string([]byte{
		hex[(i>>4)&0xf], hex[i&0xf],
	}) + "83e51a"[2:]
}

// The constants have to match the Python consumer's, because the two halves are in
// different languages and nothing else checks.
func TestTheStreamAndSubjectMatchTheConsumer(t *testing.T) {
	if Stream != "SENTINEL" {
		t.Errorf("stream %q; consumer.py declares STREAM = \"SENTINEL\"", Stream)
	}
	if Subject != "sentinel.call.finalize" {
		t.Errorf("subject %q; consumer.py declares SUBJECT_FINALIZE", Subject)
	}
	// The stream filter has to cover sentinel.call.dlq as well, because
	// consumer.py republishes poison messages there through the same JetStream
	// context.
	if SubjectFilter != "sentinel.call.>" {
		t.Errorf("subject filter %q does not cover the dead-letter subject", SubjectFilter)
	}
}
