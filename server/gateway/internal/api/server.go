// Package api serves the REST surface described by contracts/openapi.yaml.
package api

import (
	"context"
	"log/slog"
	"net/http"
	"time"

	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/httpx"
	"github.com/magickvoice/sentinel/server/gateway/internal/store"
	"github.com/magickvoice/sentinel/server/gateway/internal/telemetry"
)

type Server struct {
	Log      *slog.Logger
	Store    *store.Store
	Verifier *auth.Verifier
	Version  string
	// Ingest is mounted at /v1/ingest when set.
	Ingest http.Handler
	// CA signs device certificates at enrollment.
	CA CertificateAuthority
	// LiveTickets backs the SSE floor view. Nil disables the live routes.
	LiveTickets *LiveTickets
	// LivePoll is how often the floor view re-queries active calls.
	LivePoll time.Duration
	// AllowedOrigins enables CORS for exactly these browser origins. Empty means
	// no CORS headers, which is right when the portal is served same-origin.
	AllowedOrigins []string
	// TokenBroker backs POST /v1/oauth/token. Nil leaves the route mounted and
	// answering temporarily_unavailable, because a desktop that gets a 404 there
	// concludes the API base URL is wrong and stops, whereas one that gets a 503
	// retries and reports something an operator can act on.
	TokenBroker TokenBroker
	// TokenLimiter bounds the token endpoint, which sits outside Authenticate and
	// is therefore reachable by anyone who can resolve the hostname. Nil means no
	// limit, which is only right in tests.
	TokenLimiter *httpx.RateLimiter
	// Readiness holds the dependency probes GET /readyz runs.
	Readiness []ReadyCheck
	// Metrics may be nil; every recorder tolerates it.
	Metrics *telemetry.Metrics
	Now     func() time.Time
}

func (s *Server) now() time.Time {
	if s.Now != nil {
		return s.Now()
	}
	return time.Now()
}

// ReadyCheck is one dependency probe for GET /readyz.
type ReadyCheck struct {
	// Name appears in the response body. It must not be a connection string: the
	// endpoint is unauthenticated.
	Name  string
	Check func(context.Context) error
}

// readyTimeout bounds the whole readiness check. Shorter than any sensible probe
// interval, because a readiness endpoint that hangs is worse than one that fails:
// Kubernetes treats a timeout as a failure anyway, and a slow one just delays the
// eviction.
const readyTimeout = 3 * time.Second

// Routes builds the mux. Every route except /healthz, /readyz, enrollment and the
// token endpoint sits behind Authenticate; capability gates are applied per route
// from the matrix in spec section 13.4.
func (s *Server) Routes() http.Handler {
	mux := http.NewServeMux()

	// /healthz answers as long as the process is alive, and deliberately checks
	// nothing. It is the liveness probe: a gateway whose database is down should be
	// left alone to recover, not killed and restarted into the same dead database.
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		httpx.WriteJSON(w, http.StatusOK, map[string]string{"status": "ok", "version": s.Version})
	})
	// /readyz is the other question: should this instance be sent traffic.
	mux.HandleFunc("GET /readyz", s.readyz)

	// Enrollment is the one authenticated-by-token-only path: the device has no
	// certificate yet, which is the point of the exchange.
	mux.Handle("POST /v1/devices/enroll", http.HandlerFunc(s.enrollDevice))

	// The desktop's OAuth token endpoint. Outside Authenticate because there is no
	// token yet — this is where one comes from — and rate-limited for exactly that
	// reason.
	mux.Handle("POST /v1/oauth/token",
		httpx.RateLimit(s.TokenLimiter, 5*time.Second, http.HandlerFunc(s.tokenEndpoint)))

	authed := func(h http.HandlerFunc) http.Handler {
		return s.Authenticate(AssertMeNamespace(h))
	}
	device := func(h http.HandlerFunc) http.Handler {
		return s.Authenticate(s.requireDevice(h))
	}
	gated := func(c auth.Capability, h http.HandlerFunc) http.Handler {
		return s.Authenticate(auth.Require(c, AssertMeNamespace(h)))
	}

	// The call explorer. Scope comes from row-level security, not from the route,
	// so one pair of handlers serves every role.
	mux.Handle("GET /v1/calls", authed(s.listCalls))
	mux.Handle("GET /v1/calls/{id}", authed(s.getCall))
	mux.Handle("GET /v1/teams", authed(s.listTeams))

	// Session and device.
	mux.Handle("POST /v1/sessions", authed(s.createSession))
	mux.Handle("DELETE /v1/sessions/current", authed(s.endSession))
	mux.Handle("GET /v1/policy", device(s.getPolicy))
	mux.Handle("POST /v1/heartbeat", device(s.heartbeat))

	// Agent self-service.
	mux.Handle("GET /v1/me/calls", authed(s.listMyCalls))
	mux.Handle("GET /v1/me/calls/{id}", authed(s.getMyCall))
	mux.Handle("POST /v1/me/calls/{id}/confirm", authed(s.confirmMyCall))
	mux.Handle("GET /v1/me/stats", authed(s.myStats))
	mux.Handle("GET /v1/me/flags", authed(s.myFlags))
	mux.Handle("POST /v1/me/flags/{id}/respond", authed(s.respondToMyFlag))

	// Team.
	mux.Handle("GET /v1/teams/{id}/calls", gated(auth.CapTeamCalls, s.listTeamCalls))
	mux.Handle("GET /v1/teams/{id}/scorecards", gated(auth.CapTeamCalls, s.teamScorecards))
	mux.Handle("POST /v1/teams/{id}/live/ticket", gated(auth.CapTeamCalls, s.createLiveTicket))
	// Deliberately outside Authenticate: the ticket is the credential, and it
	// carries the identity that was verified when it was minted.
	mux.Handle("GET /v1/teams/{id}/live", http.HandlerFunc(s.streamTeamLive))

	// Compliance.
	mux.Handle("GET /v1/compliance/flags", gated(auth.CapResolveFlags, s.listFlags))
	mux.Handle("PATCH /v1/compliance/flags/{id}", gated(auth.CapResolveFlags, s.updateFlag))
	mux.Handle("POST /v1/compliance/exports", gated(auth.CapResolveFlags, s.createEvidenceExport))

	// Admin.
	mux.Handle("GET /v1/admin/devices", gated(auth.CapManageFleet, s.listDevices))
	mux.Handle("POST /v1/admin/devices/{id}/revoke", gated(auth.CapManageFleet, s.revokeDevice))
	mux.Handle("POST /v1/admin/enrollment-tokens", gated(auth.CapManageFleet, s.createEnrollmentToken))
	mux.Handle("GET /v1/admin/users", gated(auth.CapManageFleet, s.listUsers))
	mux.Handle("PATCH /v1/admin/users/{uid}", gated(auth.CapManageFleet, s.updateUser))
	mux.Handle("GET /v1/admin/rules", gated(auth.CapEditRules, s.getRules))
	mux.Handle("PUT /v1/admin/rules", gated(auth.CapEditRules, s.putRules))
	mux.Handle("GET /v1/admin/audit", gated(auth.CapManageFleet, s.auditLog))

	if s.Ingest != nil {
		mux.Handle("/v1/ingest", s.Authenticate(s.requireDevice(s.Ingest)))
	}

	var handler http.Handler = mux
	if len(s.AllowedOrigins) > 0 {
		handler = httpx.CORS(s.AllowedOrigins, handler)
	}
	handler = httpx.WithRequestID(httpx.Recover(s.Log, httpx.LogRequests(s.Log, handler)))

	// otelhttp goes outermost, so the span covers the middleware as well as the
	// handler and so httpx.LogRequests — which runs inside it — can put the trace
	// and span ids on its log line and rename the span to the matched route.
	//
	// Wrapping the whole chain in another ResponseWriter is the move the comment in
	// internal/httpx/httpx.go warns about: a wrapper that hides Flusher breaks the
	// SSE floor view, and one that hides Hijacker breaks the WebSocket upgrade on
	// /v1/ingest, invisibly, in production only. otelhttp is safe because it wraps
	// with httpsnoop, which regenerates whichever optional interfaces the writer
	// underneath actually implements — and
	// internal/httpx/httpx_test.go's chain tests assert exactly that, through this
	// wrapper, so a version bump that regressed it would fail the build rather than
	// the floor.
	//
	// When telemetry is disabled the global tracer provider is the API's no-op, so
	// this costs a nil-span allocation per request and nothing else.
	return otelhttp.NewHandler(handler, "sentinel-gateway",
		// The route is not known until the mux has matched, which happens well
		// inside this handler, so the initial span name is generic and
		// LogRequests renames it. Suppressing the default naming here avoids
		// producing one span name per distinct URL path — which for
		// /v1/calls/{id} would be one per call, the unbounded-cardinality
		// mistake the comment on telemetry.Metrics describes.
		otelhttp.WithSpanNameFormatter(func(string, *http.Request) string {
			return "http.request"
		}),
	)
}

// readyz reports whether this instance should be sent traffic.
//
// Distinct from /healthz, and the distinction is the point. Liveness asks "is the
// process wedged"; readiness asks "can it actually do the job". The job includes
// accepting ingest, and a gateway that accepts ingest with a dead object store
// answers the WebSocket, takes the audio, fails the blob write and loses the call —
// which is the one failure this product cannot absorb, and it is silent from the
// desktop's point of view because the segment simply goes unacked and stays in the
// spool until the 72-hour bound evicts it. So the probe checks the dependencies
// rather than returning a constant.
//
// The body names each check and says only "ok" or "failed". No error strings: this
// endpoint is unauthenticated by necessity — a kubelet cannot present a bearer token
// — and a database error message carries the DSN, the host, sometimes the role name.
// The detail goes to the log, where the request id ties the two together.
func (s *Server) readyz(w http.ResponseWriter, r *http.Request) {
	ctx, cancel := context.WithTimeout(r.Context(), readyTimeout)
	defer cancel()

	results := make(map[string]string, len(s.Readiness))
	ready := true
	for _, c := range s.Readiness {
		if c.Check == nil {
			continue
		}
		if err := c.Check(ctx); err != nil {
			ready = false
			results[c.Name] = "failed"
			s.Log.Error("readiness check failed", "check", c.Name, "error", err,
				"request_id", httpx.RequestID(r.Context()))
			continue
		}
		results[c.Name] = "ok"
	}

	status := http.StatusOK
	state := "ready"
	if !ready {
		// 503 rather than 500: this is a statement about this instance's
		// availability, and every orchestrator and load balancer reads it as
		// "stop sending traffic here" rather than "something is broken".
		status = http.StatusServiceUnavailable
		state = "not_ready"
	}
	httpx.WriteJSON(w, status, map[string]any{
		"status": state, "version": s.Version, "checks": results,
	})
}
