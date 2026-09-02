import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiError } from '@sentinel/shared';
import { LiveConnection } from './connection.js';
import type { EventSourceLike, LiveState, MessageEventLike } from './connection.js';

/** Stands in for the browser's EventSource; records what it was opened with. */
class FakeSource implements EventSourceLike {
  static opened: FakeSource[] = [];
  readonly url: string;
  closed = false;
  onopen: ((event: unknown) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;
  #listeners = new Map<string, Array<(event: MessageEventLike) => void>>();

  constructor(url: string) {
    this.url = url;
    FakeSource.opened.push(this);
  }

  addEventListener(type: string, listener: (event: MessageEventLike) => void): void {
    const existing = this.#listeners.get(type) ?? [];
    existing.push(listener);
    this.#listeners.set(type, existing);
  }

  close(): void {
    this.closed = true;
  }

  /* -------------------------------------------------- test-side triggers */

  open(): void {
    this.onopen?.(null);
  }

  emit(payload: unknown): void {
    for (const listener of this.#listeners.get('call') ?? []) listener({ data: JSON.stringify(payload) });
  }

  emitRaw(data: string): void {
    for (const listener of this.#listeners.get('call') ?? []) listener({ data });
  }

  fail(): void {
    this.onerror?.(null);
  }
}

function liveCall(id: string, startedAt = '2026-09-01T10:00:00Z') {
  return { call_id: id, user_uid: 'agent-a', state: 'IN_CALL', started_at: startedAt };
}

function connect(overrides: Partial<ConstructorParameters<typeof LiveConnection>[0]> = {}) {
  const states: LiveState[] = [];
  let minted = 0;
  const connection = new LiveConnection({
    teamId: 'team-1',
    mintTicket: async () => ({ ticket: `t${++minted}`, expires_at: '2026-09-01T10:01:00Z' }),
    streamUrl: (team, ticket) => `https://api.example/v1/teams/${team}/live?ticket=${ticket}`,
    open: (url) => new FakeSource(url),
    onChange: (state) => states.push(state),
    now: () => Date.now(),
    ...overrides,
  });
  return { connection, states, ticketsMinted: () => minted };
}

beforeEach(() => {
  FakeSource.opened = [];
  vi.useFakeTimers();
  vi.setSystemTime(new Date('2026-09-01T10:00:00Z'));
});

afterEach(() => {
  vi.useRealTimers();
});

describe('LiveConnection', () => {
  it('mints a ticket and puts it in the stream URL', async () => {
    const { connection } = connect();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);

    expect(FakeSource.opened).toHaveLength(1);
    expect(FakeSource.opened[0]!.url).toBe('https://api.example/v1/teams/team-1/live?ticket=t1');
    connection.stop();
  });

  it('collects the snapshot frames the gateway names "call"', async () => {
    const { connection } = connect();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    const source = FakeSource.opened[0]!;
    source.open();
    source.emit(liveCall('call-1'));
    source.emit(liveCall('call-2', '2026-09-01T09:00:00Z'));

    expect(connection.state.status).toBe('live');
    // Oldest first: a supervisor scans for the call that has been running longest.
    expect(connection.state.calls.map((c) => c.call_id)).toEqual(['call-2', 'call-1']);
    connection.stop();
  });

  it('replaces a call in place when the next snapshot repeats it', async () => {
    const { connection } = connect();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    const source = FakeSource.opened[0]!;
    source.open();
    source.emit(liveCall('call-1'));
    source.emit({ ...liveCall('call-1'), alert: 'threat language' });

    expect(connection.state.calls).toHaveLength(1);
    expect(connection.state.calls[0]!.alert).toBe('threat language');
    connection.stop();
  });

  it('ignores a frame that is not usable rather than dropping the stream', async () => {
    const { connection } = connect();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    const source = FakeSource.opened[0]!;
    source.open();
    source.emitRaw('{ truncated');
    source.emit({ user_uid: 'agent-a' });
    source.emit(liveCall('call-1'));

    expect(source.closed).toBe(false);
    expect(connection.state.calls.map((c) => c.call_id)).toEqual(['call-1']);
    connection.stop();
  });

  it('mints a fresh ticket for every reconnect, because the last one is spent', async () => {
    const { connection, ticketsMinted } = connect({ retryBaseMs: 1000 });
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    const first = FakeSource.opened[0]!;
    first.open();
    first.fail();

    // Closing the source is what stops EventSource retrying the consumed ticket by
    // itself, forever.
    expect(first.closed).toBe(true);
    expect(connection.state.status).toBe('reconnecting');

    await vi.advanceTimersByTimeAsync(1000);
    expect(ticketsMinted()).toBe(2);
    expect(FakeSource.opened[1]!.url).toContain('ticket=t2');
    expect(FakeSource.opened[1]!.url).not.toContain('ticket=t1');
    connection.stop();
  });

  it('backs off between attempts and stops widening at the ceiling', async () => {
    const { connection } = connect({ retryBaseMs: 1000, retryMaxMs: 4000 });
    connection.start();
    await vi.advanceTimersByTimeAsync(0);

    const delays: number[] = [];
    for (let attempt = 0; attempt < 5; attempt += 1) {
      const source = FakeSource.opened[FakeSource.opened.length - 1]!;
      source.fail();
      let waited = 0;
      // Step forward until the next attempt opens a source, to read the real delay.
      const before = FakeSource.opened.length;
      while (FakeSource.opened.length === before && waited < 20_000) {
        await vi.advanceTimersByTimeAsync(100);
        waited += 100;
      }
      delays.push(waited);
    }

    expect(delays).toEqual([1000, 2000, 4000, 4000, 4000]);
    connection.stop();
  });

  it('resets the backoff once a stream is open again', async () => {
    const { connection } = connect({ retryBaseMs: 1000 });
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    FakeSource.opened[0]!.fail();
    await vi.advanceTimersByTimeAsync(1000);
    FakeSource.opened[1]!.open();
    expect(connection.state.attempt).toBe(0);

    FakeSource.opened[1]!.fail();
    expect(connection.state.attempt).toBe(1);
    connection.stop();
  });

  it('ages out a call the snapshot has stopped mentioning', async () => {
    // The stream carries no "call ended" event, so a call that vanishes from the
    // snapshot has to expire on its own or the floor fills with ghosts.
    const { connection } = connect({ staleAfterMs: 10_000, sweepEveryMs: 1000 });
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    const source = FakeSource.opened[0]!;
    source.open();
    source.emit(liveCall('call-1'));
    source.emit(liveCall('call-2'));

    await vi.advanceTimersByTimeAsync(5000);
    source.emit(liveCall('call-1'));
    await vi.advanceTimersByTimeAsync(6000);

    expect(connection.state.calls.map((c) => c.call_id)).toEqual(['call-1']);
    connection.stop();
  });

  it('gives up on a refusal instead of asking again every second', async () => {
    const { connection, ticketsMinted } = connect({
      mintTicket: async () => {
        throw new ApiError(403, { code: 'forbidden', message: 'role not permitted' });
      },
      isTerminal: (error) => error instanceof ApiError && error.isForbidden,
    });
    connection.start();
    await vi.advanceTimersByTimeAsync(60_000);

    expect(connection.state.status).toBe('refused');
    expect(connection.state.refusal).not.toBeNull();
    expect(ticketsMinted()).toBe(0);
    expect(FakeSource.opened).toHaveLength(0);
    connection.stop();
  });

  it('keeps retrying a transport failure, which is not a refusal', async () => {
    let attempts = 0;
    const { connection } = connect({
      retryBaseMs: 1000,
      mintTicket: async () => {
        attempts += 1;
        if (attempts < 3) throw new ApiError(0, { code: 'network_error', message: 'offline' });
        return { ticket: 'late', expires_at: '2026-09-01T10:01:00Z' };
      },
      isTerminal: (error) => error instanceof ApiError && error.isForbidden,
    });
    connection.start();
    await vi.advanceTimersByTimeAsync(10_000);

    expect(attempts).toBe(3);
    expect(FakeSource.opened[0]!.url).toContain('ticket=late');
    connection.stop();
  });

  it('stops cleanly: closes the stream and cancels every pending timer', async () => {
    const { connection } = connect({ retryBaseMs: 1000, sweepEveryMs: 500 });
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    const source = FakeSource.opened[0]!;
    source.open();
    source.fail();

    connection.stop();
    expect(source.closed).toBe(true);
    expect(connection.state.status).toBe('stopped');

    await vi.advanceTimersByTimeAsync(30_000);
    expect(FakeSource.opened).toHaveLength(1);
    expect(vi.getTimerCount()).toBe(0);
  });

  it('does not open a second stream if start is called twice', async () => {
    const { connection } = connect();
    connection.start();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(FakeSource.opened).toHaveLength(1);
    connection.stop();
  });
});
