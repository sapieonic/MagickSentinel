import { describe, expect, it } from 'vitest';
import { ROLES } from '../api/types.js';
import type { Role } from '../api/types.js';
import { CAPABILITIES, can, canBrowseCalls, canPlayAudio, canViewRuleDefinitions } from './roles.js';
import type { Capability } from './roles.js';

/**
 * Transcribed straight from spec 13.4 so a matrix edit has to be made twice,
 * deliberately, to pass. `policy` cells are excluded — they are canPlayAudio's job.
 */
const SPEC_TABLE: Record<Capability, readonly Role[]> = {
  own_calls: ['agent', 'supervisor', 'qa', 'compliance', 'admin'],
  team_calls: ['supervisor', 'qa', 'compliance', 'admin'],
  all_tenant_calls: ['qa', 'compliance', 'admin'],
  flagged_calls_only: ['client'],
  resolve_flags: ['qa', 'compliance', 'admin'],
  edit_rules: ['compliance', 'admin'],
  manage_devices_users: ['admin'],
};

describe('role matrix', () => {
  it.each(CAPABILITIES)('matches spec 13.4 for every role: %s', (capability) => {
    const allowed = SPEC_TABLE[capability];
    for (const role of ROLES) {
      expect({ capability, role, allowed: can(role, capability) }).toEqual({
        capability,
        role,
        allowed: allowed.includes(role),
      });
    }
  });

  it('denies everything for a missing role', () => {
    for (const capability of CAPABILITIES) {
      expect(can(null, capability)).toBe(false);
      expect(can(undefined, capability)).toBe(false);
    }
  });

  it('never lets an agent reach other agents’ calls or the rule bodies', () => {
    expect(can('agent', 'team_calls')).toBe(false);
    expect(can('agent', 'all_tenant_calls')).toBe(false);
    expect(canBrowseCalls('agent')).toBe(false);
    expect(canViewRuleDefinitions('agent')).toBe(false);
    expect(can('agent', 'resolve_flags')).toBe(false);
    expect(can('agent', 'manage_devices_users')).toBe(false);
  });

  it('keeps the bank client out of everything but flagged calls', () => {
    expect(can('client', 'own_calls')).toBe(false);
    expect(can('client', 'team_calls')).toBe(false);
    expect(can('client', 'all_tenant_calls')).toBe(false);
    expect(can('client', 'flagged_calls_only')).toBe(true);
    expect(canBrowseCalls('client')).toBe(false);
  });
});

describe('canPlayAudio', () => {
  it('gates agents and bank clients on the tenant policy', () => {
    for (const role of ['agent', 'client'] as const) {
      expect(canPlayAudio(role, false)).toBe(false);
      expect(canPlayAudio(role, true)).toBe(true);
    }
  });

  it('always allows supervisor, qa, compliance and admin regardless of the flag', () => {
    for (const role of ['supervisor', 'qa', 'compliance', 'admin'] as const) {
      expect(canPlayAudio(role, false)).toBe(true);
      expect(canPlayAudio(role, true)).toBe(true);
    }
  });

  it('denies an unknown or absent role even when the tenant flag is on', () => {
    expect(canPlayAudio(null, true)).toBe(false);
    expect(canPlayAudio(undefined, true)).toBe(false);
  });
});
