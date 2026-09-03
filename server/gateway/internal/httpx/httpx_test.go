package httpx_test

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"

	"github.com/magickvoice/sentinel/server/gateway/internal/httpx"
)

// chain is the middleware stack api.Server.Routes builds, in the same order and
// including the OpenTelemetry wrapper.
//
// The otelhttp wrapper is here rather than only in the api package because it is
// another ResponseWriter wrapper, and a ResponseWriter wrapper is exactly what the
// comment in httpx.go warns about: hiding Flusher breaks the SSE floor view, hiding
// Hijacker breaks the WebSocket upgrade on /v1/ingest, and both fail invisibly in
// production while every test that mounts a handler on its own keeps passing. Having
// it in this chain means a version bump that regressed it fails the build.
func chain(h http.Handler) http.Handler {
	log := slog.New(slog.NewTextHandler(io.Discard, nil))
	return otelhttp.NewHandler(
		httpx.WithRequestID(httpx.Recover(log, httpx.LogRequests(log, h))),
		"test")
}

// The two long-lived endpoints this service exists for both need capabilities that a
// naive ResponseWriter wrapper hides. The wrapper is invisible to a test that mounts
// a handler directly, so these mount the full chain instead — which is the only
// arrangement that would have caught it.

func TestTheMiddlewareChainDoesNotHideFlusher(t *testing.T) {
	srv := httptest.NewServer(chain(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if err := http.NewResponseController(w).Flush(); err != nil {
			t.Errorf("flush through the chain failed: %v", err)
		}
		_, _ = w.Write([]byte("streamed"))
	})))
	defer srv.Close()

	resp, err := srv.Client().Get(srv.URL)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if string(body) != "streamed" {
		t.Fatalf("body %q", body)
	}
}

func TestTheMiddlewareChainDoesNotHideHijacker(t *testing.T) {
	// A WebSocket upgrade needs to hijack the connection. If the chain hides
	// Hijacker, /v1/ingest fails at runtime in production while every unit test
	// that mounts the handler on its own keeps passing.
	srv := httptest.NewServer(chain(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Errorf("accept through the chain failed: %v", err)
			return
		}
		defer conn.CloseNow()
		ctx, cancel := context.WithTimeout(r.Context(), 5*time.Second)
		defer cancel()
		_ = conn.Write(ctx, websocket.MessageText, []byte("hello"))
		_ = conn.Close(websocket.StatusNormalClosure, "")
	})))
	defer srv.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	conn, _, err := websocket.Dial(ctx, strings.Replace(srv.URL, "http://", "ws://", 1), nil)
	if err != nil {
		t.Fatalf("dial through the chain: %v", err)
	}
	defer conn.CloseNow()
	_, data, err := conn.Read(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != "hello" {
		t.Fatalf("got %q", data)
	}
}

func TestARequestIdIsAssignedAndEchoed(t *testing.T) {
	var seen string
	srv := httptest.NewServer(chain(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seen = httpx.RequestID(r.Context())
	})))
	defer srv.Close()

	resp, err := srv.Client().Get(srv.URL)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if seen == "" || resp.Header.Get("X-Request-Id") != seen {
		t.Fatalf("request id %q, header %q", seen, resp.Header.Get("X-Request-Id"))
	}
}

func TestACallerSuppliedRequestIdIsKept(t *testing.T) {
	srv := httptest.NewServer(chain(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if got := httpx.RequestID(r.Context()); got != "abc-123" {
			t.Errorf("request id %q", got)
		}
	})))
	defer srv.Close()

	req, _ := http.NewRequest(http.MethodGet, srv.URL, nil)
	req.Header.Set("X-Request-Id", "abc-123")
	resp, err := srv.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
}

func TestAPanicBecomesAFiveHundredWithTheRequestId(t *testing.T) {
	srv := httptest.NewServer(chain(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {
		panic("boom")
	})))
	defer srv.Close()

	resp, err := srv.Client().Get(srv.URL)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusInternalServerError {
		t.Fatalf("status %d", resp.StatusCode)
	}
	body, _ := io.ReadAll(resp.Body)
	if !strings.Contains(string(body), "request_id") {
		t.Fatalf("a 500 must be traceable: %s", body)
	}
	if strings.Contains(string(body), "boom") {
		t.Fatal("panic detail leaked to the caller")
	}
}
