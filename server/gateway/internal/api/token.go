package api

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/url"
	"strings"

	"github.com/magickvoice/sentinel/server/gateway/internal/httpx"
)

// POST /v1/oauth/token — the desktop agent's token endpoint.
//
// # Why the gateway is in this path at all
//
// The desktop agent's OIDC configuration points `token_endpoint` at
// `{api_base}/v1/oauth/token` (client/sentinel-agent/src/auth/pkce.rs), so until this
// existed the PKCE flow dead-ended: the browser handed back an authorization code and
// there was nothing to exchange it at.
//
// It points here rather than straight at Google because of what the exchange needs.
// An authorization-code exchange against Google's OAuth endpoint wants a client
// secret, and turning a Google ID token into an Identity Platform ID token — the kind
// this gateway's Authenticate middleware verifies — wants an Identity Platform API
// key. Neither can live on 200 collections desktops: a key shipped in an MSI is a
// published key, rotating it means a fleet rollout through the customer's SCCM, and it
// sits on machines agents have physical access to. This endpoint is where those
// credentials *can* live, which is the same argument docs/architecture.md makes for
// keeping model provider keys server-side.
//
// So: the desktop stays a public client per RFC 8252, sends no secret, and proves the
// exchange belongs to its own authorize request with PKCE. The gateway adds the
// confidential half server-side and hands back the token set.
//
// # What is fixed and cannot be changed here
//
// The response shape is `TokenResponse` in client/sentinel-agent/src/api.rs:
// `id_token`, `access_token`, `refresh_token`, `expires_in`. The Rust struct prefers
// `id_token` and falls back to `access_token`, and treats a missing `expires_in` as
// the standard hour. `token_type` is added because RFC 6749 §5.1 requires it and the
// client ignores unknown fields; nothing else may be added without changing the
// client, which another work stream owns.
//
// The error shape is RFC 6749 §5.2 — `{"error", "error_description"}` — rather than
// this service's usual httpx.Error envelope. That is a deliberate exception: the
// caller here is an OAuth client following a spec, and every OAuth client library
// ever written parses that shape. The request id still goes out in the X-Request-Id
// header, so a failure is still traceable without the body carrying it.

// TokenResult is what the upstream identity provider returned.
type TokenResult struct {
	IDToken      string
	AccessToken  string
	RefreshToken string
	// ExpiresIn is the ID token's lifetime in seconds. Zero means the provider did
	// not say, which the client reads as the standard hour.
	ExpiresIn int64
}

// TokenBroker exchanges credentials with the upstream identity provider.
//
// An interface so the endpoint's request handling — the PKCE requirements, the
// redirect-URI check, the error mapping, all of which are the parts worth getting
// right — is testable without Google.
type TokenBroker interface {
	// ExchangeCode redeems an authorization code. The verifier is passed through
	// to the upstream authorization server, which is what actually checks it
	// against the challenge from the authorize request; see the note in
	// tokenEndpoint on why this service still insists on its presence.
	ExchangeCode(ctx context.Context, code, codeVerifier, redirectURI, clientID string) (*TokenResult, error)
	// Refresh exchanges a refresh token for a fresh ID token.
	Refresh(ctx context.Context, refreshToken string) (*TokenResult, error)
}

// TokenError is an RFC 6749 §5.2 error a broker can return to control the response.
//
// Brokers use it to pass an upstream refusal through with the right code: an expired
// authorization code has to reach the client as `invalid_grant`, because that is what
// tells it to start a fresh sign-in rather than retry. Collapsing everything to a 500
// would leave the agent retrying a code that will never work.
type TokenError struct {
	Code        string
	Description string
	Status      int
}

func (e *TokenError) Error() string {
	return fmt.Sprintf("oauth token endpoint: %s: %s", e.Code, e.Description)
}

// RFC 6749 §5.2 error codes, plus the ones §4.1.3 adds.
const (
	errInvalidRequest       = "invalid_request"
	errInvalidClient        = "invalid_client"
	errInvalidGrant         = "invalid_grant"
	errUnauthorizedClient   = "unauthorized_client"
	errUnsupportedGrantType = "unsupported_grant_type"
	errInvalidScope         = "invalid_scope"
	// Not in RFC 6749 but in RFC 6749's successor conventions and understood by
	// every client: the authorization server itself failed.
	errServerError        = "server_error"
	errTemporarilyUnavail = "temporarily_unavailable"
)

// maxTokenRequestBytes bounds the form body. A token request is a few hundred bytes;
// anything approaching this is not one, and the endpoint is unauthenticated.
const maxTokenRequestBytes = 8 << 10

func (s *Server) tokenEndpoint(w http.ResponseWriter, r *http.Request) {
	if s.TokenBroker == nil {
		// A deployment with no upstream configured. Reported as
		// temporarily_unavailable rather than as an unsupported grant, because the
		// client's correct response is to retry later or to tell the user the
		// service is misconfigured — not to conclude that its own request was
		// wrong.
		s.writeTokenError(w, r, "none", &TokenError{
			Code:        errTemporarilyUnavail,
			Description: "no identity provider is configured on this deployment",
			Status:      http.StatusServiceUnavailable,
		})
		return
	}

	// application/x-www-form-urlencoded, per RFC 6749 §4.1.3. Checked explicitly
	// rather than relying on ParseForm, which would silently accept a JSON body as
	// an empty form and produce a confusing "missing grant_type".
	ct := r.Header.Get("Content-Type")
	if mediaType(ct) != "application/x-www-form-urlencoded" {
		s.writeTokenError(w, r, "none", &TokenError{
			Code:        errInvalidRequest,
			Description: "the request body must be application/x-www-form-urlencoded",
			Status:      http.StatusBadRequest,
		})
		return
	}
	r.Body = http.MaxBytesReader(w, r.Body, maxTokenRequestBytes)
	if err := r.ParseForm(); err != nil {
		s.writeTokenError(w, r, "none", &TokenError{
			Code:        errInvalidRequest,
			Description: "the request body could not be parsed",
			Status:      http.StatusBadRequest,
		})
		return
	}

	grant := r.PostForm.Get("grant_type")

	// A client secret in the request is refused rather than ignored.
	//
	// The desktop is a public client (RFC 8252 §8.4): it has no secret, and PKCE is
	// what replaces one. A secret arriving here means either that somebody
	// configured the fleet with one — in which case it is now in an MSI on 200
	// desktops and needs revoking, and silently ignoring it would hide that — or
	// that a different client is calling this endpoint expecting confidential-client
	// semantics it will not get. Both are worth failing on.
	if r.PostForm.Get("client_secret") != "" {
		s.writeTokenError(w, r, grant, &TokenError{
			Code: errInvalidClient,
			Description: "this endpoint serves public clients only; " +
				"a client secret must not be sent from a desktop installation",
			Status: http.StatusUnauthorized,
		})
		return
	}

	switch grant {
	case "authorization_code":
		s.tokenByAuthorizationCode(w, r)
	case "refresh_token":
		s.tokenByRefreshToken(w, r)
	case "":
		s.writeTokenError(w, r, "none", &TokenError{
			Code:        errInvalidRequest,
			Description: "grant_type is required",
			Status:      http.StatusBadRequest,
		})
	default:
		s.writeTokenError(w, r, "unsupported", &TokenError{
			Code: errUnsupportedGrantType,
			Description: "only authorization_code and refresh_token are supported; " +
				"the implicit and password grants are not offered",
			Status: http.StatusBadRequest,
		})
	}
}

func (s *Server) tokenByAuthorizationCode(w http.ResponseWriter, r *http.Request) {
	const grant = "authorization_code"
	code := r.PostForm.Get("code")
	verifier := r.PostForm.Get("code_verifier")
	redirectURI := r.PostForm.Get("redirect_uri")
	clientID := r.PostForm.Get("client_id")

	if code == "" {
		s.writeTokenError(w, r, grant, &TokenError{
			Code: errInvalidRequest, Description: "code is required",
			Status: http.StatusBadRequest,
		})
		return
	}

	// PKCE is required, not optional.
	//
	// The upstream authorization server is the thing that verifies the verifier
	// against the challenge, so this check does not itself prove anything about
	// the exchange. What it prevents is a downgrade: a client that omits
	// code_verifier gets an exchange with no proof of possession, and if this
	// endpoint forwarded that, an attacker who intercepted an authorization code —
	// off a loopback redirect on a shared desktop, out of a browser history, out of
	// a URL in a crash report — could redeem it here. Refusing means the only way
	// to redeem a code through this gateway is to hold the verifier that was
	// generated alongside the challenge, which never leaves the agent process.
	if verifier == "" {
		s.writeTokenError(w, r, grant, &TokenError{
			Code: errInvalidRequest,
			Description: "code_verifier is required; this endpoint does not accept " +
				"an authorization code exchange without PKCE",
			Status: http.StatusBadRequest,
		})
		return
	}
	if !validCodeVerifier(verifier) {
		s.writeTokenError(w, r, grant, &TokenError{
			Code: errInvalidRequest,
			Description: "code_verifier must be 43 to 128 characters from the " +
				"RFC 7636 unreserved set",
			Status: http.StatusBadRequest,
		})
		return
	}
	if redirectURI == "" {
		s.writeTokenError(w, r, grant, &TokenError{
			Code: errInvalidRequest, Description: "redirect_uri is required",
			Status: http.StatusBadRequest,
		})
		return
	}
	if err := checkLoopbackRedirect(redirectURI); err != nil {
		// RFC 8252 §7.3: a native client's redirect is a loopback address. This
		// endpoint holds a credential that can complete an exchange, so it must
		// not be usable to complete one for a redirect that points at somebody
		// else's server — that would turn the gateway into a code-redemption
		// service for a stolen code with an attacker-controlled callback.
		s.writeTokenError(w, r, grant, &TokenError{
			Code:        errInvalidGrant,
			Description: err.Error(),
			Status:      http.StatusBadRequest,
		})
		return
	}

	result, err := s.TokenBroker.ExchangeCode(r.Context(), code, verifier, redirectURI, clientID)
	s.finishToken(w, r, grant, result, err)
}

func (s *Server) tokenByRefreshToken(w http.ResponseWriter, r *http.Request) {
	const grant = "refresh_token"
	refresh := r.PostForm.Get("refresh_token")
	if refresh == "" {
		s.writeTokenError(w, r, grant, &TokenError{
			Code: errInvalidRequest, Description: "refresh_token is required",
			Status: http.StatusBadRequest,
		})
		return
	}
	result, err := s.TokenBroker.Refresh(r.Context(), refresh)
	s.finishToken(w, r, grant, result, err)
}

// finishToken maps a broker outcome onto the wire.
func (s *Server) finishToken(w http.ResponseWriter, r *http.Request, grant string, result *TokenResult, err error) {
	if err != nil {
		var te *TokenError
		if errors.As(err, &te) {
			// An upstream refusal the broker classified. Logged at info: an
			// expired code or a revoked refresh token is a routine event on a
			// floor where agents sign in and out every shift, and logging it as
			// an error would train everyone to ignore errors.
			s.Log.Info("token endpoint refused a request",
				"grant_type", grant, "error", te.Code)
			s.writeTokenError(w, r, grant, te)
			return
		}
		// Anything else is ours or the network's. The upstream error is logged and
		// not returned: it can carry a URL with our API key in the query string,
		// which is exactly what this endpoint exists to keep off the desktop.
		s.Log.Error("token endpoint could not reach the identity provider",
			"grant_type", grant, "error", err)
		s.writeTokenError(w, r, grant, &TokenError{
			Code:        errServerError,
			Description: "the identity provider could not be reached",
			Status:      http.StatusBadGateway,
		})
		return
	}
	if result == nil || (result.IDToken == "" && result.AccessToken == "") {
		s.Log.Error("token endpoint got a token set with no token in it", "grant_type", grant)
		s.writeTokenError(w, r, grant, &TokenError{
			Code:        errServerError,
			Description: "the identity provider returned no token",
			Status:      http.StatusBadGateway,
		})
		return
	}

	s.Metrics.TokenGrant(r.Context(), grant, "issued")
	// No-store, per RFC 6749 §5.1. This response carries a refresh token that is
	// good for as long as the user's session; a proxy or browser cache holding a
	// copy is a credential at rest in a place nobody is managing.
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("Pragma", "no-cache")

	// The field set is fixed by TokenResponse in client/sentinel-agent/src/api.rs.
	// omitempty on the two optional ones so a refresh that does not rotate the
	// refresh token omits the field rather than sending an empty string, which the
	// client would store as a token and then present.
	type tokenResponse struct {
		IDToken      string `json:"id_token"`
		AccessToken  string `json:"access_token,omitempty"`
		RefreshToken string `json:"refresh_token,omitempty"`
		ExpiresIn    int64  `json:"expires_in"`
		TokenType    string `json:"token_type"`
	}
	expires := result.ExpiresIn
	if expires <= 0 {
		// Identity Platform ID tokens live an hour. Sent explicitly rather than
		// omitted so the agent's refresh schedule (REFRESH_AT in
		// client/sentinel-agent/src/auth/mod.rs) is driven by a number we stated.
		expires = 3600
	}
	httpx.WriteJSON(w, http.StatusOK, tokenResponse{
		IDToken:      result.IDToken,
		AccessToken:  result.AccessToken,
		RefreshToken: result.RefreshToken,
		ExpiresIn:    expires,
		TokenType:    "Bearer",
	})
}

func (s *Server) writeTokenError(w http.ResponseWriter, r *http.Request, grant string, te *TokenError) {
	s.Metrics.TokenGrant(r.Context(), grant, te.Code)
	status := te.Status
	if status == 0 {
		status = http.StatusBadRequest
	}
	w.Header().Set("Cache-Control", "no-store")
	if status == http.StatusUnauthorized {
		// RFC 6749 §5.2 requires this on a 401 from a token endpoint. Bearer
		// rather than Basic because there is no HTTP-authenticated client here to
		// prompt for credentials.
		w.Header().Set("WWW-Authenticate", `Bearer realm="sentinel"`)
	}
	// httpx.WriteJSON rather than httpx.WriteError: the body here is the RFC 6749
	// §5.2 shape, not this service's `code`/`message`/`request_id` envelope. The
	// request id still reaches the caller in the X-Request-Id header, which
	// httpx.WithRequestID set before this handler ran.
	httpx.WriteJSON(w, status, map[string]string{
		"error":             te.Code,
		"error_description": te.Description,
	})
}

// validCodeVerifier applies RFC 7636 §4.1: 43 to 128 characters from
// ALPHA / DIGIT / "-" / "." / "_" / "~".
//
// Length and alphabet only. The point is not to guess whether the verifier is the
// right one — only the authorization server can know that — but to refuse a
// malformed one here rather than forwarding it and receiving an upstream error that
// says nothing useful. It also stops the field being used to smuggle arbitrary bytes
// into an upstream request body.
func validCodeVerifier(v string) bool {
	if len(v) < 43 || len(v) > 128 {
		return false
	}
	for i := 0; i < len(v); i++ {
		c := v[i]
		switch {
		case c >= 'A' && c <= 'Z', c >= 'a' && c <= 'z', c >= '0' && c <= '9':
		case c == '-', c == '.', c == '_', c == '~':
		default:
			return false
		}
	}
	return true
}

// checkLoopbackRedirect enforces RFC 8252 §7.3 on the redirect URI.
//
// The literal loopback addresses only, never `localhost`: on a machine where
// `localhost` also resolves to ::1 the browser can connect to an address the agent's
// listener is not on, and the agent already builds `http://127.0.0.1:{port}/callback`
// for that reason (client/sentinel-agent/src/auth/pkce.rs). Accepting `localhost`
// here would accept a redirect the agent never produces, which means accepting one
// only something else could have produced.
func checkLoopbackRedirect(raw string) error {
	u, err := url.Parse(raw)
	if err != nil {
		return errors.New("redirect_uri is not a valid URI")
	}
	if u.Scheme != "http" {
		return errors.New("redirect_uri must be an http loopback address")
	}
	switch u.Hostname() {
	case "127.0.0.1", "::1":
	default:
		return errors.New("redirect_uri must be a loopback address: " +
			"http://127.0.0.1:{port}/... or http://[::1]:{port}/...")
	}
	if u.Port() == "" {
		// RFC 8252 §7.3 requires the authorization server to permit any port; a
		// native client binds an ephemeral one and cannot know it in advance. What
		// it must not do is omit it, because then the redirect is port 80 — a
		// privileged port the agent could not have bound.
		return errors.New("redirect_uri must name the loopback port the client is listening on")
	}
	return nil
}

// mediaType strips parameters off a Content-Type. Written out rather than reaching
// for mime.ParseMediaType because a malformed type must be a refusal, not an error to
// distinguish from a missing one.
func mediaType(ct string) string {
	if i := strings.IndexByte(ct, ';'); i >= 0 {
		ct = ct[:i]
	}
	return strings.ToLower(strings.TrimSpace(ct))
}
