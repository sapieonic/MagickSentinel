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
func LogRequests(log *slog.Logger, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		rec := &statusRecorder{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(rec, r)
		pattern := r.Pattern
		if pattern == "" {
			pattern = "unmatched"
		}
		log.Info("request",
			"method", r.Method,
			"route", pattern,
			"status", rec.status,
			"duration_ms", time.Since(start).Milliseconds(),
			"request_id", RequestID(r.Context()),
		)
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
