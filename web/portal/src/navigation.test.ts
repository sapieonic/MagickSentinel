import { describe, expect, it } from 'vitest';
import { ROLES } from '@sentinel/shared';
import { NAV, defaultRoute, navFor } from './navigation.js';

describe('portal navigation', () => {
  it('shows an agent nothing but their own work', () => {
    expect(navFor('agent').map((entry) => entry.to)).toEqual(['/me']);
    expect(defaultRoute('agent')).toBe('/me');
  });

  it('never offers an agent another agent’s calls, the flag queue or the rules', () => {
    const paths = navFor('agent').map((entry) => entry.to);
    for (const forbidden of ['/calls', '/compliance', '/fleet', '/rules', '/scorecards', '/live', '/client']) {
      expect(paths).not.toContain(forbidden);
    }
  });

  it('gives the bank client the flagged-calls screen and nothing else', () => {
    expect(navFor('client').map((entry) => entry.to)).toEqual(['/client']);
    expect(defaultRoute('client')).toBe('/client');
  });

  it('keeps device and user management to admin', () => {
    for (const role of ROLES) {
      const hasFleet = navFor(role).some((entry) => entry.to === '/fleet');
      expect({ role, hasFleet }).toEqual({ role, hasFleet: role === 'admin' });
    }
  });

  it('keeps the rule editor to compliance and admin', () => {
    const withRules = ROLES.filter((role) => navFor(role).some((entry) => entry.to === '/rules'));
    expect(withRules).toEqual(['compliance', 'admin']);
  });

  it('gives every role a landing route it is actually allowed to open', () => {
    for (const role of ROLES) {
      const landing = defaultRoute(role);
      const entry = NAV.find((candidate) => candidate.to === landing);
      expect({ role, landing, allowed: navFor(role).includes(entry!) }).toEqual({ role, landing, allowed: true });
    }
  });

  it('signed out sees no navigation at all', () => {
    expect(navFor(null)).toEqual([]);
  });
});
