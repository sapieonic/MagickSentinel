package httpx_test

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"

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

// ------------------------------------------------------------- log/trace joins

func TestTheRequestLineCarriesTheTraceIdWhenATraceIsInProgress(t *testing.T) {
	// This is the whole payoff of the OpenTelemetry work: an operator reading the
	// log line for a failed request can click through to the span tree, and back.
	// Grafana's Loki-to-Tempo correlation is driven off exactly these two fields,
	// so their absence is not cosmetic.
	var buf bytes.Buffer
	log := slog.New(slog.NewJSONHandler(&buf, nil))
	tp := sdktrace.NewTracerProvider(sdktrace.WithSampler(sdktrace.AlwaysSample()))
	t.Cleanup(func() { _ = tp.Shutdown(context.Background()) })

	handler := otelhttp.NewHandler(
		httpx.WithRequestID(httpx.LogRequests(log, http.HandlerFunc(
			func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(http.StatusNoContent) }))),
		"test", otelhttp.WithTracerProvider(tp))

	srv := httptest.NewServer(handler)
	defer srv.Close()
	resp, err := srv.Client().Get(srv.URL)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	var line map[string]any
	if err := json.Unmarshal(bytes.TrimSpace(buf.Bytes()), &line); err != nil {
		t.Fatalf("log line is not JSON: %v (%s)", err, buf.String())
	}
	traceID, _ := line["trace_id"].(string)
	spanID, _ := line["span_id"].(string)
	if len(traceID) != 32 {
		t.Errorf("trace_id %q is not a 16-byte hex id", traceID)
	}
	if len(spanID) != 16 {
		t.Errorf("span_id %q is not an 8-byte hex id", spanID)
	}
	if line["request_id"] == "" || line["request_id"] == nil {
		t.Error("the request id must still be there; it is what a customer quotes")
	}
}

func TestTheRequestLineOmitsTraceIdsWhenTelemetryIsOff(t *testing.T) {
	// A deployment with no collector gets the log line it always had. Empty
	// trace_id fields would be worse than absent ones: they index in Loki and
	// produce a facet full of zeroes.
	var buf bytes.Buffer
	log := slog.New(slog.NewJSONHandler(&buf, nil))
	srv := httptest.NewServer(httpx.WithRequestID(httpx.LogRequests(log,
		http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))))
	defer srv.Close()
	resp, err := srv.Client().Get(srv.URL)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	var line map[string]any
	if err := json.Unmarshal(bytes.TrimSpace(buf.Bytes()), &line); err != nil {
		t.Fatal(err)
	}
	if _, ok := line["trace_id"]; ok {
		t.Errorf("trace_id present with no tracer configured: %s", buf.String())
	}
}

func TestTheRequestLineLogsTheRoutePatternNotTheRawPath(t *testing.T) {
	// A raw path can carry an account reference in a query string, and this is a
	// compliance product. The assertion is on the absence, because that is the
	// property that matters.
	var buf bytes.Buffer
	log := slog.New(slog.NewJSONHandler(&buf, nil))
	mux := http.NewServeMux()
	mux.HandleFunc("GET /v1/calls/{id}", func(w http.ResponseWriter, r *http.Request) {})
	srv := httptest.NewServer(httpx.WithRequestID(httpx.LogRequests(log, mux)))
	defer srv.Close()

	resp, err := srv.Client().Get(srv.URL + "/v1/calls/01J8ZQ8H2Q7X9K3M4N5P6R7S8T?account_ref=ACC-99887")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	out := buf.String()
	if !strings.Contains(out, "/v1/calls/{id}") {
		t.Fatalf("route pattern missing: %s", out)
	}
	if strings.Contains(out, "ACC-99887") || strings.Contains(out, "01J8ZQ8H2Q7X9K3M4N5P6R7S8T") {
		t.Fatalf("the raw path leaked into the log: %s", out)
	}
}

// ---------------------------------------------------------------- rate limiting

func TestTheRateLimiterAllowsABurstThenRefills(t *testing.T) {
	cases := []struct {
		name string
		rate float64
		// requests to make back to back from one client.
		burst float64
		want  []bool
	}{
		{
			name: "a burst of three admits exactly three",
			rate: 1, burst: 3,
			want: []bool{true, true, true, false, false},
		},
		{
			name: "a burst below one is clamped to one, not to zero",
			rate: 1, burst: 0,
			want: []bool{true, false},
		},
		{
			name: "a non-positive rate still admits the burst",
			rate: 0, burst: 2,
			want: []bool{true, true, false},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			l := httpx.NewRateLimiter(tc.rate, tc.burst)
			for i, want := range tc.want {
				if got := l.Allow("10.0.0.1"); got != want {
					t.Fatalf("request %d: allowed=%v, want %v", i+1, got, want)
				}
			}
		})
	}
}

func TestTheRateLimiterIsPerClient(t *testing.T) {
	// One abusive address must not lock out a floor of legitimate ones.
	l := httpx.NewRateLimiter(1, 1)
	if !l.Allow("10.0.0.1") || l.Allow("10.0.0.1") {
		t.Fatal("the first client's bucket did not behave")
	}
	if !l.Allow("10.0.0.2") {
		t.Fatal("a second client was refused because of the first")
	}
}

func TestARateLimitedRequestGetsA429WithARetryAfter(t *testing.T) {
	// The desktop agent distinguishes status codes rather than parsing bodies
	// (client/sentinel-agent/src/api.rs), so the status is what carries the
	// meaning; Retry-After is what stops it hammering.
	srv := httptest.NewServer(httpx.WithRequestID(httpx.RateLimit(
		httpx.NewRateLimiter(1, 1), 5*time.Second,
		http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			w.WriteHeader(http.StatusNoContent)
		}))))
	defer srv.Close()

	first, err := srv.Client().Get(srv.URL)
	if err != nil {
		t.Fatal(err)
	}
	first.Body.Close()
	if first.StatusCode != http.StatusNoContent {
		t.Fatalf("first request: %d", first.StatusCode)
	}

	second, err := srv.Client().Get(srv.URL)
	if err != nil {
		t.Fatal(err)
	}
	defer second.Body.Close()
	if second.StatusCode != http.StatusTooManyRequests {
		t.Fatalf("second request: %d, want 429", second.StatusCode)
	}
	if second.Header.Get("Retry-After") != "5" {
		t.Fatalf("Retry-After %q", second.Header.Get("Retry-After"))
	}
	body, _ := io.ReadAll(second.Body)
	if !strings.Contains(string(body), "rate_limited") {
		t.Fatalf("body %s", body)
	}
}

func TestARateLimitedRouteWithNoLimiterIsUnrestricted(t *testing.T) {
	// Nil means no limit, which is how the tests mount the token endpoint. It must
	// pass the handler through rather than wrapping it in something that refuses.
	srv := httptest.NewServer(httpx.RateLimit(nil, time.Second,
		http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			w.WriteHeader(http.StatusNoContent)
		})))
	defer srv.Close()
	for i := 0; i < 5; i++ {
		resp, err := srv.Client().Get(srv.URL)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		if resp.StatusCode != http.StatusNoContent {
			t.Fatalf("request %d: %d", i+1, resp.StatusCode)
		}
	}
}

func TestTheClientKeyIgnoresForwardedForHeaders(t *testing.T) {
	// X-Forwarded-For is set by the client, so honouring it would let anyone
	// bypass the limit entirely by varying one string.
	r := httptest.NewRequest(http.MethodPost, "/v1/oauth/token", nil)
	r.RemoteAddr = "203.0.113.9:51234"
	r.Header.Set("X-Forwarded-For", "10.1.1.1")
	if got := httpx.ClientKey(r); got != "203.0.113.9" {
		t.Fatalf("client key %q; the forwarded header must not be trusted", got)
	}
}
