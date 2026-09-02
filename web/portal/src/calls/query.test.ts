import { describe, expect, it } from 'vitest';
import { EMPTY_FILTERS, buildCallQuery, isoDateDaysAgo } from './query.js';
import type { Role } from '@sentinel/shared';

const DAY_MS = 24 * 3600_000;

describe('buildCallQuery', () => {
  it('sends nothing but the limit when no filter is set', () => {
    expect(buildCallQuery('qa', EMPTY_FILTERS, 200)).toEqual({ limit: 200 });
  });

  it('covers the whole of the day the reviewer picked', () => {
    // The gateway compares started_at < to. A single-day range that sent the same
    // midnight for both ends would return nothing at all.
    const query = buildCallQuery('qa', { ...EMPTY_FILTERS, from: '2026-09-01', to: '2026-09-01' }, 50);
    const from = Date.parse(query.from!);
    const to = Date.parse(query.to!);
    expect(to - from).toBe(DAY_MS);
    // A call at 23:59 local on the chosen day is inside the range.
    expect(new Date(2026, 8, 1, 23, 59).getTime()).toBeLessThan(to);
  });

  it('spans month and year boundaries without arithmetic on the string', () => {
    const query = buildCallQuery('qa', { ...EMPTY_FILTERS, from: '2026-12-31', to: '2026-12-31' }, 50);
    expect(new Date(Date.parse(query.to!)).getFullYear()).toBe(2027);
  });

  it('translates the tri-state compliance filter, including the negative case', () => {
    expect(buildCallQuery('qa', { ...EMPTY_FILTERS, flags: 'any' }, 50).has_flags).toBeUndefined();
    expect(buildCallQuery('qa', { ...EMPTY_FILTERS, flags: 'flagged' }, 50).has_flags).toBe(true);
    expect(buildCallQuery('qa', { ...EMPTY_FILTERS, flags: 'unflagged' }, 50).has_flags).toBe(false);
  });

  it('pins a bank client to flagged calls whatever the form says', () => {
    for (const flags of ['any', 'flagged', 'unflagged'] as const) {
      expect(buildCallQuery('client', { ...EMPTY_FILTERS, flags }, 50).has_flags).toBe(true);
    }
  });

  it('leaves every other role free to ask for unflagged calls', () => {
    const others: Role[] = ['agent', 'supervisor', 'qa', 'compliance', 'admin'];
    for (const role of others) {
      expect(buildCallQuery(role, { ...EMPTY_FILTERS, flags: 'unflagged' }, 50).has_flags).toBe(false);
    }
  });

  it('passes the narrowing filters straight through', () => {
    const query = buildCallQuery(
      'compliance',
      { ...EMPTY_FILTERS, disposition: 'refusal', q: 'unpaid', teamId: 'team-3' },
      25,
    );
    expect(query).toMatchObject({ disposition: 'refusal', q: 'unpaid', team_id: 'team-3', limit: 25 });
  });
});

describe('isoDateDaysAgo', () => {
  it('produces a value a date input accepts', () => {
    expect(isoDateDaysAgo(0, new Date(2026, 8, 1, 12))).toBe('2026-09-01');
    expect(isoDateDaysAgo(1, new Date(2026, 8, 1, 12))).toBe('2026-08-31');
  });

  it('steps back over a year boundary', () => {
    expect(isoDateDaysAgo(30, new Date(2027, 0, 5, 12))).toBe('2026-12-06');
  });
});
