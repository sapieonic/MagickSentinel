// Package api serves the REST surface described by contracts/openapi.yaml.
package api

import (
	"log/slog"
	"net/http"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/httpx"
	"github.com/magickvoice/sentinel/server/gateway/internal/store"
)

type Server struct {
	Log      *slog.Logger
	Store    *store.Store
	Verifier *auth.Verifier
	Version  string
	// Ingest is mounted at /v1/ingest when set.
	Ingest http.Handler
	// CA signs device certificates at enrollment.
	CA CertificateAuthority
	// LiveTickets backs the SSE floor view. Nil disables the live routes.
	LiveTickets *LiveTickets
	// LivePoll is how often the floor view re-queries active calls.
	LivePoll time.Duration
	Now    func() time.Time
}

func (s *Server) now() time.Time {
	if s.Now != nil {
		return s.Now()
	}
	return time.Now()
}

// Routes builds the mux. Every route except /healthz and enrollment sits behind
// Authenticate; capability gates are applied per route from the matrix in spec
// section 13.4.
func (s *Server) Routes() http.Handler {
	mux := http.NewServeMux()

	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		httpx.WriteJSON(w, http.StatusOK, map[string]string{"status": "ok", "version": s.Version})
	})

	// Enrollment is the one authenticated-by-token-only path: the device has no
	// certificate yet, which is the point of the exchange.
	mux.Handle("POST /v1/devices/enroll", http.HandlerFunc(s.enrollDevice))

	authed := func(h http.HandlerFunc) http.Handler {
		return s.Authenticate(AssertMeNamespace(h))
	}
	device := func(h http.HandlerFunc) http.Handler {
		return s.Authenticate(RequireDevice(h))
	}
	gated := func(c auth.Capability, h http.HandlerFunc) http.Handler {
		return s.Authenticate(auth.Require(c, AssertMeNamespace(h)))
	}

	// The call explorer. Scope comes from row-level security, not from the route,
	// so one pair of handlers serves every role.
	mux.Handle("GET /v1/calls", authed(s.listCalls))
	mux.Handle("GET /v1/calls/{id}", authed(s.getCall))
	mux.Handle("GET /v1/teams", authed(s.listTeams))

	// Session and device.
	mux.Handle("POST /v1/sessions", authed(s.createSession))
	mux.Handle("DELETE /v1/sessions/current", authed(s.endSession))
	mux.Handle("GET /v1/policy", device(s.getPolicy))
	mux.Handle("POST /v1/heartbeat", device(s.heartbeat))

	// Agent self-service.
	mux.Handle("GET /v1/me/calls", authed(s.listMyCalls))
	mux.Handle("GET /v1/me/calls/{id}", authed(s.getMyCall))
	mux.Handle("POST /v1/me/calls/{id}/confirm", authed(s.confirmMyCall))
	mux.Handle("GET /v1/me/stats", authed(s.myStats))
	mux.Handle("GET /v1/me/flags", authed(s.myFlags))
	mux.Handle("POST /v1/me/flags/{id}/respond", authed(s.respondToMyFlag))

	// Team.
	mux.Handle("GET /v1/teams/{id}/calls", gated(auth.CapTeamCalls, s.listTeamCalls))
	mux.Handle("GET /v1/teams/{id}/scorecards", gated(auth.CapTeamCalls, s.teamScorecards))
	mux.Handle("POST /v1/teams/{id}/live/ticket", gated(auth.CapTeamCalls, s.createLiveTicket))
	// Deliberately outside Authenticate: the ticket is the credential, and it
	// carries the identity that was verified when it was minted.
	mux.Handle("GET /v1/teams/{id}/live", http.HandlerFunc(s.streamTeamLive))

	// Compliance.
	mux.Handle("GET /v1/compliance/flags", gated(auth.CapResolveFlags, s.listFlags))
	mux.Handle("PATCH /v1/compliance/flags/{id}", gated(auth.CapResolveFlags, s.updateFlag))
	mux.Handle("POST /v1/compliance/exports", gated(auth.CapResolveFlags, s.createEvidenceExport))

	// Admin.
	mux.Handle("GET /v1/admin/devices", gated(auth.CapManageFleet, s.listDevices))
	mux.Handle("POST /v1/admin/devices/{id}/revoke", gated(auth.CapManageFleet, s.revokeDevice))
	mux.Handle("POST /v1/admin/enrollment-tokens", gated(auth.CapManageFleet, s.createEnrollmentToken))
	mux.Handle("GET /v1/admin/users", gated(auth.CapManageFleet, s.listUsers))
	mux.Handle("PATCH /v1/admin/users/{uid}", gated(auth.CapManageFleet, s.updateUser))
	mux.Handle("GET /v1/admin/rules", gated(auth.CapEditRules, s.getRules))
	mux.Handle("PUT /v1/admin/rules", gated(auth.CapEditRules, s.putRules))
	mux.Handle("GET /v1/admin/audit", gated(auth.CapManageFleet, s.auditLog))

	if s.Ingest != nil {
		mux.Handle("/v1/ingest", s.Authenticate(RequireDevice(s.Ingest)))
	}

	return httpx.WithRequestID(httpx.Recover(s.Log, httpx.LogRequests(s.Log, mux)))
}
