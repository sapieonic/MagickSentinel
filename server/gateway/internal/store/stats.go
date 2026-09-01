package store

import (
	"context"
	"sort"
	"time"

	"github.com/jackc/pgx/v5"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
)

// AgentStats is the agent self-view and the unit a scorecard is built from.
type AgentStats struct {
	UserUID           string   `json:"user_uid"`
	DisplayName       string   `json:"display_name"`
	From              string   `json:"from"`
	To                string   `json:"to"`
	Calls             int      `json:"calls"`
	CoveragePct       *float64 `json:"coverage_pct"`
	PTPRate           float64  `json:"ptp_rate"`
	PTPAmountPaise    int64    `json:"ptp_amount_paise"`
	AvgSentimentDelta *float64 `json:"avg_sentiment_delta"`
	TalkRatio         *float64 `json:"talk_ratio"`
	FlagsPer1000      float64  `json:"flags_per_1000"`
}

const statsQuery = `
WITH scope AS (
  SELECT c.id, c.user_uid
    FROM calls c
   WHERE c.started_at >= $2 AND c.started_at < $3
     AND ($1::text IS NULL OR c.user_uid = $1)
),
agg AS (
  SELECT s.user_uid,
         count(*)                                                   AS calls,
         count(*) FILTER (WHERE a.disposition = 'ptp')              AS ptp_calls,
         COALESCE(sum(COALESCE(p.agent_amount_paise, p.amount_paise)), 0) AS ptp_paise,
         avg((a.sentiment->>'delta')::float8)                       AS sentiment_delta,
         avg(a.talk_ratio)                                          AS talk_ratio,
         (SELECT count(*) FROM flags f WHERE f.call_id IN (SELECT id FROM scope s2
                                                            WHERE s2.user_uid = s.user_uid)) AS flag_count
    FROM scope s
    LEFT JOIN analyses a ON a.call_id = s.id
    LEFT JOIN ptps p ON p.call_id = s.id
   GROUP BY s.user_uid
)
SELECT agg.user_uid, COALESCE(u.display_name, agg.user_uid), agg.calls, agg.ptp_calls,
       agg.ptp_paise, agg.sentiment_delta, agg.talk_ratio, agg.flag_count,
       (SELECT CASE WHEN sum(cd.dialer_calls) > 0
                    THEN 100.0 * sum(cd.captured_calls) / sum(cd.dialer_calls) END
          FROM coverage_daily cd
         WHERE cd.user_uid = agg.user_uid
           AND cd.date >= $2::date AND cd.date < $3::date)
  FROM agg LEFT JOIN users u ON u.firebase_uid = agg.user_uid
`

func (s *Store) agentStats(ctx context.Context, tx pgx.Tx, userUID string, from, to time.Time) ([]AgentStats, error) {
	rows, err := tx.Query(ctx, statsQuery, nullText(userUID), from, to)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var out []AgentStats
	for rows.Next() {
		var a AgentStats
		var ptpCalls int
		if err := rows.Scan(&a.UserUID, &a.DisplayName, &a.Calls, &ptpCalls,
			&a.PTPAmountPaise, &a.AvgSentimentDelta, &a.TalkRatio, new(int), &a.CoveragePct); err != nil {
			return nil, err
		}
		if a.Calls > 0 {
			a.PTPRate = float64(ptpCalls) / float64(a.Calls)
		}
		a.From, a.To = from.Format("2006-01-02"), to.Format("2006-01-02")
		out = append(out, a)
	}
	return out, rows.Err()
}

// AgentStats returns one agent's numbers. RLS keeps an agent to their own row.
func (s *Store) AgentStats(ctx context.Context, id *auth.Identity, userUID string, from, to time.Time) (AgentStats, error) {
	out := AgentStats{
		UserUID: userUID,
		From:    from.Format("2006-01-02"),
		To:      to.Format("2006-01-02"),
	}
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		rows, err := s.agentStats(ctx, tx, userUID, from, to)
		if err != nil {
			return err
		}
		if len(rows) > 0 {
			out = rows[0]
		}
		return nil
	})
	return out, err
}

// TeamScorecards returns each agent's numbers plus the team median.
//
// The median, not a rank. Leaderboards on a collections floor produce gaming, not
// improvement, so the API does not offer an ordering that invites one.
func (s *Store) TeamScorecards(ctx context.Context, id *auth.Identity, teamID string, from, to time.Time) (AgentStats, []AgentStats, error) {
	var agents []AgentStats
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		var err error
		agents, err = s.agentStats(ctx, tx, "", from, to)
		return err
	})
	if err != nil {
		return AgentStats{}, nil, err
	}
	if agents == nil {
		agents = []AgentStats{}
	}
	// Sort by name so the response order carries no ranking signal.
	sort.Slice(agents, func(i, j int) bool { return agents[i].DisplayName < agents[j].DisplayName })
	return medianOf(agents, from, to), agents, nil
}

func medianOf(agents []AgentStats, from, to time.Time) AgentStats {
	m := AgentStats{
		UserUID:     "median",
		DisplayName: "Team median",
		From:        from.Format("2006-01-02"),
		To:          to.Format("2006-01-02"),
	}
	if len(agents) == 0 {
		return m
	}
	m.Calls = int(medianFloat(collect(agents, func(a AgentStats) (float64, bool) {
		return float64(a.Calls), true
	})))
	m.PTPRate = medianFloat(collect(agents, func(a AgentStats) (float64, bool) {
		return a.PTPRate, a.Calls > 0
	}))
	m.PTPAmountPaise = int64(medianFloat(collect(agents, func(a AgentStats) (float64, bool) {
		return float64(a.PTPAmountPaise), true
	})))
	if v := collect(agents, func(a AgentStats) (float64, bool) {
		if a.AvgSentimentDelta == nil {
			return 0, false
		}
		return *a.AvgSentimentDelta, true
	}); len(v) > 0 {
		d := medianFloat(v)
		m.AvgSentimentDelta = &d
	}
	if v := collect(agents, func(a AgentStats) (float64, bool) {
		if a.TalkRatio == nil {
			return 0, false
		}
		return *a.TalkRatio, true
	}); len(v) > 0 {
		d := medianFloat(v)
		m.TalkRatio = &d
	}
	if v := collect(agents, func(a AgentStats) (float64, bool) {
		if a.CoveragePct == nil {
			return 0, false
		}
		return *a.CoveragePct, true
	}); len(v) > 0 {
		d := medianFloat(v)
		m.CoveragePct = &d
	}
	return m
}

func collect(agents []AgentStats, f func(AgentStats) (float64, bool)) []float64 {
	var out []float64
	for _, a := range agents {
		if v, ok := f(a); ok {
			out = append(out, v)
		}
	}
	return out
}

func medianFloat(v []float64) float64 {
	if len(v) == 0 {
		return 0
	}
	sort.Float64s(v)
	mid := len(v) / 2
	if len(v)%2 == 1 {
		return v[mid]
	}
	return (v[mid-1] + v[mid]) / 2
}
