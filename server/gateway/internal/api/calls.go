package api

import (
	"net/http"
	"strconv"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/httpx"
	"github.com/magickvoice/sentinel/server/gateway/internal/store"
)

// listCalls serves the call explorer for every role from one endpoint.
//
// There is deliberately no capability gate on the route. The visible set is decided
// by row-level security from the verified token — an agent sees their own calls, a
// supervisor their team's, qa/compliance/admin the whole tenant, a bank client only
// flagged ones — so a gate here would be a second, weaker copy of a rule the database
// already enforces, and the two would drift.
//
// The query parameters narrow what is already visible. They cannot widen it: a
// supervisor passing another team's id gets an empty page, not someone else's calls.
func (s *Server) listCalls(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	f := callFilterFromQuery(r)
	q := r.URL.Query()
	f.UserUID = q.Get("user_uid")
	f.TeamID = q.Get("team_id")
	if v := q.Get("has_flags"); v != "" {
		if b, err := strconv.ParseBool(v); err == nil {
			f.HasFlags = &b
		}
	}
	// A bank client's scope is flagged calls only, which RLS already enforces on the
	// rows. Setting the filter as well keeps the query planner on the partial index.
	if id.Role == auth.RoleClient {
		t := true
		f.HasFlags = &t
	}
	items, next, err := s.Store.ListCalls(r.Context(), id, f)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	writeCallPage(w, items, next)
}

// getCall serves one call at the caller's scope.
//
// A call outside that scope returns 404, not 403. Whether a given call id exists in
// another team is itself information, and on a floor where agents compare notes it
// is information worth withholding.
func (s *Server) getCall(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	detail, err := s.Store.GetCall(r.Context(), id, r.PathValue("id"))
	if err != nil {
		s.fail(w, r, err)
		return
	}
	policy, err := s.Store.PolicyForTenant(r.Context(), id.TenantID)
	if err == nil && !id.CanPlayAudio(policy.AllowAgentAudioPlayback) {
		// The URL is withheld rather than the call: a reviewer who may read the
		// transcript but not hear the audio still gets everything else.
		detail.AudioURL = nil
	}
	httpx.WriteJSON(w, http.StatusOK, detail)
}

func (s *Server) listTeams(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	teams, err := s.Store.ListTeams(r.Context(), id)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	if teams == nil {
		teams = []store.Team{}
	}
	httpx.WriteJSON(w, http.StatusOK, teams)
}
