package api

import (
	"encoding/json"
	"errors"
	"net/http"
	"strconv"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/httpx"
	"github.com/magickvoice/sentinel/server/gateway/internal/store"
)

// ------------------------------------------------------------- session, policy

func (s *Server) createSession(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	policy, err := s.Store.PolicyForTenant(r.Context(), id.TenantID)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, map[string]any{
		"user": map[string]any{
			"firebase_uid": id.UserUID,
			"tenant_id":    id.TenantID,
			"role":         id.Role,
			"team_id":      nullString(id.TeamID),
			"display_name": id.UserUID,
			"status":       "active",
		},
		"policy":      policyBody(policy),
		"server_time": s.now().UTC(),
	})
}

func (s *Server) endSession(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	// The client is required to flush its spool before calling this, or the spooled
	// audio becomes unattributable. We record the sign-out either way so a coverage
	// gap has an explanation.
	_ = s.Store.Audit(r.Context(), id, "session.end", "user", id.UserUID, nil)
	w.WriteHeader(http.StatusNoContent)
}

func (s *Server) getPolicy(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	policy, err := s.Store.PolicyForTenant(r.Context(), id.TenantID)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, policyBody(policy))
}

func policyBody(p store.Policy) map[string]any {
	body := map[string]any{
		"version":                    p.Version,
		"offline_grace_hours":        p.OfflineGraceHours,
		"idle_signout_minutes":       p.IdleSignoutMinutes,
		"rules_version":              p.RulesVersion,
		"allow_agent_audio_playback": p.AllowAgentAudioPlayback,
		"retention": map[string]any{
			"audio_days":      p.AudioRetentionDays,
			"transcript_days": p.TranscriptRetentionDays,
		},
		"pinned_devices": []any{},
		"softphone":      map[string]any{},
	}
	// tenants.policy carries the capture configuration an admin edits: pinned
	// devices, softphone process names and UIA selectors, VAD thresholds. It is
	// merged over the defaults so a tenant can override only what it cares about.
	if len(p.Raw) > 0 {
		var extra map[string]any
		if err := json.Unmarshal(p.Raw, &extra); err == nil {
			for k, v := range extra {
				body[k] = v
			}
		}
	}
	return body
}

// ------------------------------------------------------------------ heartbeat

type heartbeatBody struct {
	DeviceID     string     `json:"device_id"`
	CaptureState string     `json:"capture_state"`
	CaptureTier  string     `json:"capture_tier"`
	OSBuild      string     `json:"os_build"`
	AgentVersion string     `json:"agent_version"`
	SpoolDepth   int        `json:"spool_depth"`
	SpoolBytes   int64      `json:"spool_bytes"`
	LastCallAt   *time.Time `json:"last_call_at"`
	SignedIn     bool       `json:"signed_in"`
	Events       []struct {
		Kind   string    `json:"kind"`
		At     time.Time `json:"at"`
		Count  *int      `json:"count"`
		Detail string    `json:"detail"`
	} `json:"events"`
}

func (s *Server) heartbeat(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	var body heartbeatBody
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request", "malformed heartbeat")
		return
	}
	// The device is the certificate's, not the body's. A heartbeat claiming to be
	// another machine is a bug at best.
	if body.DeviceID != "" && body.DeviceID != id.DeviceID {
		httpx.WriteError(w, r, http.StatusForbidden, "device_mismatch",
			"heartbeat device does not match the client certificate")
		return
	}
	now := s.now()
	if err := s.Store.TouchDevice(r.Context(), id.TenantID, id.DeviceID, body.CaptureState,
		body.CaptureTier, body.OSBuild, body.AgentVersion, body.SpoolDepth, now); err != nil {
		s.fail(w, r, err)
		return
	}
	for _, e := range body.Events {
		// Detail is machine state only. Anything resembling call content would be a
		// PII leak into a table the whole tenant's admins can read.
		if err := s.Store.RecordDeviceEvent(r.Context(), id.TenantID, id.DeviceID,
			e.Kind, e.Count, e.Detail, e.At); err != nil {
			s.Log.Warn("heartbeat: record event", "kind", e.Kind, "error", err)
		}
	}
	policy, err := s.Store.PolicyForTenant(r.Context(), id.TenantID)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, map[string]any{
		"policy_version": policy.Version,
		"server_time":    now.UTC(),
		"commands":       []any{},
	})
}

// ---------------------------------------------------------------- me namespace

func (s *Server) listMyCalls(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	f := callFilterFromQuery(r)
	// The UID comes from the token. AssertMeNamespace has already refused any
	// request that tried to supply one.
	f.UserUID = id.UserUID
	items, next, err := s.Store.ListCalls(r.Context(), id, f)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	writeCallPage(w, items, next)
}

func (s *Server) getMyCall(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	detail, err := s.Store.GetCall(r.Context(), id, r.PathValue("id"))
	if err != nil {
		s.fail(w, r, err)
		return
	}
	if detail.UserUID != id.UserUID {
		// Belt and braces: RLS should already have hidden it.
		httpx.WriteError(w, r, http.StatusNotFound, "not_found", "call not found")
		return
	}
	policy, err := s.Store.PolicyForTenant(r.Context(), id.TenantID)
	if err == nil && !id.CanPlayAudio(policy.AllowAgentAudioPlayback) {
		detail.AudioURL = nil
	}
	httpx.WriteJSON(w, http.StatusOK, detail)
}

func (s *Server) confirmMyCall(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	var body struct {
		Disposition string  `json:"disposition"`
		PTPPresent  bool    `json:"ptp_present"`
		AmountPaise *int64  `json:"ptp_amount_paise"`
		DueDate     *string `json:"ptp_due_date"`
		Note        string  `json:"note"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request", "malformed body")
		return
	}
	if !validDisposition(body.Disposition) {
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request", "unknown disposition")
		return
	}
	if body.AmountPaise != nil && *body.AmountPaise < 0 {
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request",
			"amount must be a non-negative integer number of paise")
		return
	}
	policy, err := s.Store.PolicyForTenant(r.Context(), id.TenantID)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	window := 24
	if policy.IdleSignoutMinutes > 0 { // placeholder until the column is surfaced
		window = 24
	}
	callID := r.PathValue("id")
	err = s.Store.ConfirmCall(r.Context(), id, callID, store.Confirmation{
		Disposition: body.Disposition, PTPPresent: body.PTPPresent,
		AmountPaise: body.AmountPaise, DueDate: body.DueDate, Note: body.Note,
	}, window, s.now())
	if errors.Is(err, store.ErrCorrectionWindowClosed) {
		httpx.WriteError(w, r, http.StatusConflict, "window_closed",
			"the correction window for this call has closed")
		return
	}
	if err != nil {
		s.fail(w, r, err)
		return
	}
	detail, err := s.Store.GetCall(r.Context(), id, callID)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, detail)
}

func (s *Server) myStats(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	from, to := dateRange(r, s.now())
	stats, err := s.Store.AgentStats(r.Context(), id, id.UserUID, from, to)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, stats)
}

func (s *Server) myFlags(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	// RLS restricts flags to calls this agent owns, so no extra predicate is needed
	// and none is written: a redundant one here would rot out of step with the
	// policy that actually enforces it.
	flags, err := s.Store.ListFlags(r.Context(), id, store.FlagFilter{Limit: 200})
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, orEmptyFlags(flags))
}

func (s *Server) respondToMyFlag(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	var body struct {
		Response string `json:"response"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil || body.Response == "" {
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request", "a response is required")
		return
	}
	if len(body.Response) > 2000 {
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request", "response too long")
		return
	}
	flag, err := s.Store.RespondToFlag(r.Context(), id, r.PathValue("id"), body.Response)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, flag)
}

// ----------------------------------------------------------------------- team

func (s *Server) listTeamCalls(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	f := callFilterFromQuery(r)
	f.TeamID = r.PathValue("id")
	// A supervisor asking for another team gets an empty page: RLS restricts the
	// rows, and the team filter only narrows what is already visible.
	items, next, err := s.Store.ListCalls(r.Context(), id, f)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	writeCallPage(w, items, next)
}

func (s *Server) teamScorecards(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	from, to := dateRange(r, s.now())
	// Compared against the team median, never ranked. Leaderboards on a collections
	// floor produce gaming, not improvement.
	median, agents, err := s.Store.TeamScorecards(r.Context(), id, r.PathValue("id"), from, to)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, map[string]any{"median": median, "agents": agents})
}

// ----------------------------------------------------------------- compliance

func (s *Server) listFlags(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	q := r.URL.Query()
	flags, err := s.Store.ListFlags(r.Context(), id, store.FlagFilter{
		Severity: q.Get("severity"),
		Status:   q.Get("status"),
		RuleID:   q.Get("rule_id"),
		Limit:    atoiOr(q.Get("limit"), 50),
	})
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, map[string]any{"items": orEmptyFlags(flags)})
}

func (s *Server) updateFlag(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	var body struct {
		Status      string `json:"status"`
		ReviewerUID string `json:"reviewer_uid"`
		Note        string `json:"note"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request", "malformed body")
		return
	}
	if body.Status != "" && !validFlagStatus(body.Status) {
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request", "unknown status")
		return
	}
	flag, err := s.Store.UpdateFlag(r.Context(), id, r.PathValue("id"),
		body.Status, body.ReviewerUID, body.Note, s.now())
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, flag)
}

// ---------------------------------------------------------------------- admin

func (s *Server) listDevices(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	devices, tiers, err := s.Store.ListDevices(r.Context(), id, r.URL.Query().Get("status"))
	if err != nil {
		s.fail(w, r, err)
		return
	}
	if devices == nil {
		devices = []store.Device{}
	}
	httpx.WriteJSON(w, http.StatusOK, map[string]any{
		"items":             devices,
		"tier_distribution": tiers,
	})
}

func (s *Server) revokeDevice(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	var body struct {
		Reason string `json:"reason"`
	}
	_ = json.NewDecoder(r.Body).Decode(&body)
	if err := s.Store.RevokeDevice(r.Context(), id, r.PathValue("id"), body.Reason, s.now()); err != nil {
		s.fail(w, r, err)
		return
	}
	// Live connections are terminated by the ingest layer's revocation poll, which
	// runs well inside the 60 s the spec requires.
	w.WriteHeader(http.StatusNoContent)
}

func (s *Server) createEnrollmentToken(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	token, expires, err := s.Store.CreateEnrollmentToken(r.Context(), id, s.now())
	if err != nil {
		s.fail(w, r, err)
		return
	}
	// The token is returned once and stored only as a hash: a leaked database backup
	// must not enrol devices.
	httpx.WriteJSON(w, http.StatusCreated, map[string]any{
		"token": token, "expires_at": expires,
	})
}

func (s *Server) listUsers(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	users, err := s.Store.ListUsers(r.Context(), id)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	if users == nil {
		users = []store.User{}
	}
	httpx.WriteJSON(w, http.StatusOK, users)
}

func (s *Server) updateUser(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	var body struct {
		Role   string  `json:"role"`
		TeamID *string `json:"team_id"`
		Status string  `json:"status"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request", "malformed body")
		return
	}
	user, err := s.Store.UpdateUser(r.Context(), id, r.PathValue("uid"), body.Role, body.TeamID, body.Status)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, user)
}

func (s *Server) getRules(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	rules, err := s.Store.ActiveRuleSet(r.Context(), id)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusOK, rules)
}

func (s *Server) putRules(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	var definition json.RawMessage
	if err := json.NewDecoder(r.Body).Decode(&definition); err != nil {
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request", "malformed rule set")
		return
	}
	// A new version, never a mutation of the existing one: flags already raised must
	// stay traceable to the rules that raised them.
	rules, err := s.Store.PublishRuleSet(r.Context(), id, definition)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusCreated, rules)
}

func (s *Server) auditLog(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	q := r.URL.Query()
	entries, err := s.Store.AuditEntries(r.Context(), id, q.Get("actor_uid"), q.Get("entity"),
		atoiOr(q.Get("limit"), 100))
	if err != nil {
		s.fail(w, r, err)
		return
	}
	if entries == nil {
		entries = []store.AuditEntry{}
	}
	httpx.WriteJSON(w, http.StatusOK, map[string]any{"items": entries})
}

// ------------------------------------------------------------------- helpers

func (s *Server) fail(w http.ResponseWriter, r *http.Request, err error) {
	switch {
	case errors.Is(err, store.ErrNotFound):
		httpx.WriteError(w, r, http.StatusNotFound, "not_found", "not found")
	default:
		s.Log.Error("handler", "error", err, "request_id", httpx.RequestID(r.Context()))
		httpx.WriteError(w, r, http.StatusInternalServerError, "internal", "internal error")
	}
}

func writeCallPage(w http.ResponseWriter, items []store.CallSummary, next *time.Time) {
	if items == nil {
		items = []store.CallSummary{}
	}
	body := map[string]any{"items": items}
	if next != nil {
		body["next_cursor"] = next.UTC().Format(time.RFC3339Nano)
	}
	httpx.WriteJSON(w, http.StatusOK, body)
}

func orEmptyFlags(f []store.Flag) []store.Flag {
	if f == nil {
		return []store.Flag{}
	}
	return f
}

func callFilterFromQuery(r *http.Request) store.CallFilter {
	q := r.URL.Query()
	f := store.CallFilter{
		Disposition: q.Get("disposition"),
		Search:      q.Get("q"),
		Limit:       atoiOr(q.Get("limit"), 50),
	}
	if t, err := time.Parse(time.RFC3339, q.Get("from")); err == nil {
		f.From = &t
	}
	if t, err := time.Parse(time.RFC3339, q.Get("to")); err == nil {
		f.To = &t
	}
	if t, err := time.Parse(time.RFC3339Nano, q.Get("cursor")); err == nil {
		f.Cursor = &t
	}
	return f
}

func dateRange(r *http.Request, now time.Time) (time.Time, time.Time) {
	q := r.URL.Query()
	to := now
	from := now.AddDate(0, 0, -30)
	if t, err := time.Parse("2006-01-02", q.Get("from")); err == nil {
		from = t
	}
	if t, err := time.Parse("2006-01-02", q.Get("to")); err == nil {
		to = t.AddDate(0, 0, 1)
	}
	return from, to
}

func atoiOr(s string, fallback int) int {
	if n, err := strconv.Atoi(s); err == nil {
		return n
	}
	return fallback
}

func nullString(s string) any {
	if s == "" {
		return nil
	}
	return s
}

func validDisposition(d string) bool {
	switch d {
	case "ptp", "refusal", "dispute", "wrong_number", "no_contact",
		"callback_requested", "partial_payment", "escalation", "other":
		return true
	}
	return false
}

func validFlagStatus(s string) bool {
	switch s {
	case "open", "assigned", "upheld", "dismissed":
		return true
	}
	return false
}

func (s *Server) createEvidenceExport(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	var body struct {
		FlagIDs      []string `json:"flag_ids"`
		IncludeAudio bool     `json:"include_audio"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil || len(body.FlagIDs) == 0 {
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request",
			"at least one flag id is required")
		return
	}
	if len(body.FlagIDs) > 500 {
		httpx.WriteError(w, r, http.StatusBadRequest, "too_many",
			"an evidence pack is capped at 500 flags")
		return
	}
	if body.IncludeAudio {
		policy, err := s.Store.PolicyForTenant(r.Context(), id.TenantID)
		if err != nil {
			s.fail(w, r, err)
			return
		}
		if !id.CanPlayAudio(policy.AllowAgentAudioPlayback) {
			httpx.WriteError(w, r, http.StatusForbidden, "audio_forbidden",
				"this role may not export call audio for this tenant")
			return
		}
	}
	job, err := s.Store.QueueEvidenceExport(r.Context(), id, body.FlagIDs, body.IncludeAudio)
	if err != nil {
		s.fail(w, r, err)
		return
	}
	httpx.WriteJSON(w, http.StatusAccepted, job)
}
