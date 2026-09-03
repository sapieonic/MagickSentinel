package api_test

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/api"
	"github.com/magickvoice/sentinel/server/gateway/internal/httpx"
)

// These tests need no database: the token endpoint sits outside Authenticate and
// touches no store, which is the point of it — there is no identity yet.

// fakeBroker stands in for Identity Platform.
type fakeBroker struct {
	// exchanged and refreshed record exactly what the endpoint forwarded, so the
	// tests can assert that the verifier and the redirect reach the upstream
	// unchanged and that no client secret is invented on the way.
	exchanged []exchangeCall
	refreshed []string
	result    *api.TokenResult
	err       error
}

type exchangeCall struct {
	code, verifier, redirectURI, clientID string
}

func (b *fakeBroker) ExchangeCode(_ context.Context, code, verifier, redirectURI, clientID string) (*api.TokenResult, error) {
	b.exchanged = append(b.exchanged, exchangeCall{code, verifier, redirectURI, clientID})
	return b.result, b.err
}

func (b *fakeBroker) Refresh(_ context.Context, refreshToken string) (*api.TokenResult, error) {
	b.refreshed = append(b.refreshed, refreshToken)
	return b.result, b.err
}

// A verifier of the length RFC 7636 requires: 32 random bytes base64url-encode to
// exactly 43 characters, which is what client/sentinel-agent/src/auth/pkce.rs
// produces.
const goodVerifier = "0123456789abcdefghijklmnopqrstuvwxyzABCDEF-"

func tokenServer(t *testing.T, broker api.TokenBroker, limiter *httpx.RateLimiter) *httptest.Server {
	t.Helper()
	srv := &api.Server{
		Log:          slog.New(slog.NewTextHandler(io.Discard, nil)),
		Version:      "test",
		TokenBroker:  broker,
		TokenLimiter: limiter,
	}
	s := httptest.NewServer(srv.Routes())
	t.Cleanup(s.Close)
	return s
}

// postForm sends a form-encoded token request the way the agent does.
func postForm(t *testing.T, srv *httptest.Server, form url.Values) (*http.Response, []byte) {
	t.Helper()
	req, err := http.NewRequest(http.MethodPost, srv.URL+"/v1/oauth/token",
		strings.NewReader(form.Encode()))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	resp, err := srv.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	return resp, body
}

func codeForm() url.Values {
	return url.Values{
		"grant_type":    {"authorization_code"},
		"code":          {"4/0AY0e-the-code"},
		"code_verifier": {goodVerifier},
		"redirect_uri":  {"http://127.0.0.1:49712/callback"},
		"client_id":     {"sentinel-desktop"},
	}
}

// ------------------------------------------------------------------ happy paths

func TestTheAuthorizationCodeGrantReturnsTheShapeTheAgentParses(t *testing.T) {
	// The response shape is fixed by TokenResponse in
	// client/sentinel-agent/src/api.rs, which another work stream owns. This test
	// is the guard on that agreement from this side.
	broker := &fakeBroker{result: &api.TokenResult{
		IDToken: "the.id.token", RefreshToken: "the-refresh-token", ExpiresIn: 3600,
	}}
	srv := tokenServer(t, broker, nil)

	resp, body := postForm(t, srv, codeForm())
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var got map[string]any
	if err := json.Unmarshal(body, &got); err != nil {
		t.Fatalf("response is not JSON: %v", err)
	}
	if got["id_token"] != "the.id.token" {
		t.Errorf("id_token %v", got["id_token"])
	}
	if got["refresh_token"] != "the-refresh-token" {
		t.Errorf("refresh_token %v", got["refresh_token"])
	}
	if got["expires_in"] != float64(3600) {
		t.Errorf("expires_in %v", got["expires_in"])
	}
	if got["token_type"] != "Bearer" {
		t.Errorf("token_type %v; RFC 6749 §5.1 requires it", got["token_type"])
	}
	// A token response in a proxy cache is a credential at rest somewhere nobody
	// is managing (RFC 6749 §5.1).
	if cc := resp.Header.Get("Cache-Control"); cc != "no-store" {
		t.Errorf("Cache-Control %q, want no-store", cc)
	}

	if len(broker.exchanged) != 1 {
		t.Fatalf("broker called %d times", len(broker.exchanged))
	}
	call := broker.exchanged[0]
	if call.verifier != goodVerifier {
		t.Errorf("the PKCE verifier did not reach the upstream: %q", call.verifier)
	}
	if call.redirectURI != "http://127.0.0.1:49712/callback" || call.clientID != "sentinel-desktop" {
		t.Errorf("forwarded %+v", call)
	}
}

func TestTheRefreshGrantWorksAndOmitsAnUnrotatedRefreshToken(t *testing.T) {
	// Identity Platform does not always rotate the refresh token. The field has to
	// be absent rather than an empty string, or the agent stores "" as a token and
	// presents it on the next refresh.
	broker := &fakeBroker{result: &api.TokenResult{IDToken: "fresh.id.token", ExpiresIn: 3600}}
	srv := tokenServer(t, broker, nil)

	resp, body := postForm(t, srv, url.Values{
		"grant_type":    {"refresh_token"},
		"refresh_token": {"the-refresh-token"},
	})
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var got map[string]any
	if err := json.Unmarshal(body, &got); err != nil {
		t.Fatal(err)
	}
	if _, present := got["refresh_token"]; present {
		t.Errorf("an unrotated refresh token must be omitted, not empty: %s", body)
	}
	if len(broker.refreshed) != 1 || broker.refreshed[0] != "the-refresh-token" {
		t.Fatalf("broker saw %v", broker.refreshed)
	}
}

func TestAProviderThatOmitsTheExpiryIsGivenTheStandardHour(t *testing.T) {
	// The agent's refresh schedule (REFRESH_AT in
	// client/sentinel-agent/src/auth/mod.rs) is driven off this number, so it is
	// stated rather than left out.
	broker := &fakeBroker{result: &api.TokenResult{IDToken: "the.id.token"}}
	srv := tokenServer(t, broker, nil)
	_, body := postForm(t, srv, codeForm())
	var got struct {
		ExpiresIn int64 `json:"expires_in"`
	}
	if err := json.Unmarshal(body, &got); err != nil {
		t.Fatal(err)
	}
	if got.ExpiresIn != 3600 {
		t.Fatalf("expires_in %d, want the standard hour", got.ExpiresIn)
	}
}

func TestAnAccessTokenOnlyProviderStillProducesAUsableResponse(t *testing.T) {
	// TokenResponse in api.rs prefers id_token and falls back to access_token, so
	// a provider that returns only the latter must not be turned into an error
	// here.
	broker := &fakeBroker{result: &api.TokenResult{AccessToken: "the.access.token", ExpiresIn: 900}}
	srv := tokenServer(t, broker, nil)
	resp, body := postForm(t, srv, codeForm())
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	if !strings.Contains(string(body), "access_token") {
		t.Fatalf("body %s", body)
	}
}

// --------------------------------------------------------------- request errors

func TestTheTokenEndpointRefusesMalformedRequestsWithRfc6749Bodies(t *testing.T) {
	broker := &fakeBroker{result: &api.TokenResult{IDToken: "the.id.token", ExpiresIn: 3600}}
	srv := tokenServer(t, broker, nil)

	// A verifier one character short of the RFC 7636 minimum.
	shortVerifier := goodVerifier[:42]
	// And one with a character outside the unreserved set.
	badCharVerifier := strings.Repeat("a", 42) + "/"

	cases := []struct {
		name       string
		form       url.Values
		wantStatus int
		wantError  string
		// wantBrokerCalls asserts that a refused request never reached the
		// upstream, which is what makes these checks a defence rather than
		// decoration.
		wantBrokerCalls int
	}{
		{
			name:       "no grant type",
			form:       url.Values{"code": {"x"}},
			wantStatus: http.StatusBadRequest, wantError: "invalid_request",
		},
		{
			name:       "a grant type nobody should be using",
			form:       url.Values{"grant_type": {"password"}, "username": {"a"}, "password": {"b"}},
			wantStatus: http.StatusBadRequest, wantError: "unsupported_grant_type",
		},
		{
			name:       "the implicit grant is not on offer either",
			form:       url.Values{"grant_type": {"client_credentials"}},
			wantStatus: http.StatusBadRequest, wantError: "unsupported_grant_type",
		},
		{
			name: "a client secret from a desktop is refused, not ignored",
			form: withForm(codeForm(), "client_secret", "leaked-from-an-msi"),
			// 401 with WWW-Authenticate, per RFC 6749 §5.2. A secret arriving
			// here means it is in an MSI on 200 desktops and needs revoking;
			// ignoring it would hide that.
			wantStatus: http.StatusUnauthorized, wantError: "invalid_client",
		},
		{
			name:       "no code",
			form:       withoutForm(codeForm(), "code"),
			wantStatus: http.StatusBadRequest, wantError: "invalid_request",
		},
		{
			name:       "no code verifier, which would be a PKCE downgrade",
			form:       withoutForm(codeForm(), "code_verifier"),
			wantStatus: http.StatusBadRequest, wantError: "invalid_request",
		},
		{
			name:       "a verifier shorter than RFC 7636 permits",
			form:       withForm(codeForm(), "code_verifier", shortVerifier),
			wantStatus: http.StatusBadRequest, wantError: "invalid_request",
		},
		{
			name:       "a verifier carrying a character outside the unreserved set",
			form:       withForm(codeForm(), "code_verifier", badCharVerifier),
			wantStatus: http.StatusBadRequest, wantError: "invalid_request",
		},
		{
			name:       "no redirect uri",
			form:       withoutForm(codeForm(), "redirect_uri"),
			wantStatus: http.StatusBadRequest, wantError: "invalid_request",
		},
		{
			name:       "a redirect uri pointing at somebody else's server",
			form:       withForm(codeForm(), "redirect_uri", "https://evil.example.com/callback"),
			wantStatus: http.StatusBadRequest, wantError: "invalid_grant",
		},
		{
			name: "localhost rather than the literal loopback address",
			// The agent builds http://127.0.0.1:{port}/callback deliberately
			// (RFC 8252 §7.3), so a localhost redirect is one only something
			// else could have produced.
			form:       withForm(codeForm(), "redirect_uri", "http://localhost:49712/callback"),
			wantStatus: http.StatusBadRequest, wantError: "invalid_grant",
		},
		{
			name:       "a loopback redirect with no port",
			form:       withForm(codeForm(), "redirect_uri", "http://127.0.0.1/callback"),
			wantStatus: http.StatusBadRequest, wantError: "invalid_grant",
		},
		{
			name:       "no refresh token on a refresh grant",
			form:       url.Values{"grant_type": {"refresh_token"}},
			wantStatus: http.StatusBadRequest, wantError: "invalid_request",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			broker.exchanged, broker.refreshed = nil, nil
			resp, body := postForm(t, srv, tc.form)
			if resp.StatusCode != tc.wantStatus {
				t.Fatalf("status %d, want %d: %s", resp.StatusCode, tc.wantStatus, body)
			}
			var got struct {
				Error       string `json:"error"`
				Description string `json:"error_description"`
			}
			if err := json.Unmarshal(body, &got); err != nil {
				t.Fatalf("error body is not JSON: %v (%s)", err, body)
			}
			if got.Error != tc.wantError {
				t.Errorf("error %q, want %q (%s)", got.Error, tc.wantError, got.Description)
			}
			if got.Description == "" {
				t.Error("an RFC 6749 error should say what was wrong")
			}
			if n := len(broker.exchanged) + len(broker.refreshed); n != tc.wantBrokerCalls {
				t.Errorf("the upstream was called %d times for a request that should "+
					"have been refused locally", n)
			}
			if tc.wantStatus == http.StatusUnauthorized &&
				resp.Header.Get("WWW-Authenticate") == "" {
				t.Error("RFC 6749 §5.2 requires WWW-Authenticate on a 401 from a token endpoint")
			}
		})
	}
}

func TestANonFormBodyIsRefusedRatherThanReadAsAnEmptyForm(t *testing.T) {
	// Relying on ParseForm would accept a JSON body as an empty form and answer
	// "grant_type is required", which sends whoever is debugging it in the wrong
	// direction entirely.
	srv := tokenServer(t, &fakeBroker{}, nil)
	req, err := http.NewRequest(http.MethodPost, srv.URL+"/v1/oauth/token",
		strings.NewReader(`{"grant_type":"refresh_token"}`))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := srv.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("status %d", resp.StatusCode)
	}
	if !strings.Contains(string(body), "x-www-form-urlencoded") {
		t.Fatalf("the refusal should name the expected encoding: %s", body)
	}
}

func TestAFormWithAContentTypeCharsetParameterIsStillAccepted(t *testing.T) {
	// Some HTTP clients append "; charset=utf-8". Refusing that would refuse a
	// perfectly valid request.
	broker := &fakeBroker{result: &api.TokenResult{IDToken: "the.id.token", ExpiresIn: 3600}}
	srv := tokenServer(t, broker, nil)
	req, err := http.NewRequest(http.MethodPost, srv.URL+"/v1/oauth/token",
		strings.NewReader(codeForm().Encode()))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded; charset=UTF-8")
	resp, err := srv.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		body, _ := io.ReadAll(resp.Body)
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
}

// ---------------------------------------------------------------- broker errors

func TestUpstreamRefusalsReachTheClientWithTheCodeItCanActOn(t *testing.T) {
	// This mapping decides what the agent does next: invalid_grant means start a
	// fresh sign-in, and anything else means retry. Getting it wrong either signs
	// the floor out for no reason or leaves an agent retrying a dead token until
	// their shift ends.
	cases := []struct {
		name       string
		brokerErr  error
		wantStatus int
		wantError  string
	}{
		{
			name: "an expired authorization code",
			brokerErr: &api.TokenError{
				Code: "invalid_grant", Description: "the code has expired",
				Status: http.StatusBadRequest,
			},
			wantStatus: http.StatusBadRequest, wantError: "invalid_grant",
		},
		{
			name: "a suspended account",
			brokerErr: &api.TokenError{
				Code: "unauthorized_client", Description: "this account is not permitted",
				Status: http.StatusForbidden,
			},
			wantStatus: http.StatusForbidden, wantError: "unauthorized_client",
		},
		{
			name: "the provider rate limiting us",
			brokerErr: &api.TokenError{
				Code: "temporarily_unavailable", Description: "retry shortly",
				Status: http.StatusServiceUnavailable,
			},
			wantStatus: http.StatusServiceUnavailable, wantError: "temporarily_unavailable",
		},
		{
			name: "an unclassified transport failure becomes a server error",
			// Deliberately a plain error, which is what a network failure or a
			// JSON decode problem arrives as. Its text must not reach the
			// client: a transport error carries the request URL, and the URL
			// carries our API key.
			brokerErr:  errUnclassified("dial tcp: connection refused to https://idp/token?key=AIzaSyREAL"),
			wantStatus: http.StatusBadGateway, wantError: "server_error",
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := tokenServer(t, &fakeBroker{err: tc.brokerErr}, nil)
			resp, body := postForm(t, srv, codeForm())
			if resp.StatusCode != tc.wantStatus {
				t.Fatalf("status %d, want %d: %s", resp.StatusCode, tc.wantStatus, body)
			}
			var got struct {
				Error       string `json:"error"`
				Description string `json:"error_description"`
			}
			if err := json.Unmarshal(body, &got); err != nil {
				t.Fatal(err)
			}
			if got.Error != tc.wantError {
				t.Fatalf("error %q, want %q", got.Error, tc.wantError)
			}
			if strings.Contains(string(body), "AIzaSy") {
				t.Fatalf("an upstream error leaked a credential to the client: %s", body)
			}
		})
	}
}

func TestABrokerThatReturnsNoTokenIsAServerErrorNotASuccess(t *testing.T) {
	// A 200 with no id_token would be stored by the agent as a session and then
	// rejected by Authenticate on every subsequent request, with a message about
	// signatures.
	srv := tokenServer(t, &fakeBroker{result: &api.TokenResult{}}, nil)
	resp, body := postForm(t, srv, codeForm())
	if resp.StatusCode != http.StatusBadGateway {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
}

func TestAnUnconfiguredDeploymentReportsItselfUnavailableRatherThanMissing(t *testing.T) {
	// A 404 here would tell a desktop that its API base URL is wrong, and it would
	// stop. A 503 tells it to retry and gives an operator something to act on.
	srv := tokenServer(t, nil, nil)
	resp, body := postForm(t, srv, codeForm())
	if resp.StatusCode != http.StatusServiceUnavailable {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	if !strings.Contains(string(body), "temporarily_unavailable") {
		t.Fatalf("body %s", body)
	}
}

// ----------------------------------------------------------------- rate limiting

func TestTheTokenEndpointIsRateLimited(t *testing.T) {
	// It sits outside Authenticate, so it is reachable by anyone who can resolve
	// the hostname, and it spends an upstream credential on every call.
	broker := &fakeBroker{result: &api.TokenResult{IDToken: "the.id.token", ExpiresIn: 3600}}
	srv := tokenServer(t, broker, httpx.NewRateLimiter(1, 2))

	for i := 0; i < 2; i++ {
		resp, body := postForm(t, srv, codeForm())
		if resp.StatusCode != http.StatusOK {
			t.Fatalf("request %d: %d %s", i+1, resp.StatusCode, body)
		}
	}
	resp, _ := postForm(t, srv, codeForm())
	if resp.StatusCode != http.StatusTooManyRequests {
		t.Fatalf("the third request was not limited: %d", resp.StatusCode)
	}
	if resp.Header.Get("Retry-After") == "" {
		t.Error("a rate-limited client needs to be told when to come back")
	}
	if len(broker.exchanged) != 2 {
		t.Fatalf("the limiter let %d requests reach the upstream, want 2", len(broker.exchanged))
	}
}

// --------------------------------------------------------------------- readiness

func TestReadyzFailsClosedAndNamesTheFailedCheckWithoutLeakingDetail(t *testing.T) {
	// /readyz is unauthenticated by necessity — a kubelet cannot present a bearer
	// token — so it must say which dependency failed and nothing about why.
	cases := []struct {
		name       string
		checks     []api.ReadyCheck
		wantStatus int
		wantState  string
	}{
		{
			name:       "no dependencies configured",
			wantStatus: http.StatusOK, wantState: "ready",
		},
		{
			name: "everything up",
			checks: []api.ReadyCheck{
				{Name: "database", Check: func(context.Context) error { return nil }},
				{Name: "object_store", Check: func(context.Context) error { return nil }},
			},
			wantStatus: http.StatusOK, wantState: "ready",
		},
		{
			name: "a dead object store makes the instance unready",
			checks: []api.ReadyCheck{
				{Name: "database", Check: func(context.Context) error { return nil }},
				{Name: "object_store", Check: func(context.Context) error {
					return errUnclassified("bucket sentinel-audio unreachable: " +
						"AccessDenied for arn:aws:iam::123456789012:role/gateway")
				}},
			},
			wantStatus: http.StatusServiceUnavailable, wantState: "not_ready",
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := &api.Server{
				Log:       slog.New(slog.NewTextHandler(io.Discard, nil)),
				Version:   "test",
				Readiness: tc.checks,
			}
			s := httptest.NewServer(srv.Routes())
			defer s.Close()

			resp, err := s.Client().Get(s.URL + "/readyz")
			if err != nil {
				t.Fatal(err)
			}
			defer resp.Body.Close()
			body, _ := io.ReadAll(resp.Body)
			if resp.StatusCode != tc.wantStatus {
				t.Fatalf("status %d, want %d: %s", resp.StatusCode, tc.wantStatus, body)
			}
			var got struct {
				Status string            `json:"status"`
				Checks map[string]string `json:"checks"`
			}
			if err := json.Unmarshal(body, &got); err != nil {
				t.Fatal(err)
			}
			if got.Status != tc.wantState {
				t.Errorf("status %q, want %q", got.Status, tc.wantState)
			}
			if len(got.Checks) != len(tc.checks) {
				t.Errorf("%d checks reported, want %d", len(got.Checks), len(tc.checks))
			}
			if strings.Contains(string(body), "arn:aws") || strings.Contains(string(body), "AccessDenied") {
				t.Fatalf("the readiness body leaked infrastructure detail: %s", body)
			}
		})
	}
}

func TestHealthzStaysConstantEvenWhenReadinessFails(t *testing.T) {
	// Liveness and readiness answer different questions. A gateway whose database
	// is down should be left alone to recover, not killed and restarted into the
	// same dead database.
	srv := &api.Server{
		Log:     slog.New(slog.NewTextHandler(io.Discard, nil)),
		Version: "test",
		Readiness: []api.ReadyCheck{{
			Name:  "database",
			Check: func(context.Context) error { return errUnclassified("down") },
		}},
	}
	s := httptest.NewServer(srv.Routes())
	defer s.Close()

	live, err := s.Client().Get(s.URL + "/healthz")
	if err != nil {
		t.Fatal(err)
	}
	live.Body.Close()
	if live.StatusCode != http.StatusOK {
		t.Fatalf("/healthz returned %d while a dependency was down", live.StatusCode)
	}
	ready, err := s.Client().Get(s.URL + "/readyz")
	if err != nil {
		t.Fatal(err)
	}
	ready.Body.Close()
	if ready.StatusCode != http.StatusServiceUnavailable {
		t.Fatalf("/readyz returned %d", ready.StatusCode)
	}
}

func TestAReadinessCheckThatHangsIsBoundedRatherThanBlockingTheProbe(t *testing.T) {
	// An orchestrator treats a timeout as a failure anyway; a probe that hangs
	// just delays the eviction and holds a connection open while it does.
	srv := &api.Server{
		Log:     slog.New(slog.NewTextHandler(io.Discard, nil)),
		Version: "test",
		Readiness: []api.ReadyCheck{{
			Name: "database",
			Check: func(ctx context.Context) error {
				<-ctx.Done()
				return ctx.Err()
			},
		}},
	}
	s := httptest.NewServer(srv.Routes())
	defer s.Close()

	start := time.Now()
	resp, err := s.Client().Get(s.URL + "/readyz")
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusServiceUnavailable {
		t.Fatalf("status %d", resp.StatusCode)
	}
	if elapsed := time.Since(start); elapsed > 10*time.Second {
		t.Fatalf("the probe took %s; it is meant to be bounded", elapsed)
	}
}

// ----------------------------------------------------------------------- helpers

func withForm(v url.Values, key, value string) url.Values {
	out := url.Values{}
	for k, vals := range v {
		out[k] = append([]string(nil), vals...)
	}
	out.Set(key, value)
	return out
}

func withoutForm(v url.Values, key string) url.Values {
	out := withForm(v, key, "")
	out.Del(key)
	return out
}

// errUnclassified is a plain error, standing in for the transport and decode
// failures that are not an api.TokenError.
type errUnclassified string

func (e errUnclassified) Error() string { return string(e) }
