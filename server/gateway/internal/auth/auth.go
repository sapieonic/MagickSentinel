// Package auth verifies the two identities Sentinel requires and turns them into a
// request context.
//
// Capture may not run unless **both** are valid:
//
//   - the device, proven by a client certificate issued at enrollment (mTLS), and
//   - the user, proven by a Google Cloud Identity Platform ID token.
//
// The single rule the rest of the server depends on: tenant and role are read from
// the verified token, never from a request body or path parameter. Everything else
// here exists to make that rule impossible to get wrong by accident.
package auth

import (
	"context"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/golang-jwt/jwt/v5"
)

var (
	ErrNoToken        = errors.New("auth: no bearer token")
	ErrTokenInvalid   = errors.New("auth: token invalid")
	ErrNoDeviceCert   = errors.New("auth: no client certificate")
	ErrDeviceRevoked  = errors.New("auth: device revoked")
	ErrTenantMismatch = errors.New("auth: certificate and token disagree on tenant")
	ErrForbidden      = errors.New("auth: role not permitted")
)

// Role values, in the order the spec's matrix lists them.
type Role string

const (
	RoleAgent      Role = "agent"
	RoleSupervisor Role = "supervisor"
	RoleQA         Role = "qa"
	RoleCompliance Role = "compliance"
	RoleAdmin      Role = "admin"
	RoleClient     Role = "client"
)

func (r Role) valid() bool {
	switch r {
	case RoleAgent, RoleSupervisor, RoleQA, RoleCompliance, RoleAdmin, RoleClient:
		return true
	}
	return false
}

// Identity is the verified caller. Constructed only by this package.
type Identity struct {
	UserUID  string
	TenantID string
	Role     Role
	TeamID   string
	// DeviceID is set only on connections that presented a device certificate.
	DeviceID string
	// CertFingerprint is the SHA-256 of the presented client certificate, used to
	// look the device up and to check revocation.
	CertFingerprint string
	ExpiresAt       time.Time
}

// Claims carried by an Identity Platform ID token. tenant_id, role and team_id are
// custom claims set by the provisioning path; they are authoritative because Google
// signed them.
type Claims struct {
	jwt.RegisteredClaims
	TenantID string `json:"tenant_id"`
	Role     Role   `json:"role"`
	TeamID   string `json:"team_id"`
	Email    string `json:"email"`
	// Firebase-specific block; `tenant` here is the Identity Platform tenant, one
	// per BPO customer, which gives hard isolation at the auth layer.
	Firebase struct {
		Tenant string `json:"tenant"`
	} `json:"firebase"`
}

// KeySource supplies the public keys an ID token is verified against.
type KeySource interface {
	Key(ctx context.Context, kid string) (*rsa.PublicKey, error)
}

// Verifier validates ID tokens.
type Verifier struct {
	Keys     KeySource
	Issuer   string
	Audience string
	// Leeway absorbs clock skew between the endpoint and the gateway. Collections
	// desktops are not reliably time-synced.
	Leeway time.Duration
	// Now is injectable for tests.
	Now func() time.Time
}

func (v *Verifier) now() time.Time {
	if v.Now != nil {
		return v.Now()
	}
	return time.Now()
}

// Verify parses and validates a token, returning the identity it asserts.
func (v *Verifier) Verify(ctx context.Context, raw string) (*Identity, error) {
	claims := &Claims{}
	parser := jwt.NewParser(
		jwt.WithValidMethods([]string{"RS256"}),
		jwt.WithIssuer(v.Issuer),
		jwt.WithAudience(v.Audience),
		jwt.WithLeeway(v.Leeway),
		jwt.WithTimeFunc(v.now),
		jwt.WithExpirationRequired(),
	)
	_, err := parser.ParseWithClaims(raw, claims, func(t *jwt.Token) (any, error) {
		kid, _ := t.Header["kid"].(string)
		if kid == "" {
			return nil, errors.New("token has no kid")
		}
		return v.Keys.Key(ctx, kid)
	})
	if err != nil {
		return nil, fmt.Errorf("%w: %v", ErrTokenInvalid, err)
	}
	if claims.Subject == "" {
		return nil, fmt.Errorf("%w: no subject", ErrTokenInvalid)
	}
	if claims.TenantID == "" {
		return nil, fmt.Errorf("%w: no tenant_id claim", ErrTokenInvalid)
	}
	if !claims.Role.valid() {
		return nil, fmt.Errorf("%w: role %q", ErrTokenInvalid, claims.Role)
	}
	exp, _ := claims.GetExpirationTime()
	id := &Identity{
		UserUID:  claims.Subject,
		TenantID: claims.TenantID,
		Role:     claims.Role,
		TeamID:   claims.TeamID,
	}
	if exp != nil {
		id.ExpiresAt = exp.Time
	}
	return id, nil
}

// CertFingerprint is the device's stable identifier: the SHA-256 of its DER
// certificate, matched against devices.cert_fingerprint.
func CertFingerprint(cert *x509.Certificate) string {
	sum := sha256.Sum256(cert.Raw)
	return hex.EncodeToString(sum[:])
}

// DeviceLookup resolves a presented certificate to a device row.
type DeviceLookup interface {
	DeviceByCertFingerprint(ctx context.Context, fingerprint string) (deviceID, tenantID, status string, err error)
}

// BearerToken pulls the raw token out of an Authorization header.
func BearerToken(r *http.Request) (string, error) {
	h := r.Header.Get("Authorization")
	if h == "" {
		return "", ErrNoToken
	}
	const prefix = "Bearer "
	if len(h) <= len(prefix) || !strings.EqualFold(h[:len(prefix)], prefix) {
		return "", fmt.Errorf("%w: malformed Authorization header", ErrNoToken)
	}
	return strings.TrimSpace(h[len(prefix):]), nil
}

type ctxKey struct{}

// WithIdentity stores a verified identity on the context.
func WithIdentity(ctx context.Context, id *Identity) context.Context {
	return context.WithValue(ctx, ctxKey{}, id)
}

// FromContext returns the verified identity, or nil.
func FromContext(ctx context.Context) *Identity {
	id, _ := ctx.Value(ctxKey{}).(*Identity)
	return id
}

// MustFromContext panics when no identity is present.
//
// Reaching a handler with no identity means a route was mounted outside the
// authentication middleware. That is a wiring bug, not a runtime condition, and
// failing loudly in tests beats serving an unauthenticated request in production.
func MustFromContext(ctx context.Context) *Identity {
	id := FromContext(ctx)
	if id == nil {
		panic("auth: handler reached with no verified identity; check middleware wiring")
	}
	return id
}

// Capability names from the role matrix in spec section 13.4.
type Capability string

const (
	CapOwnCalls       Capability = "own_calls"
	CapTeamCalls      Capability = "team_calls"
	CapAllTenantCalls Capability = "all_tenant_calls"
	CapFlaggedOnly    Capability = "flagged_calls_only"
	CapResolveFlags   Capability = "resolve_flags"
	CapEditRules      Capability = "edit_rules"
	CapManageFleet    Capability = "manage_devices_users"
)

// matrix mirrors spec section 13.4 exactly. Audio playback is deliberately absent:
// it is a policy decision per tenant for agents and clients, so it is evaluated with
// the tenant row in hand rather than from the role alone (see CanPlayAudio).
var matrix = map[Capability]map[Role]bool{
	CapOwnCalls: {
		RoleAgent: true, RoleSupervisor: true, RoleQA: true, RoleCompliance: true, RoleAdmin: true,
	},
	CapTeamCalls: {
		RoleSupervisor: true, RoleQA: true, RoleCompliance: true, RoleAdmin: true,
	},
	CapAllTenantCalls: {
		RoleQA: true, RoleCompliance: true, RoleAdmin: true,
	},
	CapFlaggedOnly: {
		RoleClient: true,
	},
	CapResolveFlags: {
		RoleQA: true, RoleCompliance: true, RoleAdmin: true,
	},
	CapEditRules: {
		RoleCompliance: true, RoleAdmin: true,
	},
	CapManageFleet: {
		RoleAdmin: true,
	},
}

// Can reports whether a role holds a capability.
func (id *Identity) Can(c Capability) bool {
	if id == nil {
		return false
	}
	return matrix[c][id.Role]
}

// CanPlayAudio applies the two policy-gated cells of the matrix. Agents and bank
// clients get playback only when the tenant allows it; everyone else with call
// access always does.
func (id *Identity) CanPlayAudio(tenantAllowsAgentPlayback bool) bool {
	if id == nil {
		return false
	}
	switch id.Role {
	case RoleAgent, RoleClient:
		return tenantAllowsAgentPlayback
	case RoleSupervisor, RoleQA, RoleCompliance, RoleAdmin:
		return true
	}
	return false
}

// Require is middleware that rejects callers without a capability.
func Require(c Capability, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !FromContext(r.Context()).Can(c) {
			writeError(w, http.StatusForbidden, "forbidden", "role not permitted")
			return
		}
		next.ServeHTTP(w, r)
	})
}

func writeError(w http.ResponseWriter, status int, code, msg string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]string{"code": code, "message": msg})
}

// ------------------------------------------------------------------- JWKS

// GoogleIssuer is the Identity Platform issuer template; %s is the GCP project.
const GoogleIssuer = "https://securetoken.google.com/%s"

// GoogleJWKSURL serves the x509 certificates Identity Platform signs with.
const GoogleJWKSURL = "https://www.googleapis.com/robot/v1/metadata/x509/securetoken@system.gserviceaccount.com"

// CachingKeySource fetches and caches signing keys.
//
// It refuses to serve a key it could not refresh past the cache lifetime rather than
// serving a stale one indefinitely: an expired key set means we can no longer tell a
// valid token from a forged one, and failing closed is the only safe answer.
type CachingKeySource struct {
	URL    string
	Client *http.Client
	TTL    time.Duration
	Now    func() time.Time

	mu        sync.RWMutex
	keys      map[string]*rsa.PublicKey
	fetchedAt time.Time
}

func (c *CachingKeySource) now() time.Time {
	if c.Now != nil {
		return c.Now()
	}
	return time.Now()
}

func (c *CachingKeySource) Key(ctx context.Context, kid string) (*rsa.PublicKey, error) {
	c.mu.RLock()
	key, ok := c.keys[kid]
	fresh := c.now().Sub(c.fetchedAt) < c.ttl()
	c.mu.RUnlock()
	if ok && fresh {
		return key, nil
	}
	if err := c.refresh(ctx); err != nil {
		// An unknown kid with a stale key set is unverifiable; say so.
		if ok && fresh {
			return key, nil
		}
		return nil, err
	}
	c.mu.RLock()
	defer c.mu.RUnlock()
	if key, ok := c.keys[kid]; ok {
		return key, nil
	}
	return nil, fmt.Errorf("auth: no signing key for kid %q", kid)
}

func (c *CachingKeySource) ttl() time.Duration {
	if c.TTL == 0 {
		return time.Hour
	}
	return c.TTL
}

func (c *CachingKeySource) refresh(ctx context.Context) error {
	client := c.Client
	if client == nil {
		client = http.DefaultClient
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.URL, nil)
	if err != nil {
		return err
	}
	resp, err := client.Do(req)
	if err != nil {
		return fmt.Errorf("auth: fetch signing keys: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("auth: signing key endpoint returned %d", resp.StatusCode)
	}
	var pems map[string]string
	if err := json.NewDecoder(resp.Body).Decode(&pems); err != nil {
		return fmt.Errorf("auth: parse signing keys: %w", err)
	}
	keys := make(map[string]*rsa.PublicKey, len(pems))
	for kid, pemStr := range pems {
		cert, err := parseCertPEM(pemStr)
		if err != nil {
			continue
		}
		if pub, ok := cert.PublicKey.(*rsa.PublicKey); ok {
			keys[kid] = pub
		}
	}
	if len(keys) == 0 {
		return errors.New("auth: signing key set is empty")
	}
	c.mu.Lock()
	c.keys, c.fetchedAt = keys, c.now()
	c.mu.Unlock()
	return nil
}
