// Package idp brokers the desktop agent's token exchange to Google Cloud Identity
// Platform.
//
// Two upstream legs, because Identity Platform is not itself the OAuth authorization
// server the desktop talks to:
//
//  1. **authorization_code.** The code came from Google's OAuth authorization
//     endpoint, so it is redeemed there (`https://oauth2.googleapis.com/token`) with
//     the client secret this process holds. That returns a *Google* ID token, which
//     the gateway's own Authenticate middleware would reject — it verifies tokens
//     issued by `securetoken.google.com/{project}`, not by `accounts.google.com`. So
//     the Google ID token is then presented to Identity Platform's
//     `accounts:signInWithIdp`, with the project's Web API key, which mints the
//     Identity Platform ID token and refresh token the rest of the system expects.
//
//  2. **refresh_token.** Identity Platform refresh tokens are redeemed at the secure
//     token service (`securetoken.googleapis.com/v1/token`), also with the API key.
//     One leg, and it is the one that runs every fifty minutes for every signed-in
//     agent on the floor.
//
// The client secret and the API key are the reason this package exists rather than
// the desktop calling Google directly. Both are credentials that cannot be
// distributed to 200 machines an agent has physical access to, cannot be rotated
// without an MSI rollout through the customer's SCCM, and cannot be revoked
// individually. Here they are two environment variables on a host we operate.
//
// Nothing in this package logs a token, a code, a verifier or a URL: the API key
// travels in the query string, so a logged request URL is a leaked credential.
package idp

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/api"
)

// Default upstream endpoints. Overridable so the whole broker can be pointed at a
// test double, and because Identity Platform's regional endpoints differ.
const (
	DefaultOAuthTokenURL  = "https://oauth2.googleapis.com/token"
	DefaultSignInWithIdP  = "https://identitytoolkit.googleapis.com/v1/accounts:signInWithIdp"
	DefaultSecureTokenURL = "https://securetoken.googleapis.com/v1/token"
	// DefaultProviderID is the federated provider Identity Platform is being told
	// the assertion came from. `google.com` is right when the desktop signs in
	// through a Google OAuth client, which is the configuration OPEN-2 assumes
	// until the customer's directory is known.
	DefaultProviderID = "google.com"
)

// Config is the broker's upstream credentials and endpoints.
type Config struct {
	// APIKey is the Identity Platform Web API key. Required: both the
	// signInWithIdp call and the secure-token call are keyed rather than
	// bearer-authorised.
	APIKey string
	// ClientID is the Google OAuth client the desktop authorizes against. The
	// desktop sends it too; this one is authoritative, and a mismatch is refused
	// rather than resolved in favour of the client's.
	ClientID string
	// ClientSecret is the confidential half of that OAuth client. Optional: a
	// client registered as a native application has no secret, in which case PKCE
	// alone completes the exchange. When it is set, it is what makes this endpoint
	// worth having.
	ClientSecret string
	// TenantID scopes the sign-in to one Identity Platform tenant — one BPO
	// customer's user pool, which is where the hard isolation at the auth layer
	// comes from (see internal/auth's package comment). Optional only because a
	// single-tenant development project has none.
	TenantID string

	OAuthTokenURL  string
	SignInWithIdP  string
	SecureTokenURL string
	ProviderID     string

	// HTTP is the client used for all three calls. A timeout is mandatory: this
	// runs inside a request the desktop is waiting on, and an upstream that
	// accepts the connection and never answers would otherwise hold the handler
	// open until the agent's own 20-second timeout fires.
	HTTP *http.Client
}

// Broker implements api.TokenBroker against Identity Platform.
type Broker struct {
	cfg Config
}

var _ api.TokenBroker = (*Broker)(nil)

// New validates the configuration and returns a broker.
func New(cfg Config) (*Broker, error) {
	if cfg.APIKey == "" {
		return nil, errors.New("idp: an Identity Platform API key is required")
	}
	if cfg.ClientID == "" {
		return nil, errors.New("idp: an OAuth client id is required")
	}
	if cfg.OAuthTokenURL == "" {
		cfg.OAuthTokenURL = DefaultOAuthTokenURL
	}
	if cfg.SignInWithIdP == "" {
		cfg.SignInWithIdP = DefaultSignInWithIdP
	}
	if cfg.SecureTokenURL == "" {
		cfg.SecureTokenURL = DefaultSecureTokenURL
	}
	if cfg.ProviderID == "" {
		cfg.ProviderID = DefaultProviderID
	}
	if cfg.HTTP == nil {
		cfg.HTTP = &http.Client{Timeout: 15 * time.Second}
	}
	return &Broker{cfg: cfg}, nil
}

// ExchangeCode redeems an authorization code and converts the result into an
// Identity Platform token set.
func (b *Broker) ExchangeCode(ctx context.Context, code, codeVerifier, redirectURI, clientID string) (*api.TokenResult, error) {
	// The client id is checked rather than trusted. The desktop sends one and it
	// should be ours; if it is not, the caller is either misconfigured or is
	// somebody else's client trying to redeem a code through our secret, and
	// forwarding it would use our credential on their behalf.
	if clientID != "" && clientID != b.cfg.ClientID {
		return nil, &api.TokenError{
			Code:        "invalid_client",
			Description: "client_id is not the client this deployment serves",
			Status:      http.StatusUnauthorized,
		}
	}

	form := url.Values{
		"grant_type":    {"authorization_code"},
		"code":          {code},
		"redirect_uri":  {redirectURI},
		"client_id":     {b.cfg.ClientID},
		"code_verifier": {codeVerifier},
	}
	if b.cfg.ClientSecret != "" {
		form.Set("client_secret", b.cfg.ClientSecret)
	}

	var google struct {
		IDToken     string `json:"id_token"`
		AccessToken string `json:"access_token"`
		Error       string `json:"error"`
		ErrorDesc   string `json:"error_description"`
	}
	if err := b.postForm(ctx, b.cfg.OAuthTokenURL, form, &google); err != nil {
		return nil, err
	}
	if google.Error != "" {
		return nil, upstreamOAuthError(google.Error, google.ErrorDesc)
	}
	if google.IDToken == "" {
		// No ID token means the authorize request did not ask for the openid
		// scope, or the client is configured for something other than OIDC. Either
		// way there is nothing to hand Identity Platform.
		return nil, &api.TokenError{
			Code: "invalid_grant",
			Description: "the upstream provider returned no id_token; " +
				"the authorize request must include the openid scope",
			Status: http.StatusBadRequest,
		}
	}

	return b.signInWithIdP(ctx, google.IDToken)
}

// signInWithIdP turns a federated ID token into an Identity Platform token set.
//
// `returnSecureToken` is what asks for a refresh token as well as an ID token.
// Without it the agent gets a one-hour credential and no way to renew it, and the
// whole session ends mid-shift.
func (b *Broker) signInWithIdP(ctx context.Context, federatedIDToken string) (*api.TokenResult, error) {
	body := map[string]any{
		// The assertion Identity Platform is being asked to verify, in the
		// URL-encoded form its API expects.
		"postBody":            "id_token=" + url.QueryEscape(federatedIDToken) + "&providerId=" + url.QueryEscape(b.cfg.ProviderID),
		"requestUri":          "http://localhost",
		"returnSecureToken":   true,
		"returnIdpCredential": false,
	}
	if b.cfg.TenantID != "" {
		body["tenantId"] = b.cfg.TenantID
	}

	var out struct {
		IDToken      string `json:"idToken"`
		RefreshToken string `json:"refreshToken"`
		ExpiresIn    string `json:"expiresIn"`
		Error        struct {
			Message string `json:"message"`
			Status  string `json:"status"`
		} `json:"error"`
	}
	if err := b.postJSON(ctx, b.withKey(b.cfg.SignInWithIdP), body, &out); err != nil {
		return nil, err
	}
	if out.Error.Message != "" {
		return nil, identityToolkitError(out.Error.Message)
	}
	if out.IDToken == "" {
		return nil, &api.TokenError{
			Code:        "server_error",
			Description: "Identity Platform returned no ID token",
			Status:      http.StatusBadGateway,
		}
	}
	return &api.TokenResult{
		IDToken:      out.IDToken,
		RefreshToken: out.RefreshToken,
		ExpiresIn:    atoi64(out.ExpiresIn),
	}, nil
}

// Refresh redeems an Identity Platform refresh token at the secure token service.
func (b *Broker) Refresh(ctx context.Context, refreshToken string) (*api.TokenResult, error) {
	form := url.Values{
		"grant_type":    {"refresh_token"},
		"refresh_token": {refreshToken},
	}
	var out struct {
		IDToken      string `json:"id_token"`
		RefreshToken string `json:"refresh_token"`
		ExpiresIn    string `json:"expires_in"`
		Error        struct {
			Message string `json:"message"`
		} `json:"error"`
		// The secure token service answers a bad grant with the plain OAuth
		// shape on some paths and the Google API error envelope on others, so
		// both are read.
		OAuthError string `json:"error_description"`
	}
	if err := b.postForm(ctx, b.withKey(b.cfg.SecureTokenURL), form, &out); err != nil {
		return nil, err
	}
	if out.Error.Message != "" {
		return nil, identityToolkitError(out.Error.Message)
	}
	if out.IDToken == "" {
		// A refresh token that no longer works is `invalid_grant`, and getting
		// that code right matters more here than anywhere else in this package:
		// the agent's error handling turns invalid_grant into a fresh sign-in
		// prompt and anything else into a retry, so a misclassification either
		// signs the floor out for no reason or leaves an agent retrying a dead
		// token until their shift ends.
		return nil, &api.TokenError{
			Code:        "invalid_grant",
			Description: "the refresh token is expired, revoked, or belongs to another project",
			Status:      http.StatusBadRequest,
		}
	}
	return &api.TokenResult{
		IDToken:      out.IDToken,
		RefreshToken: out.RefreshToken,
		ExpiresIn:    atoi64(out.ExpiresIn),
	}, nil
}

// withKey appends the API key. The key is a query parameter because that is what
// Google's APIs take; it is also why nothing here logs a URL.
func (b *Broker) withKey(endpoint string) string {
	sep := "?"
	if strings.Contains(endpoint, "?") {
		sep = "&"
	}
	return endpoint + sep + "key=" + url.QueryEscape(b.cfg.APIKey)
}

func (b *Broker) postForm(ctx context.Context, endpoint string, form url.Values, out any) error {
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint,
		strings.NewReader(form.Encode()))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	return b.do(req, out)
}

func (b *Broker) postJSON(ctx context.Context, endpoint string, body any, out any) error {
	encoded, err := json.Marshal(body)
	if err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint,
		bytes.NewReader(encoded))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")
	return b.do(req, out)
}

// maxUpstreamResponse bounds what is read back. Both upstreams answer in a few
// kilobytes; the limit is here so a compromised or misdirected endpoint cannot make
// the gateway buffer an arbitrary body per in-flight sign-in.
const maxUpstreamResponse = 256 << 10

func (b *Broker) do(req *http.Request, out any) error {
	resp, err := b.cfg.HTTP.Do(req)
	if err != nil {
		// Deliberately does not wrap the error's message into a TokenError: a
		// transport error carries the URL, and the URL carries the API key.
		return fmt.Errorf("idp: upstream request failed: %w", redactURL(err))
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, maxUpstreamResponse))
	if err != nil {
		return fmt.Errorf("idp: reading the upstream response failed: %w", err)
	}
	// Decoded regardless of status, because both upstreams put their machine
	// readable error in the body and the status alone does not distinguish "the
	// code expired" from "our key is wrong".
	if err := json.Unmarshal(raw, out); err != nil {
		if resp.StatusCode >= 500 {
			return &api.TokenError{
				Code:        "temporarily_unavailable",
				Description: "the identity provider is unavailable",
				Status:      http.StatusBadGateway,
			}
		}
		return errors.New("idp: the upstream response was not JSON")
	}
	return nil
}

// upstreamOAuthError maps an RFC 6749 error from Google's OAuth endpoint through to
// the desktop unchanged where the code is one the client can act on.
//
// `invalid_client` is the one to be careful with: from the upstream it means *our*
// client credentials are wrong, which is a gateway misconfiguration and not the
// caller's problem. Passing it through would tell a desktop to stop retrying when the
// fix is on our side, so it becomes a server error.
func upstreamOAuthError(code, description string) error {
	switch code {
	case "invalid_grant":
		return &api.TokenError{
			Code: "invalid_grant",
			Description: "the authorization code is expired, already used, " +
				"or was issued for a different redirect",
			Status: http.StatusBadRequest,
		}
	case "invalid_request", "unsupported_grant_type", "invalid_scope":
		return &api.TokenError{Code: code, Description: description, Status: http.StatusBadRequest}
	case "invalid_client", "unauthorized_client":
		return &api.TokenError{
			Code:        "server_error",
			Description: "this deployment's identity provider credentials were rejected",
			Status:      http.StatusBadGateway,
		}
	default:
		return &api.TokenError{
			Code:        "server_error",
			Description: "the identity provider refused the exchange",
			Status:      http.StatusBadGateway,
		}
	}
}

// identityToolkitError maps Identity Platform's error strings.
//
// The API answers with a short screaming-snake-case token — TOKEN_EXPIRED,
// USER_DISABLED, INVALID_IDP_RESPONSE — sometimes with a colon and detail after it.
// Only the ones the client can act on differently are distinguished; everything else
// is a server error, because telling a desktop "invalid_grant" for a problem it
// cannot fix makes it throw away a working refresh token and sign the agent out
// mid-shift.
func identityToolkitError(message string) error {
	code := message
	if i := strings.IndexByte(code, ':'); i >= 0 {
		code = strings.TrimSpace(code[:i])
	}
	switch strings.ToUpper(code) {
	case "TOKEN_EXPIRED", "INVALID_REFRESH_TOKEN", "INVALID_GRANT_TYPE",
		"MISSING_REFRESH_TOKEN", "INVALID_ID_TOKEN", "INVALID_IDP_RESPONSE":
		return &api.TokenError{
			Code:        "invalid_grant",
			Description: "the credential presented is expired or no longer valid",
			Status:      http.StatusBadRequest,
		}
	case "USER_DISABLED", "USER_NOT_FOUND":
		// The account is gone or suspended. Distinct from invalid_grant so the
		// agent stops rather than looping through a sign-in that cannot succeed —
		// which is also what happens when an admin suspends a user mid-shift.
		return &api.TokenError{
			Code:        "unauthorized_client",
			Description: "this account is not permitted to sign in",
			Status:      http.StatusForbidden,
		}
	case "TOO_MANY_ATTEMPTS_TRY_LATER", "QUOTA_EXCEEDED":
		return &api.TokenError{
			Code:        "temporarily_unavailable",
			Description: "the identity provider is rate limiting; retry shortly",
			Status:      http.StatusServiceUnavailable,
		}
	default:
		return &api.TokenError{
			Code:        "server_error",
			Description: "the identity provider refused the sign-in",
			Status:      http.StatusBadGateway,
		}
	}
}

// redactURL strips the query string out of a *url.Error so an API key in a wrapped
// transport error does not reach a log line.
func redactURL(err error) error {
	var ue *url.Error
	if !errors.As(err, &ue) {
		return err
	}
	if i := strings.IndexByte(ue.URL, '?'); i >= 0 {
		ue.URL = ue.URL[:i] + "?key=REDACTED"
	}
	return ue
}

// atoi64 parses Google's string-typed expiry fields, returning 0 for anything
// unparseable so the caller applies the standard hour rather than a zero lifetime.
func atoi64(s string) int64 {
	n, err := strconv.ParseInt(strings.TrimSpace(s), 10, 64)
	if err != nil || n < 0 {
		return 0
	}
	return n
}
