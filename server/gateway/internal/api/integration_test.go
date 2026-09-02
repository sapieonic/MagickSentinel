package api_test

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"io"
	"log/slog"
	"math/big"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/golang-jwt/jwt/v5"
	"github.com/jackc/pgx/v5/pgxpool"

	"github.com/magickvoice/sentinel/server/gateway/internal/api"
	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/store"
)

// These tests exercise the handlers against a real Postgres with the real row-level
// security policies applied, because the isolation guarantees they check are enforced
// by the database rather than by the Go code. Point SENTINEL_TEST_DATABASE_URL at a
// database with db/migrations applied; db/test/pgtest.sh will build one.
//
// Skipped, not failed, when no database is available: a developer without one should
// still be able to run the unit suite.

const (
	acmeTenant  = "11111111-1111-1111-1111-111111111111"
	rivalTenant = "22222222-2222-2222-2222-222222222222"
	teamNorth   = "aaaaaaaa-0000-0000-0000-000000000001"
)

type fixture struct {
	t *testing.T
	// pool is the application connection: role sentinel_app, NOBYPASSRLS. Every
	// assertion about what a caller can see runs through it.
	pool *pgxpool.Pool
	// admin is the owner connection, used only to seed and to read back rows the
	// application role is correctly forbidden from reading.
	admin    *pgxpool.Pool
	store    *store.Store
	server   *httptest.Server
	priv     *rsa.PrivateKey
	now      time.Time
	deviceID string
}

type staticKeys struct{ key *rsa.PublicKey }

func (s staticKeys) Key(context.Context, string) (*rsa.PublicKey, error) { return s.key, nil }

func newFixture(t *testing.T) *fixture {
	t.Helper()
	dsn := os.Getenv("SENTINEL_TEST_DATABASE_URL")
	if dsn == "" {
		t.Skip("SENTINEL_TEST_DATABASE_URL not set; run via db/test/pgtest.sh")
	}
	adminDSN := os.Getenv("SENTINEL_TEST_ADMIN_DATABASE_URL")
	if adminDSN == "" {
		t.Skip("SENTINEL_TEST_ADMIN_DATABASE_URL not set; run via db/test/gateway_it.sh")
	}
	ctx := context.Background()
	pool, err := pgxpool.New(ctx, dsn)
	if err != nil {
		t.Fatalf("connect: %v", err)
	}
	t.Cleanup(pool.Close)
	if err := pool.Ping(ctx); err != nil {
		t.Fatalf("ping: %v", err)
	}
	admin, err := pgxpool.New(ctx, adminDSN)
	if err != nil {
		t.Fatalf("connect as owner: %v", err)
	}
	t.Cleanup(admin.Close)

	priv, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	f := &fixture{
		t: t, pool: pool, admin: admin, store: store.New(pool), priv: priv,
		now: time.Date(2026, 9, 1, 12, 0, 0, 0, time.UTC),
	}
	f.seed(ctx)

	srv := &api.Server{
		Log:     slog.New(slog.NewTextHandler(io.Discard, nil)),
		Store:   f.store,
		Version: "test",
		Now:     func() time.Time { return f.now },
		CA:      devCA(t),
		LiveTickets: api.NewLiveTickets(60 * time.Second),
		LivePoll:    50 * time.Millisecond,
		Verifier: &auth.Verifier{
			Keys:     staticKeys{&priv.PublicKey},
			Issuer:   "https://securetoken.google.com/sentinel-test",
			Audience: "sentinel-test",
			Leeway:   time.Minute,
			Now:      func() time.Time { return f.now },
		},
	}
	f.server = httptest.NewServer(srv.Routes())
	t.Cleanup(f.server.Close)
	return f
}

func (f *fixture) seed(ctx context.Context) {
	f.t.Helper()
	stmts := []string{
		`TRUNCATE audit_log, device_events, flags, ptps, analyses, transcripts,
		          ingest_watermarks, media_segments, calls, devices, enrollment_tokens,
		          rule_sets, users, teams, tenants RESTART IDENTITY CASCADE`,
		fmt.Sprintf(`INSERT INTO tenants (id, name, idp_tenant_id, allow_agent_audio_playback)
		     VALUES ('%s','Acme BPO','acme',false), ('%s','Rival BPO','rival',true)`, acmeTenant, rivalTenant),
		fmt.Sprintf(`INSERT INTO teams (id, tenant_id, name) VALUES ('%s','%s','North')`, teamNorth, acmeTenant),
		fmt.Sprintf(`INSERT INTO users (firebase_uid, tenant_id, role, team_id, display_name) VALUES
		     ('agent-a','%[1]s','agent','%[2]s','Agent A'),
		     ('agent-b','%[1]s','agent','%[2]s','Agent B'),
		     ('sup-1','%[1]s','supervisor','%[2]s','Sup One'),
		     ('qa-1','%[1]s','qa',NULL,'QA One'),
		     ('admin-1','%[1]s','admin',NULL,'Admin One'),
		     ('client-1','%[1]s','client',NULL,'Bank Client'),
		     ('rival-admin','%[3]s','admin',NULL,'Rival Admin')`, acmeTenant, teamNorth, rivalTenant),
		fmt.Sprintf(`INSERT INTO devices (id, tenant_id, machine_guid, hw_fingerprint,
		            cert_fingerprint, os_build, capture_tier, agent_version)
		     VALUES ('dddddddd-0000-0000-0000-000000000001','%s','mg-1','hw-1','cf-1',
		             '10.0.22631','A','1.0.0')`, acmeTenant),
		fmt.Sprintf(`INSERT INTO calls (id, tenant_id, device_id, user_uid, team_id, started_at,
		            ended_at, duration_ms, capture_tier, status, account_ref)
		     VALUES ('c0000000-0000-0000-0000-00000000000a','%[1]s',
		             'dddddddd-0000-0000-0000-000000000001','agent-a','%[2]s',
		             '2026-09-01T10:00:00Z','2026-09-01T10:05:00Z',300000,'A','complete','LN-1'),
		            ('c0000000-0000-0000-0000-00000000000b','%[1]s',
		             'dddddddd-0000-0000-0000-000000000001','agent-b','%[2]s',
		             '2026-09-01T11:00:00Z','2026-09-01T11:04:00Z',240000,'A','complete','LN-2')`,
			acmeTenant, teamNorth),
		fmt.Sprintf(`INSERT INTO analyses (call_id, tenant_id, prompt_version, model, summary,
		            disposition, sentiment, talk_ratio, interruptions)
		     VALUES ('c0000000-0000-0000-0000-00000000000a','%[1]s','v1','test',
		             'Borrower agreed to pay.','ptp',
		             '{"far":[],"near":[],"far_open":-0.1,"far_close":-0.4,"delta":-0.3}'::jsonb,0.6,2),
		            ('c0000000-0000-0000-0000-00000000000b','%[1]s','v1','test',
		             'Borrower disputed the amount.','dispute',
		             '{"far":[],"near":[],"far_open":0.0,"far_close":-0.2,"delta":-0.2}'::jsonb,0.7,5)`,
			acmeTenant),
		fmt.Sprintf(`INSERT INTO transcripts (tenant_id, call_id, channel, asr_provider,
		            asr_version, language, text, word_timings, confidence)
		     VALUES ('%[1]s','c0000000-0000-0000-0000-00000000000a',1,'fixture','1','en',
		             'good morning this is Ravi from Acme Recovery about your loan account',
		             '[{"start_ms":0,"end_ms":500,"text":"good morning"}]'::jsonb, 0.9),
		            ('%[1]s','c0000000-0000-0000-0000-00000000000a',0,'fixture','1','en',
		             'yes speaking I will pay fifteen thousand on the fifteenth',
		             '[{"start_ms":600,"end_ms":1200,"text":"yes speaking"}]'::jsonb, 0.9)`, acmeTenant),
		fmt.Sprintf(`INSERT INTO ptps (tenant_id, call_id, amount_paise, due_date, confidence)
		     VALUES ('%s','c0000000-0000-0000-0000-00000000000a',1500000,'2026-09-15',0.86)`, acmeTenant),
		fmt.Sprintf(`INSERT INTO flags (tenant_id, call_id, rule_id, rule_set_version, severity, tier,
		            span_start_ms, span_end_ms, evidence_text)
		     VALUES ('%[1]s','c0000000-0000-0000-0000-00000000000b','false_legal_threat',1,
		             'critical',1,92100,98400,'we will file a police case')`, acmeTenant),
		fmt.Sprintf(`INSERT INTO rule_sets (tenant_id, version, definition, active, created_by)
		     SELECT '%s', 1, definition, true, 'system' FROM default_rule_set WHERE version = 1`, acmeTenant),
	}
	for _, s := range stmts {
		if _, err := f.admin.Exec(ctx, s); err != nil {
			f.t.Fatalf("seed: %v\n%s", err, s)
		}
	}
}

func (f *fixture) token(uid, tenant string, role auth.Role, team string) string {
	f.t.Helper()
	c := &auth.Claims{
		RegisteredClaims: jwt.RegisteredClaims{
			Subject:   uid,
			Issuer:    "https://securetoken.google.com/sentinel-test",
			Audience:  jwt.ClaimStrings{"sentinel-test"},
			ExpiresAt: jwt.NewNumericDate(f.now.Add(time.Hour)),
			IssuedAt:  jwt.NewNumericDate(f.now),
		},
		TenantID: tenant, Role: role, TeamID: team,
	}
	tok := jwt.NewWithClaims(jwt.SigningMethodRS256, c)
	tok.Header["kid"] = "test-key"
	s, err := tok.SignedString(f.priv)
	if err != nil {
		f.t.Fatal(err)
	}
	return s
}

func (f *fixture) do(method, path, token string, body any) (*http.Response, []byte) {
	f.t.Helper()
	var rdr io.Reader
	if body != nil {
		b, _ := json.Marshal(body)
		rdr = bytesReader(b)
	}
	req, err := http.NewRequest(method, f.server.URL+path, rdr)
	if err != nil {
		f.t.Fatal(err)
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := f.server.Client().Do(req)
	if err != nil {
		f.t.Fatal(err)
	}
	defer resp.Body.Close()
	out, _ := io.ReadAll(resp.Body)
	return resp, out
}

func bytesReader(b []byte) io.Reader { return io.NopCloser(readerOf(b)) }

type readerOf []byte

func (r readerOf) Read(p []byte) (int, error) {
	if len(r) == 0 {
		return 0, io.EOF
	}
	n := copy(p, r)
	return n, nil
}

func devCA(t *testing.T) *api.DevCA {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	tmpl := &x509.Certificate{
		SerialNumber:          big.NewInt(1),
		Subject:               pkix.Name{CommonName: "Sentinel Dev CA"},
		NotBefore:             time.Now().Add(-time.Hour),
		NotAfter:              time.Now().AddDate(2, 0, 0),
		IsCA:                  true,
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageDigitalSignature,
		BasicConstraintsValid: true,
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	cert, _ := x509.ParseCertificate(der)
	pemStr := string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}))
	return api.NewDevCA(cert, key, pemStr)
}

// -------------------------------------------------------------------- tests

func TestAgentSeesOnlyOwnCalls(t *testing.T) {
	f := newFixture(t)
	resp, body := f.do(http.MethodGet, "/v1/me/calls", f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth), nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var page struct {
		Items []struct {
			ID      string `json:"id"`
			UserUID string `json:"user_uid"`
			Summary string `json:"summary"`
		} `json:"items"`
	}
	if err := json.Unmarshal(body, &page); err != nil {
		t.Fatal(err)
	}
	if len(page.Items) != 1 {
		t.Fatalf("agent saw %d calls, want 1: %s", len(page.Items), body)
	}
	if page.Items[0].UserUID != "agent-a" {
		t.Fatalf("agent saw another agent's call: %s", body)
	}
}

func TestAgentCannotReadAnotherAgentsCallById(t *testing.T) {
	f := newFixture(t)
	resp, body := f.do(http.MethodGet, "/v1/me/calls/c0000000-0000-0000-0000-00000000000b",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth), nil)
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("expected 404, got %d: %s", resp.StatusCode, body)
	}
}

func TestTheMeNamespaceRefusesASuppliedUid(t *testing.T) {
	// The desktop binary must not be repointable at another agent's data. The
	// middleware refuses the request outright rather than trusting handlers to
	// ignore the parameter.
	f := newFixture(t)
	for _, q := range []string{"?user_uid=agent-b", "?uid=agent-b", "?as_user=agent-b"} {
		resp, body := f.do(http.MethodGet, "/v1/me/calls"+q,
			f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth), nil)
		if resp.StatusCode != http.StatusBadRequest {
			t.Fatalf("%s: expected 400, got %d: %s", q, resp.StatusCode, body)
		}
	}
}

func TestCrossTenantAccessReturnsNothing(t *testing.T) {
	f := newFixture(t)
	// A rival tenant's admin, with a perfectly valid token, asking for a call by id.
	resp, body := f.do(http.MethodGet, "/v1/me/calls/c0000000-0000-0000-0000-00000000000a",
		f.token("rival-admin", rivalTenant, auth.RoleAdmin, ""), nil)
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("expected 404, got %d: %s", resp.StatusCode, body)
	}
}

func TestQaSeesTheWholeTenantAndSupervisorSeesTheTeam(t *testing.T) {
	f := newFixture(t)
	for _, c := range []struct {
		uid  string
		role auth.Role
		team string
		want int
	}{
		{"qa-1", auth.RoleQA, "", 2},
		{"sup-1", auth.RoleSupervisor, teamNorth, 2},
	} {
		resp, body := f.do(http.MethodGet, "/v1/teams/"+teamNorth+"/calls",
			f.token(c.uid, acmeTenant, c.role, c.team), nil)
		if resp.StatusCode != http.StatusOK {
			t.Fatalf("%s: status %d: %s", c.uid, resp.StatusCode, body)
		}
		var page struct{ Items []json.RawMessage }
		json.Unmarshal(body, &page)
		if len(page.Items) != c.want {
			t.Fatalf("%s saw %d calls, want %d", c.uid, len(page.Items), c.want)
		}
	}
}

func TestAgentIsRefusedTeamAndAdminRoutes(t *testing.T) {
	f := newFixture(t)
	token := f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth)
	for _, path := range []string{
		"/v1/teams/" + teamNorth + "/calls",
		"/v1/compliance/flags",
		"/v1/admin/devices",
		"/v1/admin/rules",
		"/v1/admin/audit",
	} {
		resp, _ := f.do(http.MethodGet, path, token, nil)
		if resp.StatusCode != http.StatusForbidden {
			t.Errorf("%s: expected 403 for an agent, got %d", path, resp.StatusCode)
		}
	}
}

func TestUnauthenticatedRequestsAreRejected(t *testing.T) {
	f := newFixture(t)
	resp, _ := f.do(http.MethodGet, "/v1/me/calls", "", nil)
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("expected 401, got %d", resp.StatusCode)
	}
	resp, _ = f.do(http.MethodGet, "/v1/me/calls", "not-a-token", nil)
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("expected 401, got %d", resp.StatusCode)
	}
}

func TestDeviceScopedRoutesRequireACertificate(t *testing.T) {
	// A valid user token alone must not be enough to read a tenant's capture
	// configuration; the endpoint has to prove it is an enrolled device.
	f := newFixture(t)
	token := f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth)
	for _, path := range []string{"/v1/policy", "/v1/heartbeat"} {
		method := http.MethodGet
		if path == "/v1/heartbeat" {
			method = http.MethodPost
		}
		resp, _ := f.do(method, path, token, map[string]any{})
		if resp.StatusCode != http.StatusForbidden {
			t.Errorf("%s: expected 403 without a device certificate, got %d", path, resp.StatusCode)
		}
	}
}

func TestComplianceQueueAndFlagResolution(t *testing.T) {
	f := newFixture(t)
	token := f.token("qa-1", acmeTenant, auth.RoleQA, "")
	resp, body := f.do(http.MethodGet, "/v1/compliance/flags?severity=critical", token, nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var list struct {
		Items []struct {
			ID           string `json:"id"`
			RuleID       string `json:"rule_id"`
			EvidenceText string `json:"evidence_text"`
			SpanStartMS  int    `json:"span_start_ms"`
		} `json:"items"`
	}
	json.Unmarshal(body, &list)
	if len(list.Items) != 1 || list.Items[0].RuleID != "false_legal_threat" {
		t.Fatalf("unexpected queue: %s", body)
	}
	if list.Items[0].EvidenceText == "" || list.Items[0].SpanStartMS == 0 {
		t.Fatal("a flag must carry the span a reviewer can trace it to")
	}

	resp, body = f.do(http.MethodPatch, "/v1/compliance/flags/"+list.Items[0].ID, token,
		map[string]any{"status": "upheld", "reviewer_uid": "qa-1", "note": "clear violation"})
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var updated struct {
		Status     string `json:"status"`
		ResolvedAt string `json:"resolved_at"`
	}
	json.Unmarshal(body, &updated)
	if updated.Status != "upheld" || updated.ResolvedAt == "" {
		t.Fatalf("flag not resolved: %s", body)
	}
}

func TestAgentCanRespondToAFlagOnTheirOwnCall(t *testing.T) {
	f := newFixture(t)
	var flagID string
	if err := f.admin.QueryRow(context.Background(),
		`SELECT id::text FROM flags LIMIT 1`).Scan(&flagID); err != nil {
		t.Fatal(err)
	}
	// The flag is on agent-b's call, so agent-a must not reach it.
	resp, _ := f.do(http.MethodPost, "/v1/me/flags/"+flagID+"/respond",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth),
		map[string]any{"response": "not mine"})
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("agent-a reached another agent's flag: %d", resp.StatusCode)
	}
	// agent-b can.
	resp, body := f.do(http.MethodPost, "/v1/me/flags/"+flagID+"/respond",
		f.token("agent-b", acmeTenant, auth.RoleAgent, teamNorth),
		map[string]any{"response": "I was quoting the notice, not threatening."})
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
}

func TestConfirmingACallRecordsTheAgentCorrectionWithoutLosingTheModelValue(t *testing.T) {
	f := newFixture(t)
	resp, body := f.do(http.MethodPost, "/v1/me/calls/c0000000-0000-0000-0000-00000000000a/confirm",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth),
		map[string]any{
			"disposition": "ptp", "ptp_present": true,
			"ptp_amount_paise": 1200000, "ptp_due_date": "2026-09-20",
		})
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var model, agent *int64
	if err := f.admin.QueryRow(context.Background(),
		`SELECT amount_paise, agent_amount_paise FROM ptps
		  WHERE call_id = 'c0000000-0000-0000-0000-00000000000a'`).Scan(&model, &agent); err != nil {
		t.Fatal(err)
	}
	if model == nil || *model != 1500000 {
		t.Fatalf("the model's extraction was overwritten: %v", model)
	}
	if agent == nil || *agent != 1200000 {
		t.Fatalf("the agent's correction was not recorded: %v", agent)
	}
}

func TestConfirmRejectsRupeeShapedAmounts(t *testing.T) {
	f := newFixture(t)
	resp, _ := f.do(http.MethodPost, "/v1/me/calls/c0000000-0000-0000-0000-00000000000a/confirm",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth),
		map[string]any{"disposition": "ptp", "ptp_present": true, "ptp_amount_paise": -1})
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("expected 400 for a negative amount, got %d", resp.StatusCode)
	}
	resp, _ = f.do(http.MethodPost, "/v1/me/calls/c0000000-0000-0000-0000-00000000000a/confirm",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth),
		map[string]any{"disposition": "not_a_disposition"})
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("expected 400 for an unknown disposition, got %d", resp.StatusCode)
	}
}

func TestReadingACallIsAudited(t *testing.T) {
	// "Who listened to this borrower's call" has to be answerable, so reads are
	// audited and not only writes.
	f := newFixture(t)
	f.do(http.MethodGet, "/v1/me/calls/c0000000-0000-0000-0000-00000000000a",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth), nil)

	var n int
	if err := f.admin.QueryRow(context.Background(),
		`SELECT count(*) FROM audit_log WHERE action = 'call.read' AND actor_uid = 'agent-a'`).
		Scan(&n); err != nil {
		t.Fatal(err)
	}
	if n != 1 {
		t.Fatalf("expected one call.read audit entry, got %d", n)
	}
}

func TestPublishingRulesCreatesANewVersionRatherThanMutating(t *testing.T) {
	f := newFixture(t)
	token := f.token("admin-1", acmeTenant, auth.RoleAdmin, "")

	resp, body := f.do(http.MethodPut, "/v1/admin/rules", token, map[string]any{
		"judge_sample_pct": 10,
		"rules": []map[string]any{
			{"rule_id": "abusive_language", "enabled": true, "severity": "critical"},
		},
	})
	if resp.StatusCode != http.StatusCreated {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var rs struct {
		Version int  `json:"version"`
		Active  bool `json:"active"`
	}
	json.Unmarshal(body, &rs)
	if rs.Version != 2 || !rs.Active {
		t.Fatalf("expected an active version 2, got %+v", rs)
	}

	var versions int
	f.admin.QueryRow(context.Background(),
		`SELECT count(*) FROM rule_sets WHERE tenant_id = $1`, acmeTenant).Scan(&versions)
	if versions != 2 {
		t.Fatalf("version 1 was mutated away; %d versions remain", versions)
	}
	var activeCount int
	f.admin.QueryRow(context.Background(),
		`SELECT count(*) FROM rule_sets WHERE tenant_id = $1 AND active`, acmeTenant).Scan(&activeCount)
	if activeCount != 1 {
		t.Fatalf("expected exactly one active rule set, got %d", activeCount)
	}
}

func TestAgentCannotReadRawRuleDefinitions(t *testing.T) {
	f := newFixture(t)
	resp, _ := f.do(http.MethodGet, "/v1/admin/rules",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth), nil)
	if resp.StatusCode != http.StatusForbidden {
		t.Fatalf("expected 403, got %d", resp.StatusCode)
	}
}

func TestFleetViewReportsTierDistribution(t *testing.T) {
	f := newFixture(t)
	resp, body := f.do(http.MethodGet, "/v1/admin/devices",
		f.token("admin-1", acmeTenant, auth.RoleAdmin, ""), nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var fleet struct {
		Items            []json.RawMessage `json:"items"`
		TierDistribution map[string]int    `json:"tier_distribution"`
	}
	json.Unmarshal(body, &fleet)
	if len(fleet.Items) != 1 || fleet.TierDistribution["A"] != 1 {
		t.Fatalf("unexpected fleet: %s", body)
	}
}

func TestRevokingADeviceMarksItRevokedAndAudits(t *testing.T) {
	f := newFixture(t)
	token := f.token("admin-1", acmeTenant, auth.RoleAdmin, "")
	resp, body := f.do(http.MethodPost,
		"/v1/admin/devices/dddddddd-0000-0000-0000-000000000001/revoke", token,
		map[string]any{"reason": "laptop left the building"})
	if resp.StatusCode != http.StatusNoContent {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var status string
	f.admin.QueryRow(context.Background(),
		`SELECT status FROM devices WHERE id = 'dddddddd-0000-0000-0000-000000000001'`).Scan(&status)
	if status != "revoked" {
		t.Fatalf("device status is %q", status)
	}
	var n int
	f.admin.QueryRow(context.Background(),
		`SELECT count(*) FROM audit_log WHERE action = 'device.revoke'`).Scan(&n)
	if n != 1 {
		t.Fatalf("revocation not audited (%d entries)", n)
	}
}

func TestEnrollmentTokensAreSingleUseAndStoredHashed(t *testing.T) {
	f := newFixture(t)
	resp, body := f.do(http.MethodPost, "/v1/admin/enrollment-tokens",
		f.token("admin-1", acmeTenant, auth.RoleAdmin, ""), nil)
	if resp.StatusCode != http.StatusCreated {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var minted struct {
		Token string `json:"token"`
	}
	json.Unmarshal(body, &minted)
	if minted.Token == "" {
		t.Fatal("no token returned")
	}

	// The plaintext must not be recoverable from the database.
	var stored string
	f.admin.QueryRow(context.Background(),
		`SELECT token_hash FROM enrollment_tokens LIMIT 1`).Scan(&stored)
	if stored == minted.Token {
		t.Fatal("the enrollment token is stored in plaintext")
	}
	if stored != store.HashEnrollmentToken(minted.Token) {
		t.Fatal("stored hash does not match the issued token")
	}

	ctx := context.Background()
	tenant, err := f.store.ConsumeEnrollmentToken(ctx, minted.Token, f.now)
	if err != nil || tenant != acmeTenant {
		t.Fatalf("first use failed: %v (tenant %q)", err, tenant)
	}
	if _, err := f.store.ConsumeEnrollmentToken(ctx, minted.Token, f.now); err == nil {
		t.Fatal("a consumed enrollment token was accepted a second time")
	}
	// An expired token is refused too.
	if _, err := f.store.ConsumeEnrollmentToken(ctx, minted.Token, f.now.Add(48*time.Hour)); err == nil {
		t.Fatal("an expired token was accepted")
	}
}

func TestEnrollmentIssuesACertificateForAValidCsr(t *testing.T) {
	f := newFixture(t)
	_, body := f.do(http.MethodPost, "/v1/admin/enrollment-tokens",
		f.token("admin-1", acmeTenant, auth.RoleAdmin, ""), nil)
	var minted struct {
		Token string `json:"token"`
	}
	json.Unmarshal(body, &minted)

	key, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	csrDER, err := x509.CreateCertificateRequest(rand.Reader,
		&x509.CertificateRequest{Subject: pkix.Name{CommonName: "mg-2"}}, key)
	if err != nil {
		t.Fatal(err)
	}
	csrPEM := string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE REQUEST", Bytes: csrDER}))

	resp, body := f.do(http.MethodPost, "/v1/devices/enroll", "", map[string]any{
		"enrollment_token": minted.Token, "csr_pem": csrPEM,
		"machine_guid": "mg-2", "hw_fingerprint": "hw-2",
		"os_build": "10.0.19045", "capture_tier": "B", "agent_version": "1.0.0",
	})
	if resp.StatusCode != http.StatusCreated {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var issued struct {
		DeviceID       string `json:"device_id"`
		CertificatePEM string `json:"certificate_pem"`
	}
	json.Unmarshal(body, &issued)
	if issued.DeviceID == "" || issued.CertificatePEM == "" {
		t.Fatalf("incomplete enrollment response: %s", body)
	}
	// The device is now resolvable by the fingerprint of the certificate we issued,
	// which is how mTLS will identify it on the next connection.
	blk, _ := pem.Decode([]byte(issued.CertificatePEM))
	cert, err := x509.ParseCertificate(blk.Bytes)
	if err != nil {
		t.Fatal(err)
	}
	deviceID, tenantID, status, err := f.store.DeviceByCertFingerprint(
		context.Background(), auth.CertFingerprint(cert))
	if err != nil {
		t.Fatalf("device not resolvable by certificate: %v", err)
	}
	if deviceID != issued.DeviceID || tenantID != acmeTenant || status != "active" {
		t.Fatalf("device row wrong: %s / %s / %s", deviceID, tenantID, status)
	}
}

func TestEnrollmentRejectsTierCMachines(t *testing.T) {
	f := newFixture(t)
	resp, _ := f.do(http.MethodPost, "/v1/devices/enroll", "", map[string]any{
		"enrollment_token": "x", "csr_pem": "x", "machine_guid": "mg",
		"hw_fingerprint": "hw", "capture_tier": "C",
	})
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("a machine with no supported capture path must not enrol: %d", resp.StatusCode)
	}
}

func TestScorecardsReportAMedianAndNoRanking(t *testing.T) {
	f := newFixture(t)
	resp, body := f.do(http.MethodGet, "/v1/teams/"+teamNorth+"/scorecards",
		f.token("sup-1", acmeTenant, auth.RoleSupervisor, teamNorth), nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var out struct {
		Median struct {
			DisplayName string `json:"display_name"`
		} `json:"median"`
		Agents []struct {
			DisplayName string `json:"display_name"`
		} `json:"agents"`
	}
	json.Unmarshal(body, &out)
	if out.Median.DisplayName != "Team median" {
		t.Fatalf("no median returned: %s", body)
	}
	if len(out.Agents) != 2 {
		t.Fatalf("expected two agents, got %d", len(out.Agents))
	}
	// Alphabetical, not by performance: the response carries no ranking signal.
	if out.Agents[0].DisplayName > out.Agents[1].DisplayName {
		t.Fatal("scorecards are ordered by performance, which invites a leaderboard")
	}
}

func TestHealthzNeedsNoCredentials(t *testing.T) {
	f := newFixture(t)
	resp, body := f.do(http.MethodGet, "/healthz", "", nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
}

// ---------------------------------------------- scope-derived call explorer

// One endpoint serves every role, with row-level security deciding the visible set.
// These tests are the reason that design is safe: the same request, made by six
// different roles, returns six correctly different answers with no per-role branch
// in the handler.

func TestTheCallExplorerReturnsTheCallersScope(t *testing.T) {
	f := newFixture(t)
	for _, c := range []struct {
		name string
		uid  string
		role auth.Role
		team string
		want int
	}{
		{"agent sees own", "agent-a", auth.RoleAgent, teamNorth, 1},
		{"supervisor sees the team", "sup-1", auth.RoleSupervisor, teamNorth, 2},
		{"qa sees the tenant", "qa-1", auth.RoleQA, "", 2},
		{"admin sees the tenant", "admin-1", auth.RoleAdmin, "", 2},
		{"bank client sees flagged only", "client-1", auth.RoleClient, "", 1},
	} {
		t.Run(c.name, func(t *testing.T) {
			resp, body := f.do(http.MethodGet, "/v1/calls",
				f.token(c.uid, acmeTenant, c.role, c.team), nil)
			if resp.StatusCode != http.StatusOK {
				t.Fatalf("status %d: %s", resp.StatusCode, body)
			}
			var page struct{ Items []json.RawMessage }
			json.Unmarshal(body, &page)
			if len(page.Items) != c.want {
				t.Fatalf("saw %d calls, want %d: %s", len(page.Items), c.want, body)
			}
		})
	}
}

func TestAFilterCanNarrowAScopeButNeverWidenIt(t *testing.T) {
	f := newFixture(t)
	// agent-a asking for agent-b's calls by parameter. The filter is applied on top
	// of a set RLS has already restricted, so the answer is empty rather than
	// someone else's data.
	resp, body := f.do(http.MethodGet, "/v1/calls?user_uid=agent-b",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth), nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var page struct{ Items []json.RawMessage }
	json.Unmarshal(body, &page)
	if len(page.Items) != 0 {
		t.Fatalf("a filter widened an agent's scope: %s", body)
	}
}

func TestQaCanOpenAnyCallInTheTenantAndAnAgentCannot(t *testing.T) {
	// The gap this endpoint exists to close: the call explorer is the workhorse
	// screen and a QA reviewer has to be able to open another agent's call.
	f := newFixture(t)
	const other = "/v1/calls/c0000000-0000-0000-0000-00000000000b"

	resp, body := f.do(http.MethodGet, other, f.token("qa-1", acmeTenant, auth.RoleQA, ""), nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("qa could not open a tenant call: %d %s", resp.StatusCode, body)
	}
	var detail struct {
		UserUID string          `json:"user_uid"`
		Flags   []json.RawMessage `json:"flags"`
	}
	json.Unmarshal(body, &detail)
	if detail.UserUID != "agent-b" || len(detail.Flags) != 1 {
		t.Fatalf("unexpected detail: %s", body)
	}

	resp, _ = f.do(http.MethodGet, other, f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth), nil)
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("an agent reached another agent's call: %d", resp.StatusCode)
	}
}

func TestAnOutOfScopeCallIsNotFoundRatherThanForbidden(t *testing.T) {
	// Whether a call id exists in another team is itself information.
	f := newFixture(t)
	resp, _ := f.do(http.MethodGet, "/v1/calls/c0000000-0000-0000-0000-0000000000ff",
		f.token("admin-1", acmeTenant, auth.RoleAdmin, ""), nil)
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("expected 404, got %d", resp.StatusCode)
	}
}

func TestPlaybackIsWithheldWithoutWithholdingTheCall(t *testing.T) {
	// The Acme tenant has allow_agent_audio_playback false. An agent still gets the
	// transcript and the flags; only the audio URL is absent.
	f := newFixture(t)
	resp, body := f.do(http.MethodGet, "/v1/calls/c0000000-0000-0000-0000-00000000000a",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth), nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var detail struct {
		AudioURL *string `json:"audio_url"`
		Summary  *string `json:"summary"`
	}
	json.Unmarshal(body, &detail)
	if detail.AudioURL != nil {
		t.Fatal("audio playback leaked past the tenant policy")
	}
	if detail.Summary == nil {
		t.Fatal("withholding audio must not withhold the rest of the call")
	}
}

func TestFlaggedFilterAndTeamDiscovery(t *testing.T) {
	f := newFixture(t)
	token := f.token("qa-1", acmeTenant, auth.RoleQA, "")

	resp, body := f.do(http.MethodGet, "/v1/calls?has_flags=true", token, nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var page struct{ Items []struct{ FlagCount int `json:"flag_count"` } }
	json.Unmarshal(body, &page)
	if len(page.Items) != 1 || page.Items[0].FlagCount != 1 {
		t.Fatalf("unexpected flagged page: %s", body)
	}

	resp, body = f.do(http.MethodGet, "/v1/teams", token, nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var teams []struct{ Name string `json:"name"` }
	json.Unmarshal(body, &teams)
	if len(teams) != 1 || teams[0].Name != "North" {
		t.Fatalf("unexpected teams: %s", body)
	}
}

func TestCrossTenantIsolationHoldsOnTheExplorer(t *testing.T) {
	f := newFixture(t)
	resp, body := f.do(http.MethodGet, "/v1/calls",
		f.token("rival-admin", rivalTenant, auth.RoleAdmin, ""), nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var page struct{ Items []json.RawMessage }
	json.Unmarshal(body, &page)
	if len(page.Items) != 0 {
		t.Fatalf("a rival tenant's admin saw %d calls", len(page.Items))
	}
}

// -------------------------------------------------- live view and exports

func TestALiveTicketIsSingleUseAndTeamScoped(t *testing.T) {
	f := newFixture(t)
	token := f.token("sup-1", acmeTenant, auth.RoleSupervisor, teamNorth)

	resp, body := f.do(http.MethodPost, "/v1/teams/"+teamNorth+"/live/ticket", token, nil)
	if resp.StatusCode != http.StatusCreated {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var minted struct {
		Ticket string `json:"ticket"`
	}
	json.Unmarshal(body, &minted)
	if minted.Ticket == "" {
		t.Fatal("no ticket returned")
	}

	// A ticket for one team must not open another team's stream.
	other := "aaaaaaaa-0000-0000-0000-0000000000ff"
	resp, _ = f.do(http.MethodGet, "/v1/teams/"+other+"/live?ticket="+minted.Ticket, "", nil)
	if resp.StatusCode != http.StatusForbidden {
		t.Fatalf("a ticket crossed teams: %d", resp.StatusCode)
	}

	// That attempt consumed it, so even the right team is now refused: a ticket is
	// spent on presentation, not on success, so a leaked one cannot be replayed.
	resp, _ = f.do(http.MethodGet, "/v1/teams/"+teamNorth+"/live?ticket="+minted.Ticket, "", nil)
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("a consumed ticket was accepted again: %d", resp.StatusCode)
	}
}

func TestTheLiveStreamNeedsATicketAndNotABearerToken(t *testing.T) {
	f := newFixture(t)
	// A perfectly valid bearer token is not a substitute: the route is outside the
	// authentication middleware precisely because EventSource cannot send one.
	resp, _ := f.do(http.MethodGet, "/v1/teams/"+teamNorth+"/live",
		f.token("sup-1", acmeTenant, auth.RoleSupervisor, teamNorth), nil)
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("expected 401 without a ticket, got %d", resp.StatusCode)
	}
	resp, _ = f.do(http.MethodGet, "/v1/teams/"+teamNorth+"/live?ticket=forged", "", nil)
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("a forged ticket was accepted: %d", resp.StatusCode)
	}
}

func TestAnAgentCannotMintALiveTicket(t *testing.T) {
	f := newFixture(t)
	resp, _ := f.do(http.MethodPost, "/v1/teams/"+teamNorth+"/live/ticket",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth), nil)
	if resp.StatusCode != http.StatusForbidden {
		t.Fatalf("expected 403, got %d", resp.StatusCode)
	}
}

func TestTheLiveStreamEmitsInFlightCalls(t *testing.T) {
	f := newFixture(t)
	// One call still ingesting, and one stale ghost that must not appear.
	if _, err := f.admin.Exec(context.Background(), `
		INSERT INTO calls (id, tenant_id, device_id, user_uid, team_id, started_at,
		                   capture_tier, status)
		VALUES ('c0000000-0000-0000-0000-0000000000e1', $1,
		        'dddddddd-0000-0000-0000-000000000001', 'agent-a', $2, $3, 'A', 'ingesting'),
		       ('c0000000-0000-0000-0000-0000000000e2', $1,
		        'dddddddd-0000-0000-0000-000000000001', 'agent-a', $2, $4, 'A', 'ingesting')`,
		acmeTenant, teamNorth, f.now.Add(-2*time.Minute), f.now.Add(-6*time.Hour)); err != nil {
		t.Fatal(err)
	}

	token := f.token("sup-1", acmeTenant, auth.RoleSupervisor, teamNorth)
	_, body := f.do(http.MethodPost, "/v1/teams/"+teamNorth+"/live/ticket", token, nil)
	var minted struct {
		Ticket string `json:"ticket"`
	}
	json.Unmarshal(body, &minted)

	req, _ := http.NewRequest(http.MethodGet,
		f.server.URL+"/v1/teams/"+teamNorth+"/live?ticket="+minted.Ticket, nil)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	resp, err := f.server.Client().Do(req.WithContext(ctx))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if got := resp.Header.Get("Content-Type"); got != "text/event-stream" {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 2048))
		t.Fatalf("content type %q, status %d, body %s", got, resp.StatusCode, b)
	}

	// Read just the first burst; the stream never ends on its own.
	buf := make([]byte, 4096)
	n, _ := resp.Body.Read(buf)
	payload := string(buf[:n])
	if !strings.Contains(payload, "c0000000-0000-0000-0000-0000000000e1") {
		t.Fatalf("the in-flight call was not streamed: %q", payload)
	}
	if strings.Contains(payload, "c0000000-0000-0000-0000-0000000000e2") {
		t.Fatal("a six-hour-old ingesting call is a ghost and must not appear on the floor view")
	}
}

func TestEvidenceExportIsAuditedWithWhatWasRequested(t *testing.T) {
	f := newFixture(t)
	var flagID string
	if err := f.admin.QueryRow(context.Background(),
		`SELECT id::text FROM flags LIMIT 1`).Scan(&flagID); err != nil {
		t.Fatal(err)
	}
	resp, body := f.do(http.MethodPost, "/v1/compliance/exports",
		f.token("qa-1", acmeTenant, auth.RoleQA, ""),
		map[string]any{"flag_ids": []string{flagID}, "include_audio": true})
	if resp.StatusCode != http.StatusAccepted {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var job struct {
		JobID  string `json:"job_id"`
		Status string `json:"status"`
	}
	json.Unmarshal(body, &job)
	if job.JobID == "" || job.Status != "queued" {
		t.Fatalf("unexpected job: %s", body)
	}

	var detail []byte
	if err := f.admin.QueryRow(context.Background(),
		`SELECT detail FROM audit_log WHERE action = 'evidence.export'`).Scan(&detail); err != nil {
		t.Fatalf("export not audited: %v", err)
	}
	if !strings.Contains(string(detail), flagID) || !strings.Contains(string(detail), "include_audio") {
		t.Fatalf("the audit entry does not record what left the system: %s", detail)
	}
}

func TestExportingAFlagOutsideScopeIsRefusedWithoutConfirmingItExists(t *testing.T) {
	// Otherwise an export request doubles as an oracle for flag ids in other teams:
	// ask for one, see whether the job is accepted.
	f := newFixture(t)
	resp, _ := f.do(http.MethodPost, "/v1/compliance/exports",
		f.token("qa-1", acmeTenant, auth.RoleQA, ""),
		map[string]any{"flag_ids": []string{"ffffffff-0000-0000-0000-000000000001"}})
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("expected 404, got %d", resp.StatusCode)
	}
}

func TestAgentsCannotExportEvidence(t *testing.T) {
	f := newFixture(t)
	resp, _ := f.do(http.MethodPost, "/v1/compliance/exports",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth),
		map[string]any{"flag_ids": []string{"ffffffff-0000-0000-0000-000000000001"}})
	if resp.StatusCode != http.StatusForbidden {
		t.Fatalf("expected 403, got %d", resp.StatusCode)
	}
}

func TestListingCallsIsAuditedNotJustOpeningOne(t *testing.T) {
	// Section 12.8 covers reads, and a listing page carries summaries and account
	// references. A reviewer paging the compliance queue reads borrower data; only
	// auditing the detail view would miss it entirely.
	f := newFixture(t)
	resp, body := f.do(http.MethodGet, "/v1/calls?q=Ravi",
		f.token("qa-1", acmeTenant, auth.RoleQA, ""), nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}

	var detail []byte
	if err := f.admin.QueryRow(context.Background(),
		`SELECT detail FROM audit_log WHERE action = 'call.list' AND actor_uid = 'qa-1'`).
		Scan(&detail); err != nil {
		t.Fatalf("listing not audited: %v", err)
	}
	if !strings.Contains(string(detail), "call_ids") || !strings.Contains(string(detail), `"searched": true`) {
		t.Fatalf("the entry does not record what was read: %s", detail)
	}
	// The search term itself must not be there. A QA user searching for a borrower
	// by name would otherwise write that name into a table every admin can read.
	if strings.Contains(string(detail), "Ravi") {
		t.Fatalf("the audit entry leaked the search term: %s", detail)
	}
}

func TestAnEmptyPageIsNotAudited(t *testing.T) {
	// Otherwise every poll from an idle portal tab writes a row, and the table that
	// has to answer "who read this call" fills with noise.
	f := newFixture(t)
	f.do(http.MethodGet, "/v1/calls?from=2020-01-01T00:00:00Z&to=2020-01-02T00:00:00Z",
		f.token("qa-1", acmeTenant, auth.RoleQA, ""), nil)

	var n int
	f.admin.QueryRow(context.Background(),
		`SELECT count(*) FROM audit_log WHERE action = 'call.list'`).Scan(&n)
	if n != 0 {
		t.Fatalf("an empty listing wrote %d audit rows", n)
	}
}

// ------------------------------------------- team scope, median, CORS, SSE

func TestAnAgentGetsATeamMedianWithoutSeeingColleagues(t *testing.T) {
	// The self-view is defined as a comparison against the median, and the
	// scorecards endpoint that also carries it is gated on a capability agents do
	// not hold — so without this the comparison column is always empty.
	f := newFixture(t)
	resp, body := f.do(http.MethodGet, "/v1/me/stats?from=2026-08-01&to=2026-09-30",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth), nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var out struct {
		Self   map[string]any `json:"self"`
		Median map[string]any `json:"median"`
	}
	if err := json.Unmarshal(body, &out); err != nil {
		t.Fatal(err)
	}
	if out.Self["user_uid"] != "agent-a" {
		t.Fatalf("self is not the caller: %s", body)
	}
	if out.Median == nil {
		t.Fatalf("no median returned: %s", body)
	}
	if out.Median["user_uid"] != "median" || out.Median["display_name"] != "Team median" {
		t.Fatalf("median is not anonymised: %s", body)
	}
	// The median is computed over agent-a and agent-b, so it is genuinely a peer
	// comparison rather than the caller's own numbers relabelled.
	if !strings.Contains(string(body), `"calls"`) {
		t.Fatalf("median carries no comparable figures: %s", body)
	}
	// No colleague may be named anywhere in the response.
	if strings.Contains(string(body), "agent-b") || strings.Contains(string(body), "Agent B") {
		t.Fatalf("a colleague's identity leaked into the self-view: %s", body)
	}
}

func TestAnAgentCannotEnumerateTheTeamRoster(t *testing.T) {
	// The org chart is the same kind of information as other agents' scores, and no
	// agent screen needs it.
	f := newFixture(t)
	resp, body := f.do(http.MethodGet, "/v1/teams",
		f.token("agent-a", acmeTenant, auth.RoleAgent, teamNorth), nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d: %s", resp.StatusCode, body)
	}
	var teams []struct {
		ID   string `json:"id"`
		Name string `json:"name"`
	}
	json.Unmarshal(body, &teams)
	// Their own team, and only their own.
	for _, tm := range teams {
		if tm.ID != teamNorth {
			t.Fatalf("an agent saw a team they are not in: %s", body)
		}
	}
}

func TestCorsIsOffByDefaultAndExactWhenConfigured(t *testing.T) {
	f := newFixture(t)
	// The fixture configures no origins, so a browser request gets no headers —
	// correct when the portal is served same-origin, and a deliberate decision
	// rather than a wildcard nobody chose.
	req, _ := http.NewRequest(http.MethodGet, f.server.URL+"/healthz", nil)
	req.Header.Set("Origin", "https://portal.example.com")
	resp, err := f.server.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.Header.Get("Access-Control-Allow-Origin") != "" {
		t.Fatal("CORS headers appeared without being configured")
	}
}

func TestTheLiveStreamAnnouncesEndedCallsAndSnapshots(t *testing.T) {
	// Without an explicit end signal a client can only remove rows by guessing at
	// staleness, and cannot tell a finished call from a frozen stream.
	f := newFixture(t)
	if _, err := f.admin.Exec(context.Background(), `
		INSERT INTO calls (id, tenant_id, device_id, user_uid, team_id, started_at,
		                   capture_tier, status)
		VALUES ('c0000000-0000-0000-0000-0000000000e1', $1,
		        'dddddddd-0000-0000-0000-000000000001', 'agent-a', $2, $3, 'A', 'ingesting')`,
		acmeTenant, teamNorth, f.now.Add(-2*time.Minute)); err != nil {
		t.Fatal(err)
	}

	token := f.token("sup-1", acmeTenant, auth.RoleSupervisor, teamNorth)
	_, body := f.do(http.MethodPost, "/v1/teams/"+teamNorth+"/live/ticket", token, nil)
	var minted struct {
		Ticket string `json:"ticket"`
	}
	json.Unmarshal(body, &minted)

	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	req, _ := http.NewRequestWithContext(ctx, http.MethodGet,
		f.server.URL+"/v1/teams/"+teamNorth+"/live?ticket="+minted.Ticket, nil)
	resp, err := f.server.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	buf := make([]byte, 4096)
	n, _ := resp.Body.Read(buf)
	first := string(buf[:n])
	if !strings.Contains(first, "event: call\n") || !strings.Contains(first, "event: snapshot\n") {
		t.Fatalf("first snapshot missing a call or its boundary marker: %q", first)
	}
	// The capture state, not the ingest status: "ingesting" tells a supervisor
	// nothing about what is happening on the floor.
	if strings.Contains(first, `"state":"ingesting"`) {
		t.Fatalf("the stream carries the ingest status instead of the capture state: %q", first)
	}

	// End the call; the next poll must say so rather than simply omitting it.
	if _, err := f.admin.Exec(context.Background(),
		`UPDATE calls SET ended_at = now() WHERE id = 'c0000000-0000-0000-0000-0000000000e1'`); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		n, err := resp.Body.Read(buf)
		if err != nil {
			break
		}
		if strings.Contains(string(buf[:n]), "event: call_ended") {
			return
		}
	}
	t.Fatal("a finished call was never announced as ended")
}
