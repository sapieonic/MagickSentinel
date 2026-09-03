package telemetry

import (
	"context"
	"time"

	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/metric"
)

// Metrics holds the gateway's instruments.
//
// # What may be a label here, and what may never be
//
// This is the rule, and it is a compliance constraint rather than a cardinality
// preference — though it is both.
//
// **Allowed:** tenant_id and device_id. A tenant is a BPO customer; there are tens of
// them, they are the unit every operational question is asked in ("is Acme's floor
// capturing?"), and the id is already in the certificate and the token. A device is a
// desktop; there are a few hundred per tenant, and "which machine stopped uploading"
// is the question tamper detection exists to answer. Both are bounded by things we
// sell and ship.
//
// **Never:** a user UID, an agent's name, a call id, an account reference, a
// borrower's phone number, a transcript fragment, a flag's evidence text, or a
// disposition attached to any of those. Two independent reasons, and either alone is
// disqualifying:
//
//  1. Cardinality. A call id is unbounded — one new label value per call, forever.
//     Prometheus and every OTLP backend behind it store one time series per distinct
//     label set, so a call-id label turns a handful of series into millions and takes
//     the metrics store down. This is the failure that gets discovered at 3 a.m. two
//     months after someone added "just the call id, for debugging".
//  2. It is PII in a system with no retention policy. AGENTS.md states the rule for
//     logs — no transcripts, account references or borrower names, at any level — and
//     a metrics label is a log line that is kept forever, replicated to a monitoring
//     vendor, and visible to everyone with a Grafana account. A borrower's account
//     reference in a metric label has left the retention regime entirely: the nightly
//     purge in server/pipeline/sentinel_pipeline/retention.py deletes rows and
//     objects, and it has no idea the value is also sitting in a time-series
//     database.
//
// Spans are subject to the same rule with one addition: a span may carry a call id as
// an attribute, because a span is a sampled, retention-bounded record of one request
// rather than a dimension of an aggregate, and correlating a failed ingest to a call
// is the whole point of having traces. It must still never carry transcript text, an
// account reference or a borrower name.
//
// Every method is safe on a nil receiver, so a Server or a Sink constructed without
// metrics — which is how the unit tests construct them — needs no guards.
type Metrics struct {
	ingestFrames   metric.Int64Counter
	ingestSegments metric.Int64Counter
	ingestBytes    metric.Int64Counter
	ingestLag      metric.Float64Histogram
	callsFinalized metric.Int64Counter

	outboxPublished metric.Int64Counter
	outboxFailures  metric.Int64Counter
	outboxPending   metric.Int64ObservableGauge
	outboxOldest    metric.Float64ObservableGauge

	enrollments metric.Int64Counter
	authRejects metric.Int64Counter
	tokenGrants metric.Int64Counter

	// depthSource is what the observable gauges read. Registered separately from
	// construction because the store is not available when the instruments are.
	depthSource func(context.Context) (pending int64, oldest time.Time, err error)
}

// Attribute keys. Named constants so a call site cannot invent a second spelling of
// tenant_id and split a dashboard in half.
const (
	attrTenant  = attribute.Key("tenant_id")
	attrChannel = attribute.Key("channel")
	attrOutcome = attribute.Key("outcome")
	attrReason  = attribute.Key("reason")
	attrGrant   = attribute.Key("grant_type")
)

// NewMetrics builds the instruments off the global meter provider.
//
// Called unconditionally, including when telemetry is disabled: the global provider is
// then the API's no-op, every instrument is a discard, and the call sites do not have
// to care. Instrument creation cannot fail in a way worth propagating — the API
// returns a working no-op instrument alongside any error — so the errors are dropped
// rather than turned into a startup failure over a metric name.
func NewMetrics() *Metrics {
	m := otel.Meter(ServiceName)
	out := &Metrics{}

	out.ingestFrames, _ = m.Int64Counter("sentinel.ingest.frames",
		metric.WithDescription("Media frames accepted on the ingest socket."),
		metric.WithUnit("{frame}"))
	out.ingestSegments, _ = m.Int64Counter("sentinel.ingest.segments",
		metric.WithDescription("Audio segments durably stored, per channel."),
		metric.WithUnit("{segment}"))
	out.ingestBytes, _ = m.Int64Counter("sentinel.ingest.bytes",
		metric.WithDescription("Encoded audio bytes durably stored."),
		metric.WithUnit("By"))
	out.ingestLag, _ = m.Float64Histogram("sentinel.ingest.lag",
		metric.WithDescription("Wall-clock age of a segment when it was stored: "+
			"how far the floor is behind live. Dominated by spool backlog draining "+
			"after an outage."),
		metric.WithUnit("s"),
		// Buckets chosen around what the numbers mean rather than around powers of
		// two: under 5 s is live, a minute is a hiccup, an hour is a desktop that
		// was offline for a shift and is now catching up, and the 72-hour spool
		// bound (client/sentinel-core/src/spool.rs) is the ceiling.
		metric.WithExplicitBucketBoundaries(1, 5, 15, 60, 300, 1800, 7200, 86400, 259200))
	out.callsFinalized, _ = m.Int64Counter("sentinel.ingest.calls_finalized",
		metric.WithDescription("Calls that reached call.end and were queued for the pipeline."),
		metric.WithUnit("{call}"))

	out.outboxPublished, _ = m.Int64Counter("sentinel.outbox.published",
		metric.WithDescription("Finalize messages acknowledged by JetStream."),
		metric.WithUnit("{message}"))
	out.outboxFailures, _ = m.Int64Counter("sentinel.outbox.failures",
		metric.WithDescription("Finalize publish attempts that failed and will be retried."),
		metric.WithUnit("{attempt}"))

	// The two numbers that say "the pipeline has stopped receiving work". Depth
	// alone is noisy — a busy floor's queue is briefly non-empty constantly — so
	// the age of the oldest unpublished entry is exported alongside it, and that is
	// the one to alarm on.
	out.outboxPending, _ = m.Int64ObservableGauge("sentinel.outbox.pending",
		metric.WithDescription("Finalize messages queued and not yet published."),
		metric.WithUnit("{message}"))
	out.outboxOldest, _ = m.Float64ObservableGauge("sentinel.outbox.oldest_age",
		metric.WithDescription("Age of the oldest unpublished finalize message. "+
			"Non-zero and growing means calls are being captured and never analysed."),
		metric.WithUnit("s"))

	out.enrollments, _ = m.Int64Counter("sentinel.enrollment.attempts",
		metric.WithDescription("Device enrollment attempts by outcome."),
		metric.WithUnit("{attempt}"))
	out.authRejects, _ = m.Int64Counter("sentinel.auth.rejections",
		metric.WithDescription("Requests refused by the authentication middleware, by reason."),
		metric.WithUnit("{request}"))
	out.tokenGrants, _ = m.Int64Counter("sentinel.oauth.token_requests",
		metric.WithDescription("Token endpoint requests by grant type and outcome."),
		metric.WithUnit("{request}"))

	// Register the callback once, reading through whatever depthSource is set at
	// collection time. Registering here rather than in RegisterOutboxDepth keeps
	// the instrument's lifetime tied to the Metrics value.
	if out.outboxPending != nil && out.outboxOldest != nil {
		_, _ = m.RegisterCallback(out.observeOutbox, out.outboxPending, out.outboxOldest)
	}
	return out
}

// RegisterOutboxDepth supplies the query the outbox gauges read on each collection.
//
// A gauge rather than a counter maintained by the drainer, because the number that
// matters is the one in the database: a drainer that has crashed reports nothing, and
// "no data" and "queue empty" must not look the same on a dashboard.
func (m *Metrics) RegisterOutboxDepth(fn func(context.Context) (int64, time.Time, error)) {
	if m == nil {
		return
	}
	m.depthSource = fn
}

func (m *Metrics) observeOutbox(ctx context.Context, o metric.Observer) error {
	if m.depthSource == nil {
		return nil
	}
	pending, oldest, err := m.depthSource(ctx)
	if err != nil {
		// Swallowed rather than returned: a returned error is logged by the SDK on
		// every collection interval, and a database blip would fill the log with
		// it. The absence of the series is itself the signal.
		return nil
	}
	o.ObserveInt64(m.outboxPending, pending)
	age := 0.0
	if !oldest.IsZero() {
		age = time.Since(oldest).Seconds()
	}
	o.ObserveFloat64(m.outboxOldest, age)
	return nil
}

// ---------------------------------------------------------------- recorders

// IngestFrames counts media frames pulled off one socket. The tenant is the only
// dimension: per-device frame rate is a heartbeat concern, and adding device_id here
// would multiply the series count by the size of the fleet for no question anyone
// asks of this counter.
func (m *Metrics) IngestFrames(ctx context.Context, tenantID string, n int64) {
	if m == nil || m.ingestFrames == nil || n == 0 {
		return
	}
	m.ingestFrames.Add(ctx, n, metric.WithAttributes(attrTenant.String(tenantID)))
}

// IngestSegment records one durably stored segment.
//
// Channel is a bounded label with exactly two values, and it is worth having: the two
// channels are captured by different Windows APIs (render loopback and microphone
// capture), so one channel arriving and the other not is a real and diagnosable
// failure — and it is invisible in a combined count.
func (m *Metrics) IngestSegment(ctx context.Context, tenantID string, channel uint8, bytes int) {
	if m == nil {
		return
	}
	attrs := metric.WithAttributes(attrTenant.String(tenantID), attrChannel.Int(int(channel)))
	if m.ingestSegments != nil {
		m.ingestSegments.Add(ctx, 1, attrs)
	}
	if m.ingestBytes != nil && bytes > 0 {
		m.ingestBytes.Add(ctx, int64(bytes), attrs)
	}
}

// IngestLag records how far behind live a stored segment was. Negative values are
// clamped: they mean the endpoint's clock is ahead of the gateway's, which is common
// on desktops that are not time-synced and is not a lag of minus four seconds.
func (m *Metrics) IngestLag(ctx context.Context, tenantID string, lag time.Duration) {
	if m == nil || m.ingestLag == nil {
		return
	}
	if lag < 0 {
		lag = 0
	}
	m.ingestLag.Record(ctx, lag.Seconds(), metric.WithAttributes(attrTenant.String(tenantID)))
}

// CallFinalized counts calls handed to the pipeline. Compare against
// sentinel.outbox.published to see whether the handover is keeping up.
func (m *Metrics) CallFinalized(ctx context.Context, tenantID string) {
	if m == nil || m.callsFinalized == nil {
		return
	}
	m.callsFinalized.Add(ctx, 1, metric.WithAttributes(attrTenant.String(tenantID)))
}

// OutboxPublished counts acknowledged messages. Deliberately carries no tenant
// label: the drainer is one queue for the whole deployment, its health is a
// deployment-level property, and a per-tenant breakdown would invite someone to alarm
// per tenant on a number that is not per tenant.
func (m *Metrics) OutboxPublished(ctx context.Context, n int64) {
	if m == nil || m.outboxPublished == nil || n == 0 {
		return
	}
	m.outboxPublished.Add(ctx, n)
}

// OutboxFailure counts a failed publish attempt. reason is a small closed set —
// "publish", "mark", "encode" — never the broker's error string, which is unbounded
// and can contain a hostname or a certificate subject.
func (m *Metrics) OutboxFailure(ctx context.Context, reason string) {
	if m == nil || m.outboxFailures == nil {
		return
	}
	m.outboxFailures.Add(ctx, 1, metric.WithAttributes(attrReason.String(reason)))
}

// Enrollment records an enrollment attempt.
//
// No tenant label, and that is not an oversight: enrollment happens before any
// identity is established, so the only tenant available is one an unauthenticated
// caller supplied — which is both untrustworthy and, because the enrollment token is
// what names the tenant, a way for an attacker to create arbitrary label values by
// guessing tokens. outcome is a closed set of this handler's own refusal codes.
func (m *Metrics) Enrollment(ctx context.Context, outcome string) {
	if m == nil || m.enrollments == nil {
		return
	}
	m.enrollments.Add(ctx, 1, metric.WithAttributes(attrOutcome.String(outcome)))
}

// AuthRejection records a refused request by reason.
//
// The reasons are the middleware's own distinctions — no_token, token_invalid,
// device_unknown, device_revoked, tenant_mismatch, device_required — and telling them
// apart is what makes this useful: a spike in tenant_mismatch is an attack or a
// misprovisioned floor, a spike in token_invalid is usually an IdP problem, and a
// spike in device_revoked is a revocation that someone forgot to tell the floor
// about. There is no tenant label because a rejected request has no verified tenant,
// and using the unverified one would let a caller mint label values at will.
func (m *Metrics) AuthRejection(ctx context.Context, reason string) {
	if m == nil || m.authRejects == nil {
		return
	}
	m.authRejects.Add(ctx, 1, metric.WithAttributes(attrReason.String(reason)))
}

// TokenGrant records a token-endpoint request. grantType is one of the two grants the
// endpoint supports plus "unsupported"; outcome is a closed set of RFC 6749 error
// codes plus "issued". No client id, no subject, no refresh token prefix.
func (m *Metrics) TokenGrant(ctx context.Context, grantType, outcome string) {
	if m == nil || m.tokenGrants == nil {
		return
	}
	m.tokenGrants.Add(ctx, 1, metric.WithAttributes(
		attrGrant.String(grantType), attrOutcome.String(outcome)))
}
