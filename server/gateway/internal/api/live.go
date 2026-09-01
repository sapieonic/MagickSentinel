package api

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/http"
	"sync"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/httpx"
)

// LiveTicket is a single-use credential for the SSE floor view.
//
// EventSource cannot send an Authorization header, so the browser has no way to
// present a bearer token on the stream. Putting the token in the query string would
// work and is what most implementations do; it also writes a one-hour credential
// into every access log, proxy cache and browser history entry between here and the
// client. A 60-second single-use ticket scoped to one team costs one extra round
// trip and leaks nothing worth stealing.
type LiveTicket struct {
	Identity  *auth.Identity
	TeamID    string
	ExpiresAt time.Time
}

// LiveTickets is an in-memory ticket store.
//
// Deliberately not in Postgres. A ticket lives 60 seconds and is consumed once; the
// worst case for a single-process restart is that a supervisor's floor view
// reconnects, which it is built to do anyway. Behind more than one gateway replica
// this needs to move to Redis or to sticky routing — noted here rather than
// discovered later.
type LiveTickets struct {
	mu   sync.Mutex
	byID map[string]LiveTicket
	ttl  time.Duration
}

func NewLiveTickets(ttl time.Duration) *LiveTickets {
	if ttl == 0 {
		ttl = 60 * time.Second
	}
	return &LiveTickets{byID: map[string]LiveTicket{}, ttl: ttl}
}

func (l *LiveTickets) Mint(id *auth.Identity, teamID string, now time.Time) (string, time.Time) {
	raw := make([]byte, 32)
	_, _ = rand.Read(raw)
	token := base64.RawURLEncoding.EncodeToString(raw)
	expires := now.Add(l.ttl)

	l.mu.Lock()
	defer l.mu.Unlock()
	// Sweep on write. Tickets are short-lived and low-volume, so a background
	// reaper would be more machinery than the problem deserves.
	for k, v := range l.byID {
		if now.After(v.ExpiresAt) {
			delete(l.byID, k)
		}
	}
	l.byID[hashTicket(token)] = LiveTicket{Identity: id, TeamID: teamID, ExpiresAt: expires}
	return token, expires
}

// Consume redeems a ticket, or reports that it is unusable. A redeemed ticket is
// removed whether or not it had expired, so a leaked one cannot be replayed.
func (l *LiveTickets) Consume(token string, now time.Time) (LiveTicket, bool) {
	l.mu.Lock()
	defer l.mu.Unlock()
	key := hashTicket(token)
	t, ok := l.byID[key]
	delete(l.byID, key)
	if !ok || now.After(t.ExpiresAt) {
		return LiveTicket{}, false
	}
	return t, true
}

func hashTicket(token string) string {
	sum := sha256.Sum256([]byte(token))
	return hex.EncodeToString(sum[:])
}

func (s *Server) createLiveTicket(w http.ResponseWriter, r *http.Request) {
	id := auth.MustFromContext(r.Context())
	if s.LiveTickets == nil {
		httpx.WriteError(w, r, http.StatusServiceUnavailable, "unavailable",
			"the live view is not enabled on this deployment")
		return
	}
	teamID := r.PathValue("id")
	token, expires := s.LiveTickets.Mint(id, teamID, s.now())
	httpx.WriteJSON(w, http.StatusCreated, map[string]any{
		"ticket": token, "expires_at": expires.UTC(),
	})
}

// streamTeamLive serves the supervisor floor view.
//
// The route is outside the Authenticate middleware because the ticket *is* the
// credential; the identity it carries was verified when the ticket was minted, and
// every query below runs under that identity's row-level security context.
func (s *Server) streamTeamLive(w http.ResponseWriter, r *http.Request) {
	if s.LiveTickets == nil {
		httpx.WriteError(w, r, http.StatusServiceUnavailable, "unavailable",
			"the live view is not enabled on this deployment")
		return
	}
	ticket, ok := s.LiveTickets.Consume(r.URL.Query().Get("ticket"), s.now())
	if !ok {
		httpx.WriteError(w, r, http.StatusUnauthorized, "bad_ticket",
			"ticket is invalid, expired, or already used")
		return
	}
	if ticket.TeamID != r.PathValue("id") {
		httpx.WriteError(w, r, http.StatusForbidden, "wrong_team",
			"this ticket was minted for a different team")
		return
	}
	if !ticket.Identity.Can(auth.CapTeamCalls) {
		httpx.WriteError(w, r, http.StatusForbidden, "forbidden", "role not permitted")
		return
	}

	// ResponseController rather than a type assertion: the request has passed
	// through wrappers that do not themselves implement Flusher, and it follows
	// their Unwrap chain to the real writer.
	//
	// There is no support probe before the headers go out, because probing means
	// flushing, and flushing commits a 200 that then cannot be turned into an error
	// response. Headers first, then flush and check.
	rc := http.NewResponseController(w)
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	// Long-lived responses through a reverse proxy get buffered into uselessness
	// without this; nginx in particular will hold the whole stream.
	w.Header().Set("X-Accel-Buffering", "no")
	w.WriteHeader(http.StatusOK)
	if err := rc.Flush(); err != nil {
		s.Log.Error("live view cannot stream", "error", err)
		return
	}

	poll := s.LivePoll
	if poll == 0 {
		poll = 3 * time.Second
	}
	ticker := time.NewTicker(poll)
	defer ticker.Stop()
	// A comment frame every 20 s keeps intermediaries from reaping an idle stream
	// on a quiet floor between shifts.
	keepalive := time.NewTicker(20 * time.Second)
	defer keepalive.Stop()

	// Calls seen in the previous snapshot, so a call that has gone away can be
	// announced rather than left for the client to time out. Without an explicit
	// end, removal is a staleness heuristic, and a supervisor watching a floor where
	// nothing changes for a minute cannot tell a finished call from a frozen stream.
	previous := map[string]bool{}

	send := func() bool {
		events, err := s.Store.LiveCalls(r.Context(), ticket.Identity, ticket.TeamID, s.now())
		if err != nil {
			// A transient database failure must not end the stream, and must not be
			// reported as every call ending: the previous snapshot stands.
			s.Log.Error("live view query", "error", err)
			return true
		}
		current := make(map[string]bool, len(events))
		for _, e := range events {
			current[e.CallID] = true
			b, err := json.Marshal(e)
			if err != nil {
				continue
			}
			if _, err := fmt.Fprintf(w, "event: call\ndata: %s\n\n", b); err != nil {
				return false
			}
		}
		for callID := range previous {
			if current[callID] {
				continue
			}
			if _, err := fmt.Fprintf(w,
				"event: call_ended\ndata: {\"call_id\":%q}\n\n", callID); err != nil {
				return false
			}
		}
		previous = current
		// A snapshot marker closes each batch, so a client that missed a call_ended
		// can reconcile against the complete set rather than accumulating ghosts.
		if _, err := fmt.Fprintf(w,
			"event: snapshot\ndata: {\"count\":%d}\n\n", len(current)); err != nil {
			return false
		}
		return rc.Flush() == nil
	}

	if !send() {
		return
	}
	for {
		select {
		case <-r.Context().Done():
			return
		case <-ticker.C:
			if !send() {
				return
			}
		case <-keepalive.C:
			if _, err := fmt.Fprint(w, ": keepalive\n\n"); err != nil {
				return
			}
			if rc.Flush() != nil {
				return
			}
		}
	}
}
