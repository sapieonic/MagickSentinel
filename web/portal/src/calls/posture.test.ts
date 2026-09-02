import { describe, expect, it } from 'vitest';
import type { CallSummary } from '@sentinel/shared';
import { summarisePosture } from './posture.js';

function call(overrides: Partial<CallSummary>): CallSummary {
  return {
    id: 'c1',
    started_at: '2026-09-01T10:00:00Z',
    user_uid: 'agent-a',
    capture_tier: 'A',
    status: 'complete',
    flag_count: 1,
    ...overrides,
  };
}

describe('summarisePosture', () => {
  it('reports zeroes rather than blanks for an empty window', () => {
    const posture = summarisePosture([], false);
    expect(posture).toMatchObject({ flaggedCalls: 0, totalFlags: 0, seriousCalls: 0, worst: undefined });
    expect(posture.bySeverity).toEqual({ low: 0, medium: 0, high: 0, critical: 0 });
  });

  it('counts breaches, not calls, when one call trips several rules', () => {
    const posture = summarisePosture(
      [call({ id: 'a', flag_count: 3, max_severity: 'high' }), call({ id: 'b', flag_count: 1, max_severity: 'low' })],
      false,
    );
    expect(posture.flaggedCalls).toBe(2);
    expect(posture.totalFlags).toBe(4);
  });

  it('buckets a call by its worst flag and names the worst overall', () => {
    const posture = summarisePosture(
      [
        call({ id: 'a', max_severity: 'low' }),
        call({ id: 'b', max_severity: 'critical' }),
        call({ id: 'c', max_severity: 'high' }),
      ],
      false,
    );
    expect(posture.bySeverity).toEqual({ low: 1, medium: 0, high: 1, critical: 1 });
    expect(posture.worst).toBe('critical');
    expect(posture.seriousCalls).toBe(2);
  });

  it('reports the window it actually covers, comparing instants not strings', () => {
    const posture = summarisePosture(
      [
        call({ id: 'a', started_at: '2026-09-01T12:00:00Z' }),
        // 09:00Z written in the tenant's own offset. It sorts last as a string and
        // first as an instant, which is exactly the mistake being guarded against.
        call({ id: 'b', started_at: '2026-09-01T14:30:00+05:30' }),
        call({ id: 'c', started_at: '2026-09-01T18:00:00Z' }),
      ],
      false,
    );
    expect(posture.earliest).toBe('2026-09-01T14:30:00+05:30');
    expect(Date.parse(posture.latest!)).toBe(Date.parse('2026-09-01T18:00:00Z'));
  });

  it('carries the truncation flag so a partial count is never shown as a total', () => {
    expect(summarisePosture([call({})], true).partial).toBe(true);
    expect(summarisePosture([call({})], false).partial).toBe(false);
  });
});
