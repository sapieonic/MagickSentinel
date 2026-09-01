-- Sentinel core schema.
--
-- Tenant isolation is enforced by row-level security, not by application WHERE
-- clauses. One missed clause in a multi-tenant collections product is a reportable
-- incident, so the database refuses to hand out another tenant's rows even if the
-- query asks for them.

BEGIN;

CREATE EXTENSION IF NOT EXISTS pgcrypto;
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE tenants (
  id                         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  name                       text NOT NULL,
  idp_tenant_id              text UNIQUE NOT NULL,
  timezone                   text NOT NULL DEFAULT 'Asia/Kolkata',
  audio_retention_days       int  NOT NULL DEFAULT 30,
  transcript_retention_days  int  NOT NULL DEFAULT 365,
  offline_grace_hours        int  NOT NULL DEFAULT 8,
  idle_signout_minutes       int  NOT NULL DEFAULT 30,
  ptp_correction_window_hours int NOT NULL DEFAULT 24,
  allow_agent_audio_playback boolean NOT NULL DEFAULT false,
  monthly_budget_paise       bigint,
  policy_version             int  NOT NULL DEFAULT 1,
  policy                     jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at                 timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE teams (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id  uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name       text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, name)
);

CREATE TABLE users (
  firebase_uid text PRIMARY KEY,
  tenant_id    uuid NOT NULL REFERENCES tenants(id),
  role         text NOT NULL CHECK (role IN
                 ('agent','supervisor','qa','compliance','admin','client')),
  team_id      uuid REFERENCES teams(id),
  display_name text NOT NULL,
  status       text NOT NULL DEFAULT 'active' CHECK (status IN ('active','suspended')),
  created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON users (tenant_id, team_id);

CREATE TABLE enrollment_tokens (
  token_hash text PRIMARY KEY,
  tenant_id  uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  created_by text NOT NULL,
  expires_at timestamptz NOT NULL,
  consumed_at timestamptz,
  consumed_by_device uuid,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON enrollment_tokens (tenant_id, expires_at);

CREATE TABLE devices (
  id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id         uuid NOT NULL REFERENCES tenants(id),
  machine_guid      text NOT NULL,
  hw_fingerprint    text NOT NULL,
  cert_fingerprint  text NOT NULL,
  cert_not_after    timestamptz,
  os_build          text NOT NULL,
  capture_tier      char(1) NOT NULL CHECK (capture_tier IN ('A','B')),
  agent_version     text NOT NULL,
  pinned_device_id  text,
  status            text NOT NULL DEFAULT 'active' CHECK (status IN ('active','revoked')),
  revoked_at        timestamptz,
  revoked_reason    text,
  last_seen_at      timestamptz,
  last_capture_state text,
  last_spool_depth  int,
  agent_restarts    int NOT NULL DEFAULT 0,
  UNIQUE (tenant_id, machine_guid)
);
CREATE INDEX ON devices (tenant_id, status, last_seen_at DESC);
CREATE UNIQUE INDEX ON devices (cert_fingerprint);

CREATE TABLE calls (
  id             uuid PRIMARY KEY,          -- ULID minted by the client
  tenant_id      uuid NOT NULL REFERENCES tenants(id),
  device_id      uuid NOT NULL REFERENCES devices(id),
  user_uid       text NOT NULL REFERENCES users(firebase_uid),
  team_id        uuid REFERENCES teams(id),
  started_at     timestamptz NOT NULL,
  ended_at       timestamptz,
  duration_ms    int,
  direction      text CHECK (direction IN ('outbound','inbound')),
  account_ref    text,
  dialer_call_id text,
  capture_tier   char(1) NOT NULL CHECK (capture_tier IN ('A','B')),
  end_reason     text,
  -- Denormalised so the bank-client RLS predicate ("flagged calls only") does not
  -- have to consult `flags`, which would make the two policies mutually recursive.
  -- Maintained by trigger, never written by the application.
  has_flags      boolean NOT NULL DEFAULT false,
  status         text NOT NULL DEFAULT 'ingesting'
                   CHECK (status IN ('ingesting','transcribing','analyzing',
                                     'complete','failed','discarded')),
  created_at     timestamptz NOT NULL DEFAULT now(),
  updated_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON calls (tenant_id, started_at DESC);
CREATE INDEX ON calls (tenant_id, user_uid, started_at DESC);
CREATE INDEX ON calls (tenant_id, account_ref);
CREATE INDEX ON calls (tenant_id, status) WHERE status <> 'complete';
CREATE INDEX ON calls (tenant_id, started_at DESC) WHERE has_flags;

CREATE TABLE media_segments (
  tenant_id     uuid NOT NULL REFERENCES tenants(id),
  call_id       uuid NOT NULL REFERENCES calls(id) ON DELETE CASCADE,
  channel       smallint NOT NULL CHECK (channel IN (0,1)),
  seq           int NOT NULL,
  s3_key        text NOT NULL,
  bytes         int NOT NULL DEFAULT 0,
  duration_ms   int NOT NULL,
  timestamp_ms  bigint NOT NULL,
  foreign_audio boolean NOT NULL DEFAULT false,
  silence_inserted boolean NOT NULL DEFAULT false,
  received_at   timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (call_id, channel, seq)
);
CREATE INDEX ON media_segments (tenant_id, call_id);

-- Cumulative ack watermark per (call, channel). The client deletes spool rows only
-- once the watermark advances past them.
CREATE TABLE ingest_watermarks (
  call_id      uuid NOT NULL REFERENCES calls(id) ON DELETE CASCADE,
  channel      smallint NOT NULL CHECK (channel IN (0,1)),
  through_seq  int NOT NULL,
  updated_at   timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (call_id, channel)
);

CREATE TABLE transcripts (
  tenant_id    uuid NOT NULL REFERENCES tenants(id),
  call_id      uuid NOT NULL REFERENCES calls(id) ON DELETE CASCADE,
  channel      smallint NOT NULL CHECK (channel IN (0,1)),
  asr_provider text NOT NULL,
  asr_version  text NOT NULL,
  language     text NOT NULL,
  text         text NOT NULL,
  word_timings jsonb NOT NULL DEFAULT '[]'::jsonb,
  confidence   real,
  embedding    vector(1024),
  created_at   timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (call_id, channel)
);
CREATE INDEX ON transcripts USING gin (to_tsvector('simple', text));

CREATE TABLE analyses (
  call_id        uuid PRIMARY KEY REFERENCES calls(id) ON DELETE CASCADE,
  tenant_id      uuid NOT NULL REFERENCES tenants(id),
  prompt_version text NOT NULL,
  model          text NOT NULL,
  summary        text NOT NULL,
  disposition    text NOT NULL,
  next_action    text,
  sentiment      jsonb NOT NULL,
  talk_ratio     real,
  interruptions  int,
  input_tokens   int,
  output_tokens  int,
  cost_paise     bigint,
  truncated      boolean NOT NULL DEFAULT false,
  created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON analyses (tenant_id, created_at DESC);

CREATE TABLE ptps (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id          uuid NOT NULL REFERENCES tenants(id),
  call_id            uuid NOT NULL REFERENCES calls(id) ON DELETE CASCADE,
  amount_paise       bigint,
  due_date           date,
  confidence         real NOT NULL,
  extracted_span     int4range,
  agent_confirmed    boolean,
  agent_amount_paise bigint,
  agent_due_date     date,
  corrected_at       timestamptz,
  created_at         timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX ON ptps (call_id);
CREATE INDEX ON ptps (tenant_id, due_date);

CREATE TABLE rule_sets (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id  uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  version    int NOT NULL,
  definition jsonb NOT NULL,
  active     boolean NOT NULL DEFAULT false,
  created_by text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, version)
);
CREATE UNIQUE INDEX one_active_rule_set_per_tenant
  ON rule_sets (tenant_id) WHERE active;

CREATE TABLE prompt_templates (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id  uuid REFERENCES tenants(id) ON DELETE CASCADE,  -- null = global default
  kind       text NOT NULL CHECK (kind IN ('analysis','judge')),
  version    int NOT NULL,
  template   text NOT NULL,
  active     boolean NOT NULL DEFAULT false,
  created_by text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, kind, version)
);

CREATE TABLE flags (
  id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id        uuid NOT NULL REFERENCES tenants(id),
  call_id          uuid NOT NULL REFERENCES calls(id) ON DELETE CASCADE,
  rule_id          text NOT NULL,
  rule_set_version int NOT NULL,
  severity         text NOT NULL CHECK (severity IN ('low','medium','high','critical')),
  tier             smallint NOT NULL CHECK (tier IN (1,2)),
  span_start_ms    int,
  span_end_ms      int,
  evidence_text    text,
  judge_rationale  text,
  judge_confidence real,
  status           text NOT NULL DEFAULT 'open'
                     CHECK (status IN ('open','assigned','upheld','dismissed')),
  reviewer_uid     text,
  reviewer_note    text,
  agent_response   text,
  resolved_at      timestamptz,
  created_at       timestamptz NOT NULL DEFAULT now(),
  UNIQUE (call_id, rule_id, tier)
);
CREATE INDEX ON flags (tenant_id, status, severity);
CREATE INDEX ON flags (tenant_id, call_id);

CREATE FUNCTION calls_sync_has_flags() RETURNS trigger
  LANGUAGE plpgsql SECURITY DEFINER AS $fn$
BEGIN
  UPDATE calls SET has_flags = EXISTS (SELECT 1 FROM flags f WHERE f.call_id = c.id)
  FROM calls c
  WHERE calls.id = c.id AND c.id = COALESCE(NEW.call_id, OLD.call_id);
  RETURN NULL;
END
$fn$;

CREATE TRIGGER flags_sync_call AFTER INSERT OR DELETE ON flags
  FOR EACH ROW EXECUTE FUNCTION calls_sync_has_flags();

CREATE TABLE coverage_daily (
  tenant_id        uuid NOT NULL REFERENCES tenants(id),
  user_uid         text NOT NULL,
  date             date NOT NULL,
  dialer_calls     int,
  captured_calls   int,
  dialer_minutes   int,
  captured_minutes int,
  gap_reason       text,
  PRIMARY KEY (tenant_id, user_uid, date)
);

CREATE TABLE device_events (
  id         bigserial PRIMARY KEY,
  tenant_id  uuid NOT NULL REFERENCES tenants(id),
  device_id  uuid NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  kind       text NOT NULL,
  at         timestamptz NOT NULL DEFAULT now(),
  count      int,
  detail     text
);
CREATE INDEX ON device_events (tenant_id, device_id, at DESC);

CREATE TABLE audit_log (
  id        bigserial PRIMARY KEY,
  tenant_id uuid NOT NULL,
  actor_uid text,
  action    text NOT NULL,
  entity    text NOT NULL,
  entity_id text,
  at        timestamptz NOT NULL DEFAULT now(),
  detail    jsonb
);
CREATE INDEX ON audit_log (tenant_id, at DESC);
CREATE INDEX ON audit_log (tenant_id, entity, entity_id);

COMMIT;
