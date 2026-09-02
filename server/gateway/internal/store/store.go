// Package store is the Postgres access layer.
//
// Every query runs inside a transaction that has first set the row-level security
// context from the verified identity:
//
//	SET LOCAL sentinel.tenant_id = ...
//	SET LOCAL sentinel.user_uid  = ...
//	SET LOCAL sentinel.role      = ...
//
// The gateway connects as `sentinel_app`, which is NOBYPASSRLS, so a query that
// forgets its tenant filter returns nothing rather than another tenant's rows. That
// is the whole design: application-level WHERE clauses are the second line of
// defence here, not the first.
package store

import (
	"context"
	"errors"
	"fmt"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
)

var ErrNotFound = errors.New("store: not found")

type Store struct {
	pool *pgxpool.Pool
}

func New(pool *pgxpool.Pool) *Store { return &Store{pool: pool} }

func Open(ctx context.Context, dsn string) (*Store, error) {
	cfg, err := pgxpool.ParseConfig(dsn)
	if err != nil {
		return nil, err
	}
	// Statement caching interacts badly with SET LOCAL on pooled connections in
	// some poolers; describe-by-exec avoids preparing across the boundary.
	cfg.ConnConfig.DefaultQueryExecMode = pgx.QueryExecModeDescribeExec
	pool, err := pgxpool.NewWithConfig(ctx, cfg)
	if err != nil {
		return nil, err
	}
	if err := pool.Ping(ctx); err != nil {
		pool.Close()
		return nil, err
	}
	return &Store{pool: pool}, nil
}

func (s *Store) Close() { s.pool.Close() }

func (s *Store) Pool() *pgxpool.Pool { return s.pool }

// AsIdentity runs fn inside a transaction carrying the caller's RLS context.
//
// SET LOCAL is transaction-scoped, so the context cannot leak to the next borrower
// of the pooled connection — which is exactly why the settings are applied here and
// not on connect.
func (s *Store) AsIdentity(ctx context.Context, id *auth.Identity, fn func(pgx.Tx) error) error {
	if id == nil || id.TenantID == "" {
		return errors.New("store: refusing to run without a verified identity")
	}
	tx, err := s.pool.Begin(ctx)
	if err != nil {
		return err
	}
	defer func() { _ = tx.Rollback(ctx) }()

	if _, err := tx.Exec(ctx,
		`SELECT set_config('sentinel.tenant_id', $1, true),
		        set_config('sentinel.user_uid',  $2, true),
		        set_config('sentinel.role',      $3, true)`,
		id.TenantID, id.UserUID, string(id.Role),
	); err != nil {
		return fmt.Errorf("store: set rls context: %w", err)
	}
	if err := fn(tx); err != nil {
		return err
	}
	return tx.Commit(ctx)
}

// AsSystem runs fn with no RLS context but an explicit tenant, for paths that have no
// user: ingest writes, the pipeline, and retention jobs.
//
// It still sets sentinel.tenant_id, so the policies constrain it to one tenant. The
// role is `admin` because these paths legitimately write rows for any user in that
// tenant; what they cannot do is cross a tenant boundary.
func (s *Store) AsSystem(ctx context.Context, tenantID string, fn func(pgx.Tx) error) error {
	if tenantID == "" {
		return errors.New("store: refusing to run system work with no tenant")
	}
	return s.AsIdentity(ctx, &auth.Identity{
		TenantID: tenantID,
		UserUID:  "system",
		Role:     auth.RoleAdmin,
	}, fn)
}

// ---------------------------------------------------------------- devices

type Device struct {
	ID              string
	TenantID        string
	MachineGUID     string
	OSBuild         string
	CaptureTier     string
	AgentVersion    string
	PinnedDeviceID  *string
	Status          string
	LastSeenAt      *time.Time
	LastCaptureState *string
	CoveragePct7d   *float64
}

// DeviceByCertFingerprint resolves a presented client certificate to a device.
//
// This is one of the three bootstrap lookups that legitimately run before a tenant
// context exists — it is how we learn which tenant the caller belongs to. Rather than
// weaken the RLS policies for it, it goes through a narrow SECURITY DEFINER function
// (db/migrations/0005) that returns only the three fields needed to build an
// identity.
func (s *Store) DeviceByCertFingerprint(ctx context.Context, fingerprint string) (deviceID, tenantID, status string, err error) {
	err = s.pool.QueryRow(ctx,
		`SELECT device_id::text, tenant_id::text, status FROM sentinel_device_by_cert($1)`,
		fingerprint,
	).Scan(&deviceID, &tenantID, &status)
	if errors.Is(err, pgx.ErrNoRows) {
		return "", "", "", ErrNotFound
	}
	return deviceID, tenantID, status, err
}

// TouchDevice records a heartbeat.
func (s *Store) TouchDevice(ctx context.Context, tenantID, deviceID, captureState, tier, osBuild, version string, spoolDepth int, at time.Time) error {
	return s.AsSystem(ctx, tenantID, func(tx pgx.Tx) error {
		_, err := tx.Exec(ctx,
			`UPDATE devices
			    SET last_seen_at = $3, last_capture_state = $4, capture_tier = $5,
			        os_build = $6, agent_version = $7, last_spool_depth = $8
			  WHERE id = $2 AND tenant_id = $1`,
			tenantID, deviceID, at, captureState, tier, osBuild, version, spoolDepth)
		return err
	})
}

// RecordDeviceEvent appends a client-reported event. Detail must never contain call
// content: transcripts, account references and borrower names are barred from logs
// and from this table.
func (s *Store) RecordDeviceEvent(ctx context.Context, tenantID, deviceID, kind string, count *int, detail string, at time.Time) error {
	return s.AsSystem(ctx, tenantID, func(tx pgx.Tx) error {
		_, err := tx.Exec(ctx,
			`INSERT INTO device_events (tenant_id, device_id, kind, at, count, detail)
			 VALUES ($1, $2, $3, $4, $5, $6)`,
			tenantID, deviceID, kind, at, count, detail)
		return err
	})
}

// RevokeDevice marks a device revoked. Terminating its live connections within 60 s
// is the ingest layer's job; this is the durable half.
func (s *Store) RevokeDevice(ctx context.Context, id *auth.Identity, deviceID, reason string, at time.Time) error {
	return s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		tag, err := tx.Exec(ctx,
			`UPDATE devices SET status = 'revoked', revoked_at = $2, revoked_reason = $3
			  WHERE id = $1 AND status = 'active'`,
			deviceID, at, reason)
		if err != nil {
			return err
		}
		if tag.RowsAffected() == 0 {
			return ErrNotFound
		}
		return auditTx(ctx, tx, id, "device.revoke", "device", deviceID,
			map[string]any{"reason": reason})
	})
}

// DeviceStatus reports whether a device is still active, for the revocation poll.
func (s *Store) DeviceStatus(ctx context.Context, tenantID, deviceID string) (string, error) {
	var status string
	err := s.AsSystem(ctx, tenantID, func(tx pgx.Tx) error {
		return tx.QueryRow(ctx, `SELECT status FROM devices WHERE id = $1`, deviceID).Scan(&status)
	})
	if errors.Is(err, pgx.ErrNoRows) {
		return "", ErrNotFound
	}
	return status, err
}

// ---------------------------------------------------------------- policy

type Policy struct {
	Version                 int64
	OfflineGraceHours       int
	IdleSignoutMinutes      int
	AllowAgentAudioPlayback bool
	AudioRetentionDays      int
	TranscriptRetentionDays int
	RulesVersion            int64
	Raw                     []byte
}

func (s *Store) PolicyForTenant(ctx context.Context, tenantID string) (Policy, error) {
	var p Policy
	err := s.AsSystem(ctx, tenantID, func(tx pgx.Tx) error {
		return tx.QueryRow(ctx,
			`SELECT t.policy_version, t.offline_grace_hours, t.idle_signout_minutes,
			        t.allow_agent_audio_playback, t.audio_retention_days,
			        t.transcript_retention_days,
			        COALESCE((SELECT version FROM rule_sets r
			                   WHERE r.tenant_id = t.id AND r.active), 0),
			        t.policy
			   FROM tenants t WHERE t.id = $1`, tenantID,
		).Scan(&p.Version, &p.OfflineGraceHours, &p.IdleSignoutMinutes,
			&p.AllowAgentAudioPlayback, &p.AudioRetentionDays,
			&p.TranscriptRetentionDays, &p.RulesVersion, &p.Raw)
	})
	if errors.Is(err, pgx.ErrNoRows) {
		return p, ErrNotFound
	}
	return p, err
}

// ---------------------------------------------------------------- audit

// Audit records an action. Reads and exports of call content are audited as well as
// writes: a compliance product has to be able to answer "who listened to this
// borrower's call".
func (s *Store) Audit(ctx context.Context, id *auth.Identity, action, entity, entityID string, detail map[string]any) error {
	return s.AsIdentity(ctx, id, func(tx pgx.Tx) error {
		return auditTx(ctx, tx, id, action, entity, entityID, detail)
	})
}

func auditTx(ctx context.Context, tx pgx.Tx, id *auth.Identity, action, entity, entityID string, detail map[string]any) error {
	_, err := tx.Exec(ctx,
		`INSERT INTO audit_log (tenant_id, actor_uid, action, entity, entity_id, detail)
		 VALUES ($1, $2, $3, $4, $5, $6)`,
		id.TenantID, id.UserUID, action, entity, nullable(entityID), detail)
	return err
}

func nullable(s string) *string {
	if s == "" {
		return nil
	}
	return &s
}
