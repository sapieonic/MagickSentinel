/**
 * Generated from `contracts/openapi.yaml` (MagickVoice Sentinel API 1.0.0).
 *
 * These types are hand-maintained but are NOT free to diverge: `contracts/` is the
 * source of truth for client, gateway and web alike. When a schema changes there,
 * regenerate/update this file in the same change — a silent drift here produces a
 * front end that compiles happily against a shape the gateway never sends.
 *
 * Conventions taken from the contract:
 *  - `nullable: true` becomes `| null` on a required key, `?: T | null` when the
 *    property is optional, because the gateway omits some optional keys entirely.
 *  - money is `*_paise`, an int64 in paise. It is never a float and never rupees.
 *    Format it only at the display edge (see ../money.ts).
 */

/* ------------------------------------------------------------------ enums */

export type Role = 'agent' | 'supervisor' | 'qa' | 'compliance' | 'admin' | 'client';
export const ROLES: readonly Role[] = ['agent', 'supervisor', 'qa', 'compliance', 'admin', 'client'];

export type Severity = 'low' | 'medium' | 'high' | 'critical';
export const SEVERITIES: readonly Severity[] = ['low', 'medium', 'high', 'critical'];

export type FlagStatus = 'open' | 'assigned' | 'upheld' | 'dismissed';
export const FLAG_STATUSES: readonly FlagStatus[] = ['open', 'assigned', 'upheld', 'dismissed'];

export type Disposition =
  | 'ptp'
  | 'refusal'
  | 'dispute'
  | 'wrong_number'
  | 'no_contact'
  | 'callback_requested'
  | 'partial_payment'
  | 'escalation'
  | 'other';
export const DISPOSITIONS: readonly Disposition[] = [
  'ptp',
  'refusal',
  'dispute',
  'wrong_number',
  'no_contact',
  'callback_requested',
  'partial_payment',
  'escalation',
  'other',
];

export type CaptureTier = 'A' | 'B';

export type CaptureState = 'IDLE' | 'ARMED' | 'IN_CALL' | 'WRAP' | 'FINALIZE' | 'BLOCKED';

export type CallStatus = 'ingesting' | 'transcribing' | 'analyzing' | 'complete' | 'failed' | 'discarded';

export type UserStatus = 'active' | 'suspended';

export type DeviceStatus = 'active' | 'revoked';

/** `status` filter on GET /v1/admin/devices; `stale` is a query-only value. */
export type DeviceStatusFilter = 'active' | 'revoked' | 'stale';

/* ------------------------------------------------------------------ error */

/** The documented error body. `request_id` is optional; log it, never the message body. */
export interface ApiErrorBody {
  code: string;
  message: string;
  request_id?: string;
}

/* ------------------------------------------------------------- device/auth */

export interface EnrollRequest {
  enrollment_token: string;
  csr_pem: string;
  machine_guid: string;
  hw_fingerprint: string;
  os_build: string;
  capture_tier: CaptureTier;
  agent_version: string;
}

export interface EnrollResponse {
  device_id: string;
  certificate_pem: string;
  ca_chain_pem: string;
  not_after: string;
}

export interface User {
  firebase_uid: string;
  tenant_id: string;
  role: Role;
  team_id?: string | null;
  display_name: string;
  status: UserStatus;
}

export interface PinnedDevice {
  container_id: string;
  friendly_name?: string;
}

export interface SoftphoneConfig {
  process_names?: string[];
  /** null disables the UI Automation account-reference scrape. */
  uia_account_ref_selector?: string | null;
}

export interface RetentionConfig {
  audio_days?: number;
  transcript_days?: number;
}

export interface VadConfig {
  speech_ms_to_confirm?: number;
  armed_timeout_ms?: number;
  hangup_silence_ms?: number;
  wrap_ms?: number;
  debounce_ms?: number;
}

export interface Policy {
  version: number;
  pinned_devices: PinnedDevice[];
  softphone: SoftphoneConfig;
  offline_grace_hours: number;
  idle_signout_minutes?: number;
  rules_version: number;
  /** Tenant flag behind the two policy-gated cells of the role matrix. */
  allow_agent_audio_playback?: boolean;
  retention: RetentionConfig;
  vad?: VadConfig;
}

export interface SessionResponse {
  user: User;
  policy: Policy;
  server_time: string;
}

export type HeartbeatEventKind =
  | 'tier_downgrade'
  | 'spool_eviction'
  | 'device_lost'
  | 'device_restored'
  | 'agent_restart'
  | 'capture_error'
  | 'foreign_audio_suppressed';

export interface HeartbeatEvent {
  kind: HeartbeatEventKind;
  at: string;
  count?: number;
  detail?: string;
}

export interface Heartbeat {
  device_id: string;
  capture_state: CaptureState;
  capture_tier: CaptureTier;
  os_build: string;
  agent_version: string;
  spool_depth: number;
  spool_bytes?: number;
  last_call_at?: string | null;
  dialer_session_active?: boolean;
  signed_in?: boolean;
  agent_restarts?: number;
  pinned_device_present?: boolean;
  events?: HeartbeatEvent[];
  sent_at: string;
}

export type HeartbeatCommandKind = 'refetch_policy' | 'stop_capture' | 'update_now' | 'flush_spool';

export interface HeartbeatCommand {
  kind: HeartbeatCommandKind;
  detail?: string;
}

export interface HeartbeatResponse {
  policy_version: number;
  server_time: string;
  commands?: HeartbeatCommand[];
}

export interface Device {
  id: string;
  machine_guid: string;
  os_build: string;
  capture_tier: CaptureTier;
  agent_version: string;
  pinned_device_id?: string | null;
  status: DeviceStatus;
  last_seen_at?: string | null;
  last_capture_state?: CaptureState;
  coverage_pct_7d?: number | null;
}

export interface DevicePage {
  items: Device[];
  tier_distribution: { A?: number; B?: number };
  next_cursor?: string | null;
}

/* ------------------------------------------------------------------ calls */

export interface Ptp {
  present?: boolean;
  amount_paise?: number | null;
  due_date?: string | null;
  confidence?: number;
  /** [start_ms, end_ms] of the utterance the extraction came from. */
  evidence_span_ms?: [number, number];
  agent_confirmed?: boolean | null;
  agent_amount_paise?: number | null;
  agent_due_date?: string | null;
}

export interface CallSummary {
  id: string;
  started_at: string;
  ended_at?: string | null;
  duration_ms?: number | null;
  user_uid?: string;
  account_ref?: string | null;
  direction?: string | null;
  capture_tier: CaptureTier;
  status: CallStatus;
  disposition?: Disposition;
  summary?: string | null;
  ptp?: Ptp | null;
  sentiment_delta?: number | null;
  flag_count?: number;
  max_severity?: Severity;
}

export interface CallPage {
  items: CallSummary[];
  next_cursor?: string | null;
}

export interface TranscriptTurn {
  channel: 0 | 1;
  speaker: 'borrower' | 'agent';
  start_ms: number;
  end_ms: number;
  text: string;
  confidence?: number | null;
}

export interface SentimentPoint {
  t_ms: number;
  /** -1..1 */
  v: number;
}

export interface SentimentSeries {
  far?: SentimentPoint[];
  near?: SentimentPoint[];
  far_open?: number;
  far_close?: number;
  delta?: number;
}

export interface AnalysisProvenance {
  model?: string;
  prompt_version?: string;
  asr_provider?: string;
  asr_version?: string;
}

export interface CallDetail extends CallSummary {
  transcript?: TranscriptTurn[];
  sentiment?: SentimentSeries;
  flags?: Flag[];
  talk_ratio?: number | null;
  interruptions?: number | null;
  next_action?: string | null;
  /**
   * Short-lived signed URL, or null when role/tenant policy forbids playback.
   * Treat null as authoritative: never construct a playback URL client-side.
   */
  audio_url?: string | null;
  analysis?: AnalysisProvenance;
}

export interface CallConfirmation {
  disposition: Disposition;
  ptp_present?: boolean;
  /** Paise. Never rupees, never a float. */
  ptp_amount_paise?: number | null;
  ptp_due_date?: string | null;
  note?: string;
}

/* ------------------------------------------------------------------ flags */

export interface Flag {
  id: string;
  call_id: string;
  rule_id: string;
  rule_set_version?: number;
  severity: Severity;
  /** 1 = deterministic rule, 2 = LLM judge. */
  tier: 1 | 2;
  span_start_ms?: number | null;
  span_end_ms?: number | null;
  evidence_text?: string | null;
  judge_rationale?: string | null;
  status: FlagStatus;
  reviewer_uid?: string | null;
  agent_response?: string | null;
  resolved_at?: string | null;
}

export interface FlagPage {
  items: Flag[];
  next_cursor?: string | null;
}

export interface FlagUpdate {
  status?: FlagStatus;
  reviewer_uid?: string;
  note?: string;
}

export type ExportJobStatus = 'queued' | 'running' | 'ready' | 'failed';

export interface EvidenceExportRequest {
  flag_ids: string[];
  include_audio?: boolean;
}

export interface EvidenceExportJob {
  job_id: string;
  status: ExportJobStatus;
  download_url?: string | null;
}

/* ------------------------------------------------------------------ stats */

export interface AgentStats {
  user_uid?: string;
  display_name?: string;
  from?: string;
  to?: string;
  calls?: number;
  coverage_pct?: number | null;
  /** PTP calls / connected calls. */
  ptp_rate?: number;
  ptp_amount_paise?: number;
  avg_sentiment_delta?: number | null;
  talk_ratio?: number | null;
  flags_per_1000?: number;
}

export interface TeamScorecards {
  median: AgentStats;
  agents: AgentStats[];
}

export interface LiveCallEvent {
  call_id: string;
  user_uid: string;
  display_name?: string;
  state: CaptureState;
  started_at: string;
  elapsed_ms?: number;
  sentiment_far?: number | null;
  sentiment_near?: number | null;
  alert?: string | null;
}

/* ------------------------------------------------------------------ rules */

export interface RuleDefinition {
  rule_id: string;
  enabled: boolean;
  severity: Severity;
  params?: Record<string, unknown>;
}

export interface RuleSetDefinition {
  call_hours?: { start?: string; end?: string; timezone?: string };
  judge_sample_pct?: number;
  rules: RuleDefinition[];
}

export interface RuleSet extends RuleSetDefinition {
  id: string;
  version: number;
  active: boolean;
  created_at: string;
  created_by: string;
}

/* ------------------------------------------------------------------ admin */

export interface EnrollmentToken {
  token: string;
  expires_at: string;
}

export interface UserUpdate {
  role?: Role;
  team_id?: string | null;
  status?: UserStatus;
}

export interface AuditEntry {
  id: number;
  actor_uid?: string | null;
  action: string;
  entity: string;
  entity_id?: string | null;
  at: string;
  /** Contract forbids transcript text, borrower names or account refs in here. */
  detail?: Record<string, unknown>;
}

export interface AuditPage {
  items?: AuditEntry[];
  next_cursor?: string | null;
}

export interface HealthResponse {
  status?: string;
  version?: string;
}
