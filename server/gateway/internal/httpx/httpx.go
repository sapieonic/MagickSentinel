// Package httpx holds the shared HTTP plumbing: error shapes, request ids and
// structured logging.
//
// The logging rule this package enforces is absolute: **no PII in logs**. Transcript
// text, account references and borrower names must not appear at any level. Handlers
// log identifiers and status, never content.
package httpx

import (
	"bufio"
	"context"
	"encoding/json"
	"net"
	"log/slog"
	"net/http"
	"time"

	"github.com/google/uuid"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"
)

type Error struct {
	Code      string `json:"code"`
	Message   string `json:"message"`
	RequestID string `json:"request_id,omitempty"`
}

// WriteError sends the error shape from contracts/openapi.yaml.
func WriteError(w http.ResponseWriter, r *http.Request, status int, code, message string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(Error{Code: code, Message: message, RequestID: RequestID(r.Context())})
}

// WriteJSON sends a success body.
func WriteJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	if v != nil {
		_ = json.NewEncoder(w).Encode(v)
	}
}

type ctxKey struct{}

func RequestID(ctx context.Context) string {
	id, _ := ctx.Value(ctxKey{}).(string)
	return id
}

// WithRequestID assigns an id to every request so a client error report can be
// traced without asking anyone to quote call content.
func WithRequestID(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		id := r.Header.Get("X-Request-Id")
		if id == "" {
			id = uuid.NewString()
		}
		w.Header().Set("X-Request-Id", id)
		next.ServeHTTP(w, r.WithContext(context.WithValue(r.Context(), ctxKey{}, id)))
	})
}

type statusRecorder struct {
	http.ResponseWriter
	status int
}

func (s *statusRecorder) WriteHeader(code int) {
	s.status = code
	s.ResponseWriter.WriteHeader(code)
}

// The three methods below exist because embedding http.ResponseWriter in a wrapper
// silently *removes* the optional interfaces the real writer implements. Both
// long-lived endpoints this service exists for depend on them: the SSE floor view
// needs Flusher, and the WebSocket upgrade on /v1/ingest needs Hijacker.
//
// The failure is invisible until production. It is a runtime type assertion, not a
// compile error, and it only happens under the full middleware chain — so a handler
// mounted directly in a test passes while the deployed service returns 501 on every
// upgrade. internal/httpx/httpx_test.go mounts the chain for exactly this reason.
//
// Unwrap serves http.ResponseController; Flush and Hijack serve the many libraries
// that still type-assert directly, coder/websocket among them.

func (s *statusRecorder) Unwrap() http.ResponseWriter { return s.ResponseWriter }

func (s *statusRecorder) Flush() {
	if f, ok := s.ResponseWriter.(http.Flusher); ok {
		f.Flush()
	}
}

func (s *statusRecorder) Hijack() (net.Conn, *bufio.ReadWriter, error) {
	h, ok := s.ResponseWriter.(http.Hijacker)
	if !ok {
		return nil, nil, http.ErrNotSupported
	}
	return h.Hijack()
}

// LogRequests emits one structured line per request.
//
// It deliberately logs the route pattern rather than the raw path: a raw path can
// carry an account reference in a query string, and this is a compliance product.
//
// It also carries the trace and span ids when a trace is in progress, which is what
// makes logs and traces the same investigation rather than two. Grafana's Loki-to-
// Tempo correlation is driven off exactly these two fields: an operator reading this
// line about a 500 on /v1/ingest clicks through to the span tree that produced it,
// and from a slow span back to the line. Without the ids in the log, the two are
// joined by hand on a timestamp, which does not work on a floor doing three shifts.
//
// The ids are absent — not empty strings — when no exporter is configured, so a
// deployment with telemetry off gets the same log line it always had.
func LogRequests(log *slog.Logger, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		rec := &statusRecorder{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(rec, r)
		pattern := r.Pattern
		if pattern == "" {
			pattern = "unmatched"
		}

		attrs := []any{
			"method", r.Method,
			"route", pattern,
			"status", rec.status,
			"duration_ms", time.Since(start).Milliseconds(),
			"request_id", RequestID(r.Context()),
		}

		// The span is renamed here rather than by otelhttp's span-name formatter
		// because the matched route pattern does not exist until the mux has
		// routed, which is inside this middleware. Naming the span after the
		// pattern rather than the path is the same decision as logging the
		// pattern: /v1/calls/{id} is one operation, whereas one span name per
		// call id is the unbounded-cardinality mistake described on
		// telemetry.Metrics, applied to traces.
		span := trace.SpanFromContext(r.Context())
		if sc := span.SpanContext(); sc.IsValid() {
			attrs = append(attrs,
				"trace_id", sc.TraceID().String(),
				"span_id", sc.SpanID().String())
		}
		if span.IsRecording() {
			span.SetName(r.Method + " " + pattern)
			// http.route is the semantic-convention key a backend groups by, and
			// the request id ties the span to the log line and to the error body
			// the caller was given. Neither is a route parameter: no call id, no
			// account reference — see the attribute rule on telemetry.Metrics.
			span.SetAttributes(
				attribute.String("http.route", pattern),
				attribute.String("sentinel.request_id", RequestID(r.Context())),
			)
		}

		log.Info("request", attrs...)
	})
}

// Recover turns a panic into a 500 rather than a dropped connection, and logs the
// request id so the crash can be found without the caller's payload.
func Recover(log *slog.Logger, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer func() {
			if v := recover(); v != nil {
				log.Error("panic", "value", v, "request_id", RequestID(r.Context()))
				WriteError(w, r, http.StatusInternalServerError, "internal", "internal error")
			}
		}()
		next.ServeHTTP(w, r)
	})
}


// CORS applies an explicit allow-list of browser origins.
//
// The gateway sets no CORS headers by default, which is correct when the portal is
// served same-origin or behind a reverse proxy and is a deployment surprise
// otherwise. Configuring it is deliberate rather than automatic: a wildcard origin on
// an API that serves borrower call content is not a default anyone should inherit.
//
// Credentials are allowed because the portal sends a bearer token, and a wildcard
// origin is rejected outright for the same reason — the two combined would let any
// page on the internet read a tenant's calls with a stolen token.
func CORS(allowedOrigins []string, next http.Handler) http.Handler {
	allowed := make(map[string]bool, len(allowedOrigins))
	for _, o := range allowedOrigins {
		if o == "*" {
			panic("httpx: a wildcard CORS origin is not permitted on an API serving call content")
		}
		allowed[o] = true
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		origin := r.Header.Get("Origin")
		if origin != "" && allowed[origin] {
			h := w.Header()
			h.Set("Access-Control-Allow-Origin", origin)
			h.Set("Access-Control-Allow-Credentials", "true")
			h.Set("Access-Control-Allow-Headers", "Authorization, Content-Type, X-Request-Id")
			h.Set("Access-Control-Allow-Methods", "GET, POST, PATCH, DELETE, OPTIONS")
			h.Set("Access-Control-Expose-Headers", "X-Request-Id")
			h.Set("Access-Control-Max-Age", "600")
			// The response varies by Origin, so a shared cache must not serve one
			// tenant's portal the headers minted for another's.
			h.Add("Vary", "Origin")
		}
		if r.Method == http.MethodOptions && r.Header.Get("Access-Control-Request-Method") != "" {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		next.ServeHTTP(w, r)
	})
}
