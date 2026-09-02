package auth

import (
	"context"
	"crypto/rand"
	"crypto/rsa"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/golang-jwt/jwt/v5"
)

type staticKeys struct{ key *rsa.PublicKey }

func (s staticKeys) Key(context.Context, string) (*rsa.PublicKey, error) { return s.key, nil }

type harness struct {
	priv     *rsa.PrivateKey
	verifier *Verifier
	now      time.Time
}

func newHarness(t *testing.T) *harness {
	t.Helper()
	priv, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	h := &harness{priv: priv, now: time.Date(2026, 9, 1, 10, 0, 0, 0, time.UTC)}
	h.verifier = &Verifier{
		Keys:     staticKeys{&priv.PublicKey},
		Issuer:   "https://securetoken.google.com/sentinel-prod",
		Audience: "sentinel-prod",
		Leeway:   30 * time.Second,
		Now:      func() time.Time { return h.now },
	}
	return h
}

func (h *harness) token(t *testing.T, mutate func(*Claims)) string {
	t.Helper()
	c := &Claims{
		RegisteredClaims: jwt.RegisteredClaims{
			Subject:   "agent-a",
			Issuer:    "https://securetoken.google.com/sentinel-prod",
			Audience:  jwt.ClaimStrings{"sentinel-prod"},
			ExpiresAt: jwt.NewNumericDate(h.now.Add(time.Hour)),
			IssuedAt:  jwt.NewNumericDate(h.now),
		},
		TenantID: "11111111-1111-1111-1111-111111111111",
		Role:     RoleAgent,
		TeamID:   "team-north",
	}
	if mutate != nil {
		mutate(c)
	}
	tok := jwt.NewWithClaims(jwt.SigningMethodRS256, c)
	tok.Header["kid"] = "test-key"
	s, err := tok.SignedString(h.priv)
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func TestVerifyAcceptsAWellFormedToken(t *testing.T) {
	h := newHarness(t)
	id, err := h.verifier.Verify(context.Background(), h.token(t, nil))
	if err != nil {
		t.Fatalf("verify: %v", err)
	}
	if id.UserUID != "agent-a" || id.Role != RoleAgent {
		t.Fatalf("unexpected identity %+v", id)
	}
	if id.TenantID != "11111111-1111-1111-1111-111111111111" {
		t.Fatalf("tenant not taken from the claim: %q", id.TenantID)
	}
}

func TestVerifyRejectsBadTokens(t *testing.T) {
	h := newHarness(t)
	other, _ := rsa.GenerateKey(rand.Reader, 2048)

	cases := []struct {
		name  string
		token func() string
	}{
		{"expired", func() string {
			return h.token(t, func(c *Claims) {
				c.ExpiresAt = jwt.NewNumericDate(h.now.Add(-2 * time.Hour))
			})
		}},
		{"wrong issuer", func() string {
			return h.token(t, func(c *Claims) { c.Issuer = "https://evil.example" })
		}},
		{"wrong audience", func() string {
			return h.token(t, func(c *Claims) { c.Audience = jwt.ClaimStrings{"other-project"} })
		}},
		{"no tenant claim", func() string {
			return h.token(t, func(c *Claims) { c.TenantID = "" })
		}},
		{"unknown role", func() string {
			return h.token(t, func(c *Claims) { c.Role = "superuser" })
		}},
		{"no subject", func() string {
			return h.token(t, func(c *Claims) { c.Subject = "" })
		}},
		{"signed by someone else", func() string {
			c := &Claims{RegisteredClaims: jwt.RegisteredClaims{
				Subject: "agent-a", Issuer: "https://securetoken.google.com/sentinel-prod",
				Audience:  jwt.ClaimStrings{"sentinel-prod"},
				ExpiresAt: jwt.NewNumericDate(h.now.Add(time.Hour)),
			}, TenantID: "t", Role: RoleAdmin}
			tok := jwt.NewWithClaims(jwt.SigningMethodRS256, c)
			tok.Header["kid"] = "test-key"
			s, _ := tok.SignedString(other)
			return s
		}},
		{"garbage", func() string { return "not.a.token" }},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if _, err := h.verifier.Verify(context.Background(), c.token()); err == nil {
				t.Fatal("expected rejection")
			}
		})
	}
}

func TestUnsignedTokensAreRejected(t *testing.T) {
	// The alg=none attack. jwt.WithValidMethods is what stops it; this test fails
	// loudly if that option is ever dropped.
	h := newHarness(t)
	tok := jwt.NewWithClaims(jwt.SigningMethodNone, &Claims{
		RegisteredClaims: jwt.RegisteredClaims{
			Subject: "attacker", Issuer: "https://securetoken.google.com/sentinel-prod",
			Audience:  jwt.ClaimStrings{"sentinel-prod"},
			ExpiresAt: jwt.NewNumericDate(h.now.Add(time.Hour)),
		},
		TenantID: "11111111-1111-1111-1111-111111111111", Role: RoleAdmin,
	})
	tok.Header["kid"] = "test-key"
	raw, err := tok.SignedString(jwt.UnsafeAllowNoneSignatureType)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := h.verifier.Verify(context.Background(), raw); err == nil {
		t.Fatal("an unsigned token must never verify")
	}
}

func TestClockSkewIsToleratedWithinLeeway(t *testing.T) {
	h := newHarness(t)
	// A desktop 20 s ahead of the gateway mints a token that is not yet valid.
	raw := h.token(t, func(c *Claims) {
		c.IssuedAt = jwt.NewNumericDate(h.now.Add(20 * time.Second))
		c.NotBefore = jwt.NewNumericDate(h.now.Add(20 * time.Second))
	})
	if _, err := h.verifier.Verify(context.Background(), raw); err != nil {
		t.Fatalf("30 s of leeway should absorb 20 s of skew: %v", err)
	}
}

func TestBearerToken(t *testing.T) {
	cases := map[string]struct {
		header string
		want   string
		wantOK bool
	}{
		"normal":       {"Bearer abc.def.ghi", "abc.def.ghi", true},
		"lowercase":    {"bearer abc.def.ghi", "abc.def.ghi", true},
		"missing":      {"", "", false},
		"wrong scheme": {"Basic dXNlcjpwYXNz", "", false},
		"empty value":  {"Bearer ", "", false},
	}
	for name, c := range cases {
		t.Run(name, func(t *testing.T) {
			r := httptest.NewRequest(http.MethodGet, "/v1/me/calls", nil)
			if c.header != "" {
				r.Header.Set("Authorization", c.header)
			}
			got, err := BearerToken(r)
			if c.wantOK != (err == nil) {
				t.Fatalf("err = %v, wantOK = %v", err, c.wantOK)
			}
			if got != c.want {
				t.Fatalf("got %q, want %q", got, c.want)
			}
		})
	}
}

func TestRoleMatrixMatchesTheSpec(t *testing.T) {
	// Section 13.4, transcribed. A change to the matrix has to change this table
	// too, which is the point.
	type row struct {
		cap  Capability
		want map[Role]bool
	}
	rows := []row{
		{CapOwnCalls, map[Role]bool{
			RoleAgent: true, RoleSupervisor: true, RoleQA: true,
			RoleCompliance: true, RoleAdmin: true, RoleClient: false}},
		{CapTeamCalls, map[Role]bool{
			RoleAgent: false, RoleSupervisor: true, RoleQA: true,
			RoleCompliance: true, RoleAdmin: true, RoleClient: false}},
		{CapAllTenantCalls, map[Role]bool{
			RoleAgent: false, RoleSupervisor: false, RoleQA: true,
			RoleCompliance: true, RoleAdmin: true, RoleClient: false}},
		{CapFlaggedOnly, map[Role]bool{
			RoleAgent: false, RoleSupervisor: false, RoleQA: false,
			RoleCompliance: false, RoleAdmin: false, RoleClient: true}},
		{CapResolveFlags, map[Role]bool{
			RoleAgent: false, RoleSupervisor: false, RoleQA: true,
			RoleCompliance: true, RoleAdmin: true, RoleClient: false}},
		{CapEditRules, map[Role]bool{
			RoleAgent: false, RoleSupervisor: false, RoleQA: false,
			RoleCompliance: true, RoleAdmin: true, RoleClient: false}},
		{CapManageFleet, map[Role]bool{
			RoleAgent: false, RoleSupervisor: false, RoleQA: false,
			RoleCompliance: false, RoleAdmin: true, RoleClient: false}},
	}
	for _, r := range rows {
		for role, want := range r.want {
			id := &Identity{Role: role}
			if got := id.Can(r.cap); got != want {
				t.Errorf("%s for %s: got %v, want %v", r.cap, role, got, want)
			}
		}
	}
}

func TestAudioPlaybackIsPolicyGatedForAgentsAndClients(t *testing.T) {
	for _, role := range []Role{RoleAgent, RoleClient} {
		id := &Identity{Role: role}
		if id.CanPlayAudio(false) {
			t.Errorf("%s must not get playback when the tenant forbids it", role)
		}
		if !id.CanPlayAudio(true) {
			t.Errorf("%s should get playback when the tenant allows it", role)
		}
	}
	for _, role := range []Role{RoleSupervisor, RoleQA, RoleCompliance, RoleAdmin} {
		if !(&Identity{Role: role}).CanPlayAudio(false) {
			t.Errorf("%s playback must not depend on the agent-playback policy", role)
		}
	}
	if (&Identity{Role: "unknown"}).CanPlayAudio(true) {
		t.Error("an unrecognised role must not get playback")
	}
	if (*Identity)(nil).CanPlayAudio(true) {
		t.Error("an absent identity must not get playback")
	}
}

func TestRequireRejectsAndPasses(t *testing.T) {
	next := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(299) })
	h := Require(CapEditRules, next)

	for role, wantStatus := range map[Role]int{
		RoleAdmin:      299,
		RoleCompliance: 299,
		RoleQA:         http.StatusForbidden,
		RoleAgent:      http.StatusForbidden,
	} {
		r := httptest.NewRequest(http.MethodPut, "/v1/admin/rules", nil).
			WithContext(WithIdentity(context.Background(), &Identity{Role: role}))
		w := httptest.NewRecorder()
		h.ServeHTTP(w, r)
		if w.Code != wantStatus {
			t.Errorf("%s: got %d, want %d", role, w.Code, wantStatus)
		}
	}

	// No identity at all must be a refusal, not a pass-through.
	w := httptest.NewRecorder()
	h.ServeHTTP(w, httptest.NewRequest(http.MethodPut, "/v1/admin/rules", nil))
	if w.Code != http.StatusForbidden {
		t.Fatalf("unauthenticated request got %d", w.Code)
	}
}

func TestMustFromContextPanicsOnMisconfiguredRoutes(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatal("expected a panic so the wiring bug is caught in tests")
		}
	}()
	MustFromContext(context.Background())
}
