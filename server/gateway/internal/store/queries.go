package store

import (
	"context"
	"encoding/json"
	"errors"
	"time"

	"github.com/jackc/pgx/v5"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
)

// CallSummary matches the CallSummary schema in contracts/openapi.yaml.
type CallSummary struct {
	ID             string     `json:"id"`
	StartedAt      time.Time  `json:"started_at"`
	EndedAt        *time.Time `json:"ended_at"`
	DurationMS     *int       `json:"duration_ms"`
	UserUID        string     `json:"user_uid"`
	AccountRef     *string    `json:"account_ref"`
	Direction      *string    `json:"direction"`
	CaptureTier    string     `json:"capture_tier"`
	Status         string     `json:"status"`
	Disposition    *string    `json:"disposition"`
	Summary        *string    `json:"summary"`
	SentimentDelta *float64   `json:"sentiment_delta"`
	FlagCount      int        `json:"flag_count"`
	MaxSeverity    *string    `json:"max_severity"`
	PTP            *PTP       `json:"ptp"`
}

type PTP struct {
	Present          bool    `json:"present"`
	AmountPaise      *int64  `json:"amount_paise"`
	DueDate          *string `json:"due_date"`
	Confidence       float64 `json:"confidence"`
	AgentConfirmed   *bool   `json:"agent_confirmed"`
	AgentAmountPaise *int64  `json:"agent_amount_paise"`
	AgentDueDate     *string `json:"agent_due_date"`
}

// CallFilter is the query surface shared by the me, team and compliance listings.
type CallFilter struct {
	UserUID     string
	TeamID      string
	From        *time.Time
	To          *time.Time
	Disposition string
	Search      string
	Limit       int
	Cursor      *time.Time
}

func (f CallFilter) limit() int {
	switch {
	case f.Limit <= 0:
		return 50
	case f.Limit > 200:
		return 200
	default:
		return f.Limit
	}
}

const callSelect = `
SELECT c.id::text, c.started_at, c.ended_at, c.duration_ms, c.user_uid, c.account_ref,
       c.direction, c.capture_tier, c.status,
       a.disposition, a.summary, (a.sentiment->>'delta')::float8,
       COALESCE(f.n, 0), f.max_severity,
       p.amount_paise, p.due_date::text, p.confidence,
       p.agent_confirmed, p.agent_amount_paise, p.agent_due_date::text
  FROM calls c
  LEFT JOIN analyses a ON a.call_id = c.id
  LEFT JOIN ptps p ON p.call_id = c.id
  LEFT JOIN LATERAL (
        SELECT count(*) AS n,
               (ARRAY['critical','high','medium','low'])[
                 min(array_position(ARRAY['critical','high','medium','low'], severity))
               ] AS max_severity
          FROM flags fl WHERE fl.call_id = c.id
  ) f ON true
`

// ListCalls returns a page of calls visible to the caller.
//
// Visibility is not expressed here: row-level security already limits the rows to
// what this role may see. The filters below narrow that set, they do not widen it,
// which is why a supervisor passing another team's id gets an empty page rather than
// someone else's calls.
func (s *Store) ListCalls(ctx context.Context, id *auth.Identity, f CallFilter) ([]CallSummary, *time.Time, error) {
	var out []CallSummary
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		rows, err := tx.Query(ctx, callSelect+`
 WHERE ($1::text  IS NULL OR c.user_uid = $1)
   AND ($2::uuid  IS NULL OR c.team_id = $2)
   AND ($3::timestamptz IS NULL OR c.started_at >= $3)
   AND ($4::timestamptz IS NULL OR c.started_at <  $4)
   AND ($5::text  IS NULL OR a.disposition = $5)
   AND ($6::timestamptz IS NULL OR c.started_at < $6)
   AND ($7::text  IS NULL OR EXISTS (
         SELECT 1 FROM transcripts t
          WHERE t.call_id = c.id
            AND to_tsvector('simple', t.text) @@ plainto_tsquery('simple', $7)))
 ORDER BY c.started_at DESC
 LIMIT $8`,
			nullText(f.UserUID), nullUUID(f.TeamID), f.From, f.To,
			nullText(f.Disposition), f.Cursor, nullText(f.Search), f.limit())
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			c, err := scanCall(rows)
			if err != nil {
				return err
			}
			out = append(out, c)
		}
		return rows.Err()
	})
	if err != nil {
		return nil, nil, err
	}
	var next *time.Time
	if len(out) == f.limit() {
		next = &out[len(out)-1].StartedAt
	}
	return out, next, nil
}

func scanCall(rows pgx.Rows) (CallSummary, error) {
	var c CallSummary
	var amount, agentAmount *int64
	var dueDate, agentDueDate *string
	var confidence *float64
	var agentConfirmed *bool
	err := rows.Scan(&c.ID, &c.StartedAt, &c.EndedAt, &c.DurationMS, &c.UserUID,
		&c.AccountRef, &c.Direction, &c.CaptureTier, &c.Status,
		&c.Disposition, &c.Summary, &c.SentimentDelta, &c.FlagCount, &c.MaxSeverity,
		&amount, &dueDate, &confidence, &agentConfirmed, &agentAmount, &agentDueDate)
	if err != nil {
		return c, err
	}
	if confidence != nil {
		c.PTP = &PTP{
			Present: amount != nil, AmountPaise: amount, DueDate: dueDate,
			Confidence: *confidence, AgentConfirmed: agentConfirmed,
			AgentAmountPaise: agentAmount, AgentDueDate: agentDueDate,
		}
	}
	return c, nil
}

// CallDetail adds the transcript, sentiment and flags.
type CallDetail struct {
	CallSummary
	Transcript    []TranscriptTurn `json:"transcript"`
	Sentiment     json.RawMessage  `json:"sentiment"`
	Flags         []Flag           `json:"flags"`
	TalkRatio     *float64         `json:"talk_ratio"`
	Interruptions *int             `json:"interruptions"`
	NextAction    *string          `json:"next_action"`
	AudioURL      *string          `json:"audio_url"`
}

type TranscriptTurn struct {
	Channel    int16    `json:"channel"`
	Speaker    string   `json:"speaker"`
	StartMS    int      `json:"start_ms"`
	EndMS      int      `json:"end_ms"`
	Text       string   `json:"text"`
	Confidence *float64 `json:"confidence"`
}

type Flag struct {
	ID             string     `json:"id"`
	CallID         string     `json:"call_id"`
	RuleID         string     `json:"rule_id"`
	RuleSetVersion int        `json:"rule_set_version"`
	Severity       string     `json:"severity"`
	Tier           int        `json:"tier"`
	SpanStartMS    *int       `json:"span_start_ms"`
	SpanEndMS      *int       `json:"span_end_ms"`
	EvidenceText   *string    `json:"evidence_text"`
	JudgeRationale *string    `json:"judge_rationale"`
	Status         string     `json:"status"`
	ReviewerUID    *string    `json:"reviewer_uid"`
	AgentResponse  *string    `json:"agent_response"`
	ResolvedAt     *time.Time `json:"resolved_at"`
}

// GetCall loads one call. Reading call content is audited, not just writing it: a
// compliance product must be able to answer who read a borrower's call.
func (s *Store) GetCall(ctx context.Context, id *auth.Identity, callID string) (*CallDetail, error) {
	var d CallDetail
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		rows, err := tx.Query(ctx, callSelect+` WHERE c.id = $1`, callID)
		if err != nil {
			return err
		}
		found := rows.Next()
		if !found {
			rows.Close()
			return ErrNotFound
		}
		d.CallSummary, err = scanCall(rows)
		rows.Close()
		if err != nil {
			return err
		}

		if err := tx.QueryRow(ctx,
			`SELECT a.sentiment, a.talk_ratio, a.interruptions, a.next_action
			   FROM analyses a WHERE a.call_id = $1`, callID,
		).Scan(&d.Sentiment, &d.TalkRatio, &d.Interruptions, &d.NextAction); err != nil &&
			!errors.Is(err, pgx.ErrNoRows) {
			return err
		}

		d.Transcript, err = transcriptTurns(ctx, tx, callID)
		if err != nil {
			return err
		}
		d.Flags, err = flagsForCall(ctx, tx, callID)
		if err != nil {
			return err
		}
		return auditTx(ctx, tx, id, "call.read", "call", callID, nil)
	})
	if err != nil {
		return nil, err
	}
	return &d, nil
}

// transcriptTurns merges the two channels into a single time-ordered list.
//
// There is no diarization step and there must never be one: the channels are already
// separate, so the speaker is known exactly rather than inferred.
func transcriptTurns(ctx context.Context, tx pgx.Tx, callID string) ([]TranscriptTurn, error) {
	rows, err := tx.Query(ctx,
		`SELECT channel, text, word_timings, confidence FROM transcripts
		  WHERE call_id = $1 ORDER BY channel`, callID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var turns []TranscriptTurn
	for rows.Next() {
		var channel int16
		var text string
		var timings []byte
		var confidence *float64
		if err := rows.Scan(&channel, &text, &timings, &confidence); err != nil {
			return nil, err
		}
		speaker := "borrower"
		if channel == 1 {
			speaker = "agent"
		}
		var words []struct {
			StartMS int    `json:"start_ms"`
			EndMS   int    `json:"end_ms"`
			Text    string `json:"text"`
		}
		if err := json.Unmarshal(timings, &words); err != nil || len(words) == 0 {
			turns = append(turns, TranscriptTurn{
				Channel: channel, Speaker: speaker, Text: text, Confidence: confidence,
			})
			continue
		}
		for _, w := range words {
			turns = append(turns, TranscriptTurn{
				Channel: channel, Speaker: speaker,
				StartMS: w.StartMS, EndMS: w.EndMS, Text: w.Text, Confidence: confidence,
			})
		}
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	sortTurns(turns)
	return turns, nil
}

func sortTurns(t []TranscriptTurn) {
	for i := 1; i < len(t); i++ {
		for j := i; j > 0 && t[j].StartMS < t[j-1].StartMS; j-- {
			t[j], t[j-1] = t[j-1], t[j]
		}
	}
}

func flagsForCall(ctx context.Context, tx pgx.Tx, callID string) ([]Flag, error) {
	rows, err := tx.Query(ctx,
		`SELECT id::text, call_id::text, rule_id, rule_set_version, severity, tier,
		        span_start_ms, span_end_ms, evidence_text, judge_rationale, status,
		        reviewer_uid, agent_response, resolved_at
		   FROM flags WHERE call_id = $1 ORDER BY severity, span_start_ms`, callID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanFlags(rows)
}

func scanFlags(rows pgx.Rows) ([]Flag, error) {
	var out []Flag
	for rows.Next() {
		var f Flag
		if err := rows.Scan(&f.ID, &f.CallID, &f.RuleID, &f.RuleSetVersion, &f.Severity,
			&f.Tier, &f.SpanStartMS, &f.SpanEndMS, &f.EvidenceText, &f.JudgeRationale,
			&f.Status, &f.ReviewerUID, &f.AgentResponse, &f.ResolvedAt); err != nil {
			return nil, err
		}
		out = append(out, f)
	}
	return out, rows.Err()
}

// FlagFilter drives the compliance queue.
type FlagFilter struct {
	Severity string
	Status   string
	RuleID   string
	Limit    int
}

func (s *Store) ListFlags(ctx context.Context, id *auth.Identity, f FlagFilter) ([]Flag, error) {
	var out []Flag
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		limit := f.Limit
		if limit <= 0 || limit > 200 {
			limit = 50
		}
		rows, err := tx.Query(ctx,
			`SELECT id::text, call_id::text, rule_id, rule_set_version, severity, tier,
			        span_start_ms, span_end_ms, evidence_text, judge_rationale, status,
			        reviewer_uid, agent_response, resolved_at
			   FROM flags
			  WHERE ($1::text IS NULL OR severity = $1)
			    AND ($2::text IS NULL OR status = $2)
			    AND ($3::text IS NULL OR rule_id = $3)
			  ORDER BY array_position(ARRAY['critical','high','medium','low'], severity),
			           created_at DESC
			  LIMIT $4`,
			nullText(f.Severity), nullText(f.Status), nullText(f.RuleID), limit)
		if err != nil {
			return err
		}
		defer rows.Close()
		out, err = scanFlags(rows)
		return err
	})
	return out, err
}

// UpdateFlag assigns, upholds or dismisses a flag, writing the reviewer trail.
func (s *Store) UpdateFlag(ctx context.Context, id *auth.Identity, flagID, status, reviewerUID, note string, at time.Time) (*Flag, error) {
	var f Flag
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		var resolvedAt *time.Time
		if status == "upheld" || status == "dismissed" {
			resolvedAt = &at
		}
		row := tx.QueryRow(ctx,
			`UPDATE flags
			    SET status = COALESCE(NULLIF($2,''), status),
			        reviewer_uid = COALESCE(NULLIF($3,''), reviewer_uid),
			        reviewer_note = COALESCE(NULLIF($4,''), reviewer_note),
			        resolved_at = COALESCE($5, resolved_at)
			  WHERE id = $1
			  RETURNING id::text, call_id::text, rule_id, rule_set_version, severity, tier,
			            span_start_ms, span_end_ms, evidence_text, judge_rationale, status,
			            reviewer_uid, agent_response, resolved_at`,
			flagID, status, reviewerUID, note, resolvedAt)
		if err := row.Scan(&f.ID, &f.CallID, &f.RuleID, &f.RuleSetVersion, &f.Severity,
			&f.Tier, &f.SpanStartMS, &f.SpanEndMS, &f.EvidenceText, &f.JudgeRationale,
			&f.Status, &f.ReviewerUID, &f.AgentResponse, &f.ResolvedAt); err != nil {
			if errors.Is(err, pgx.ErrNoRows) {
				return ErrNotFound
			}
			return err
		}
		return auditTx(ctx, tx, id, "flag.update", "flag", flagID,
			map[string]any{"status": status})
	})
	if err != nil {
		return nil, err
	}
	return &f, nil
}

// RespondToFlag records an agent's response to a flag on their own call.
//
// An agent who can see and contest a flag treats the system as a tool; one who only
// hears about flags in a review treats it as surveillance. RLS restricts the update
// to the agent's own calls.
func (s *Store) RespondToFlag(ctx context.Context, id *auth.Identity, flagID, response string) (*Flag, error) {
	var f Flag
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		row := tx.QueryRow(ctx,
			`UPDATE flags SET agent_response = $2 WHERE id = $1
			 RETURNING id::text, call_id::text, rule_id, rule_set_version, severity, tier,
			           span_start_ms, span_end_ms, evidence_text, judge_rationale, status,
			           reviewer_uid, agent_response, resolved_at`,
			flagID, response)
		if err := row.Scan(&f.ID, &f.CallID, &f.RuleID, &f.RuleSetVersion, &f.Severity,
			&f.Tier, &f.SpanStartMS, &f.SpanEndMS, &f.EvidenceText, &f.JudgeRationale,
			&f.Status, &f.ReviewerUID, &f.AgentResponse, &f.ResolvedAt); err != nil {
			if errors.Is(err, pgx.ErrNoRows) {
				return ErrNotFound
			}
			return err
		}
		return auditTx(ctx, tx, id, "flag.agent_response", "flag", flagID, nil)
	})
	if err != nil {
		return nil, err
	}
	return &f, nil
}

// Confirmation is an agent's correction of a call's disposition and PTP.
type Confirmation struct {
	Disposition string
	PTPPresent  bool
	AmountPaise *int64
	DueDate     *string
	Note        string
}

// ConfirmCall records the agent's corrections without overwriting the model's
// extraction: both are kept so accuracy can be measured against real corrections.
func (s *Store) ConfirmCall(ctx context.Context, id *auth.Identity, callID string, c Confirmation, windowHours int, at time.Time) error {
	return s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		var endedAt *time.Time
		if err := tx.QueryRow(ctx, `SELECT ended_at FROM calls WHERE id = $1`, callID).
			Scan(&endedAt); err != nil {
			if errors.Is(err, pgx.ErrNoRows) {
				return ErrNotFound
			}
			return err
		}
		if endedAt != nil && at.Sub(*endedAt) > time.Duration(windowHours)*time.Hour {
			return ErrCorrectionWindowClosed
		}
		if _, err := tx.Exec(ctx,
			`INSERT INTO ptps (tenant_id, call_id, confidence, agent_confirmed,
			                   agent_amount_paise, agent_due_date, corrected_at)
			 SELECT c.tenant_id, c.id, 0, $2, $3, $4::date, $5 FROM calls c WHERE c.id = $1
			 ON CONFLICT (call_id) DO UPDATE
			   SET agent_confirmed = excluded.agent_confirmed,
			       agent_amount_paise = excluded.agent_amount_paise,
			       agent_due_date = excluded.agent_due_date,
			       corrected_at = excluded.corrected_at`,
			callID, c.PTPPresent, c.AmountPaise, c.DueDate, at); err != nil {
			return err
		}
		detail := map[string]any{"disposition": c.Disposition, "ptp_present": c.PTPPresent}
		return auditTx(ctx, tx, id, "call.confirm", "call", callID, detail)
	})
}

var ErrCorrectionWindowClosed = errors.New("store: the correction window has closed")

func nullText(s string) *string {
	if s == "" {
		return nil
	}
	return &s
}

func nullUUID(s string) *string { return nullText(s) }
