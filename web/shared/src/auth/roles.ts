/**
 * The role matrix from spec section 13.4, as data.
 *
 * This mirrors `server/gateway/internal/auth/auth.go`. The server is the security
 * boundary — this copy exists so the UI does not offer navigation to something the
 * gateway will refuse, which is a usability job, not an authorisation one. If the
 * two ever disagree, the Go matrix wins and this file is the bug.
 */
import type { Role } from '../api/types.js';

/** Capability names match the Go `Capability` constants exactly. */
export type Capability =
  | 'own_calls'
  | 'team_calls'
  | 'all_tenant_calls'
  | 'flagged_calls_only'
  | 'resolve_flags'
  | 'edit_rules'
  | 'manage_devices_users';

export const CAPABILITIES: readonly Capability[] = [
  'own_calls',
  'team_calls',
  'all_tenant_calls',
  'flagged_calls_only',
  'resolve_flags',
  'edit_rules',
  'manage_devices_users',
];

/**
 * Audio playback is deliberately absent from this table: for `agent` and `client` it
 * is a per-tenant policy decision, so it needs the tenant flag in hand. See
 * `canPlayAudio`.
 */
const MATRIX: Record<Capability, ReadonlySet<Role>> = {
  own_calls: new Set<Role>(['agent', 'supervisor', 'qa', 'compliance', 'admin']),
  team_calls: new Set<Role>(['supervisor', 'qa', 'compliance', 'admin']),
  all_tenant_calls: new Set<Role>(['qa', 'compliance', 'admin']),
  flagged_calls_only: new Set<Role>(['client']),
  resolve_flags: new Set<Role>(['qa', 'compliance', 'admin']),
  edit_rules: new Set<Role>(['compliance', 'admin']),
  manage_devices_users: new Set<Role>(['admin']),
};

/** Whether `role` holds `capability`. Unknown/absent role is always denied. */
export function can(role: Role | null | undefined, capability: Capability): boolean {
  if (!role) return false;
  return MATRIX[capability].has(role);
}

/**
 * The two policy-gated cells. Agents and bank clients get playback only when the
 * tenant's `allow_agent_audio_playback` is on; everyone else with call access always
 * does.
 *
 * The tenant flag is deliberately a required argument: defaulting it would make a
 * missing policy silently permissive, which is the wrong direction for recorded
 * borrower audio.
 */
export function canPlayAudio(role: Role | null | undefined, tenantAllowsAgentPlayback: boolean): boolean {
  switch (role) {
    case 'agent':
    case 'client':
      return tenantAllowsAgentPlayback;
    case 'supervisor':
    case 'qa':
    case 'compliance':
    case 'admin':
      return true;
    default:
      return false;
  }
}

/**
 * A bank client sees flagged calls only — never the general explorer. Kept as its
 * own predicate because "can see some calls" and "can browse calls" are different
 * questions and conflating them is how a client role leaks into the explorer.
 */
export function canBrowseCalls(role: Role | null | undefined): boolean {
  return can(role, 'team_calls') || can(role, 'all_tenant_calls');
}

/**
 * Raw rule definitions are compliance/admin only. An agent who can read the rule
 * bodies can game them, which is precisely what spec 13.4's closing sentence
 * forbids; note that this is stricter than `edit_rules` would suggest on its own.
 */
export function canViewRuleDefinitions(role: Role | null | undefined): boolean {
  return can(role, 'edit_rules');
}
