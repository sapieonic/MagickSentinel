package store

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"time"

	"github.com/jackc/pgx/v5"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
)

// ListDevices returns the fleet plus the tier distribution, which is how a customer
// sees how much of the floor is stuck on degraded tier B capture and worth upgrading
// to Windows 11.
func (s *Store) ListDevices(ctx context.Context, id *auth.Identity, status string) ([]Device, map[string]int, error) {
	var devices []Device
	tiers := map[string]int{"A": 0, "B": 0}
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		rows, err := tx.Query(ctx,
			`SELECT d.id::text, d.tenant_id::text, d.machine_guid, d.os_build, d.capture_tier,
			        d.agent_version, d.pinned_device_id, d.status, d.last_seen_at,
			        d.last_capture_state,
			        (SELECT CASE WHEN sum(cd.dialer_calls) > 0
			                     THEN 100.0 * sum(cd.captured_calls) / sum(cd.dialer_calls) END
			           FROM coverage_daily cd
			          WHERE cd.tenant_id = d.tenant_id
			            AND cd.date >= current_date - 7)
			   FROM devices d
			  WHERE ($1::text IS NULL OR d.status = $1)
			  ORDER BY d.last_seen_at DESC NULLS LAST`, nullText(status))
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var d Device
			if err := rows.Scan(&d.ID, &d.TenantID, &d.MachineGUID, &d.OSBuild, &d.CaptureTier,
				&d.AgentVersion, &d.PinnedDeviceID, &d.Status, &d.LastSeenAt,
				&d.LastCaptureState, &d.CoveragePct7d); err != nil {
				return err
			}
			tiers[d.CaptureTier]++
			devices = append(devices, d)
		}
		return rows.Err()
	})
	return devices, tiers, err
}

type User struct {
	FirebaseUID string  `json:"firebase_uid"`
	TenantID    string  `json:"tenant_id"`
	Role        string  `json:"role"`
	TeamID      *string `json:"team_id"`
	DisplayName string  `json:"display_name"`
	Status      string  `json:"status"`
}

func (s *Store) ListUsers(ctx context.Context, id *auth.Identity) ([]User, error) {
	var out []User
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		rows, err := tx.Query(ctx,
			`SELECT firebase_uid, tenant_id::text, role, team_id::text, display_name, status
			   FROM users ORDER BY display_name`)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var u User
			if err := rows.Scan(&u.FirebaseUID, &u.TenantID, &u.Role, &u.TeamID,
				&u.DisplayName, &u.Status); err != nil {
				return err
			}
			out = append(out, u)
		}
		return rows.Err()
	})
	return out, err
}

func (s *Store) UpdateUser(ctx context.Context, id *auth.Identity, uid, role string, teamID *string, status string) (*User, error) {
	var u User
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		row := tx.QueryRow(ctx,
			`UPDATE users
			    SET role = COALESCE(NULLIF($2,''), role),
			        team_id = COALESCE($3::uuid, team_id),
			        status = COALESCE(NULLIF($4,''), status)
			  WHERE firebase_uid = $1
			  RETURNING firebase_uid, tenant_id::text, role, team_id::text, display_name, status`,
			uid, role, teamID, status)
		if err := row.Scan(&u.FirebaseUID, &u.TenantID, &u.Role, &u.TeamID,
			&u.DisplayName, &u.Status); err != nil {
			if errors.Is(err, pgx.ErrNoRows) {
				return ErrNotFound
			}
			return err
		}
		// A role change is a privilege change and belongs in the audit trail. The
		// Identity Platform custom claim is updated separately by the provisioning
		// path; this row is the local mirror.
		return auditTx(ctx, tx, id, "user.update", "user", uid,
			map[string]any{"role": u.Role, "status": u.Status})
	})
	if err != nil {
		return nil, err
	}
	return &u, nil
}

// CreateEnrollmentToken mints a single-use, tenant-scoped token with a 24 h TTL.
//
// Only the hash is stored. A leaked database backup must not be usable to enrol a
// device, and the plaintext is returned exactly once.
func (s *Store) CreateEnrollmentToken(ctx context.Context, id *auth.Identity, now time.Time) (string, time.Time, error) {
	raw := make([]byte, 32)
	if _, err := rand.Read(raw); err != nil {
		return "", time.Time{}, err
	}
	token := base64.RawURLEncoding.EncodeToString(raw)
	expires := now.Add(24 * time.Hour)
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		if _, err := tx.Exec(ctx,
			`INSERT INTO enrollment_tokens (token_hash, tenant_id, created_by, expires_at)
			 VALUES ($1, $2, $3, $4)`,
			HashEnrollmentToken(token), id.TenantID, id.UserUID, expires); err != nil {
			return err
		}
		return auditTx(ctx, tx, id, "enrollment_token.create", "device", "", nil)
	})
	if err != nil {
		return "", time.Time{}, err
	}
	return token, expires, nil
}

// HashEnrollmentToken is the one-way function the token is stored under.
func HashEnrollmentToken(token string) string {
	sum := sha256.Sum256([]byte(token))
	return hex.EncodeToString(sum[:])
}

var ErrTokenUnusable = errors.New("store: enrollment token is invalid, expired or already used")

// ConsumeEnrollmentToken atomically claims a token and returns its tenant.
//
// Single-use is enforced inside sentinel_consume_enrollment_token by the
// `consumed_at IS NULL` predicate: two racing enrollments cannot both win, because
// only one of them affects a row.
func (s *Store) ConsumeEnrollmentToken(ctx context.Context, token string, now time.Time) (tenantID string, err error) {
	err = s.pool.QueryRow(ctx,
		`SELECT sentinel_consume_enrollment_token($1, $2)::text`,
		HashEnrollmentToken(token), now,
	).Scan(&tenantID)
	if errors.Is(err, pgx.ErrNoRows) {
		return "", ErrTokenUnusable
	}
	return tenantID, err
}

// RegisterDevice records an enrolled device. It is the second of the three bootstrap
// operations: the tenant comes from the token just consumed, not from the caller.
func (s *Store) RegisterDevice(ctx context.Context, tenantID, machineGUID, hwFingerprint, certFingerprint, osBuild, tier, agentVersion string, notAfter time.Time) (string, error) {
	var deviceID string
	err := s.pool.QueryRow(ctx,
		`SELECT sentinel_register_device($1, $2, $3, $4, $5, $6, $7, $8)::text`,
		tenantID, machineGUID, hwFingerprint, certFingerprint, notAfter, osBuild, tier, agentVersion,
	).Scan(&deviceID)
	return deviceID, err
}

type RuleSet struct {
	ID         string          `json:"id"`
	Version    int             `json:"version"`
	Active     bool            `json:"active"`
	CreatedAt  time.Time       `json:"created_at"`
	CreatedBy  string          `json:"created_by"`
	Definition json.RawMessage `json:"definition"`
}

func (s *Store) ActiveRuleSet(ctx context.Context, id *auth.Identity) (*RuleSet, error) {
	var rs RuleSet
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		return tx.QueryRow(ctx,
			`SELECT id::text, version, active, created_at, created_by, definition
			   FROM rule_sets WHERE active ORDER BY version DESC LIMIT 1`,
		).Scan(&rs.ID, &rs.Version, &rs.Active, &rs.CreatedAt, &rs.CreatedBy, &rs.Definition)
	})
	if errors.Is(err, pgx.ErrNoRows) {
		return nil, ErrNotFound
	}
	if err != nil {
		return nil, err
	}
	return &rs, nil
}

// PublishRuleSet creates the next version and makes it active.
//
// Existing versions are never mutated: a flag raised last month must stay traceable
// to the exact rule text that raised it, which is what a bank asks for when it
// challenges a finding.
func (s *Store) PublishRuleSet(ctx context.Context, id *auth.Identity, definition json.RawMessage) (*RuleSet, error) {
	var rs RuleSet
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		if _, err := tx.Exec(ctx,
			`UPDATE rule_sets SET active = false WHERE tenant_id = $1 AND active`,
			id.TenantID); err != nil {
			return err
		}
		if err := tx.QueryRow(ctx,
			`INSERT INTO rule_sets (tenant_id, version, definition, active, created_by)
			 VALUES ($1,
			         COALESCE((SELECT max(version) FROM rule_sets WHERE tenant_id = $1), 0) + 1,
			         $2, true, $3)
			 RETURNING id::text, version, active, created_at, created_by, definition`,
			id.TenantID, definition, id.UserUID,
		).Scan(&rs.ID, &rs.Version, &rs.Active, &rs.CreatedAt, &rs.CreatedBy, &rs.Definition); err != nil {
			return err
		}
		return auditTx(ctx, tx, id, "rule_set.publish", "rule_set", rs.ID,
			map[string]any{"version": rs.Version})
	})
	if err != nil {
		return nil, err
	}
	return &rs, nil
}

type AuditEntry struct {
	ID       int64           `json:"id"`
	ActorUID *string         `json:"actor_uid"`
	Action   string          `json:"action"`
	Entity   string          `json:"entity"`
	EntityID *string         `json:"entity_id"`
	At       time.Time       `json:"at"`
	Detail   json.RawMessage `json:"detail"`
}

func (s *Store) AuditEntries(ctx context.Context, id *auth.Identity, actorUID, entity string, limit int) ([]AuditEntry, error) {
	if limit <= 0 || limit > 500 {
		limit = 100
	}
	var out []AuditEntry
	err := s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		rows, err := tx.Query(ctx,
			`SELECT id, actor_uid, action, entity, entity_id, at, COALESCE(detail, '{}'::jsonb)
			   FROM audit_log
			  WHERE ($1::text IS NULL OR actor_uid = $1)
			    AND ($2::text IS NULL OR entity = $2)
			  ORDER BY at DESC LIMIT $3`,
			nullText(actorUID), nullText(entity), limit)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var e AuditEntry
			if err := rows.Scan(&e.ID, &e.ActorUID, &e.Action, &e.Entity, &e.EntityID,
				&e.At, &e.Detail); err != nil {
				return err
			}
			out = append(out, e)
		}
		return rows.Err()
	})
	return out, err
}
