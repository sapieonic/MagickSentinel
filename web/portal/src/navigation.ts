/**
 * Which screens a role may reach, and where it lands.
 *
 * Kept out of the component so it can be asserted directly. The regression this
 * guards against is quiet and serious: a new screen added to the nav without a
 * capability, letting an agent see that other people's calls and flags exist. Spec
 * 13.4 asks for the link not to be there at all, not merely for the request to 403.
 */
import type { Capability, Role } from '@sentinel/shared';
import { can } from '@sentinel/shared';

export interface NavEntry {
  to: string;
  label: string;
  /** Every entry must name one; there is no "visible to everyone" option by design. */
  capability: Capability;
}

export const NAV: readonly NavEntry[] = [
  { to: '/me', label: 'My work', capability: 'own_calls' },
  { to: '/calls', label: 'Call explorer', capability: 'team_calls' },
  { to: '/compliance', label: 'Compliance', capability: 'resolve_flags' },
  { to: '/fleet', label: 'Fleet', capability: 'manage_devices_users' },
  { to: '/rules', label: 'Rules', capability: 'edit_rules' },
  { to: '/scorecards', label: 'Scorecards', capability: 'team_calls' },
  { to: '/live', label: 'Live floor', capability: 'team_calls' },
  { to: '/client', label: 'Compliance posture', capability: 'flagged_calls_only' },
];

export function navFor(role: Role | null): NavEntry[] {
  return NAV.filter((entry) => can(role, entry.capability));
}

/** Lands each role on the screen it actually works in. */
export function defaultRoute(role: Role | null): string {
  if (can(role, 'flagged_calls_only')) return '/client';
  if (can(role, 'resolve_flags')) return '/compliance';
  if (can(role, 'team_calls')) return '/calls';
  return '/me';
}
