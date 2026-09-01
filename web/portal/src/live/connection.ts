/**
 * The live floor's connection, as plain logic with no React and no DOM.
 *
 * Two properties make this worth its own file. First, the stream's credential is a
 * single-use ticket: `EventSource` reconnects on its own with the same URL, and
 * that URL's ticket is spent the instant the first connection is made, so the
 * built-in retry would loop forever on a rejected ticket. The connection therefore
 * closes the stream on the first error and mints a fresh ticket for every attempt.
 *
 * Second, the stream is a repeating snapshot of the calls in flight — there is no
 * "call ended" event — so a call that disappears from the snapshot has to be aged
 * out here. Without that, a floor view accumulates calls that ended hours ago and a
 * supervisor stops believing it.
 */
import type { LiveCallEvent, LiveTicket } from '@sentinel/shared';

export interface MessageEventLike {
  data: string;
}

export interface EventSourceLike {
  addEventListener(type: string, listener: (event: MessageEventLike) => void): void;
  close(): void;
  onopen: ((event: unknown) => void) | null;
  onerror: ((event: unknown) => void) | null;
}

export type LiveStatus = 'idle' | 'connecting' | 'live' | 'reconnecting' | 'stopped' | 'refused';

export interface LiveRow extends LiveCallEvent {
  /** Wall clock of the last snapshot this call appeared in. */
  seenAt: number;
}

export interface LiveState {
  status: LiveStatus;
  calls: readonly LiveRow[];
  /** Consecutive failed attempts; zero once a stream is open. */
  attempt: number;
  /** Set only for `refused`, which is terminal. */
  refusal: string | null;
  /** Wall clock of the last snapshot received, or null before the first one. */
  lastEventAt: number | null;
}

export interface LiveConnectionOptions {
  teamId: string;
  mintTicket: (teamId: string, signal: AbortSignal) => Promise<LiveTicket>;
  streamUrl: (teamId: string, ticket: string) => string;
  open: (url: string) => EventSourceLike;
  onChange: (state: LiveState) => void;
  /** Whether a mint failure is permanent (wrong role, revoked session). */
  isTerminal?: (error: unknown) => boolean;
  now?: () => number;
  /** How long a call may go unseen before it is dropped from the floor. */
  staleAfterMs?: number;
  sweepEveryMs?: number;
  retryBaseMs?: number;
  retryMaxMs?: number;
}

/**
 * Five seconds is the gateway's poll interval three times over: long enough that a
 * slow query or a missed tick does not blink a live call off the floor, short
 * enough that an ended call clears while the supervisor is still looking at it.
 */
const DEFAULT_STALE_AFTER_MS = 10_000;
const DEFAULT_SWEEP_MS = 2_000;
const DEFAULT_RETRY_BASE_MS = 1_000;
const DEFAULT_RETRY_MAX_MS = 30_000;

type ResolvedOptions = LiveConnectionOptions &
  Required<Pick<LiveConnectionOptions, 'isTerminal' | 'now' | 'staleAfterMs' | 'sweepEveryMs' | 'retryBaseMs' | 'retryMaxMs'>>;

export class LiveConnection {
  readonly #options: ResolvedOptions;

  #source: EventSourceLike | null = null;
  #mint: AbortController | null = null;
  #retryTimer: ReturnType<typeof setTimeout> | null = null;
  #sweepTimer: ReturnType<typeof setInterval> | null = null;
  #stopped = false;
  #calls = new Map<string, LiveRow>();
  #state: LiveState = { status: 'idle', calls: [], attempt: 0, refusal: null, lastEventAt: null };

  constructor(options: LiveConnectionOptions) {
    this.#options = {
      isTerminal: () => false,
      now: () => Date.now(),
      staleAfterMs: DEFAULT_STALE_AFTER_MS,
      sweepEveryMs: DEFAULT_SWEEP_MS,
      retryBaseMs: DEFAULT_RETRY_BASE_MS,
      retryMaxMs: DEFAULT_RETRY_MAX_MS,
      ...options,
    };
  }

  get state(): LiveState {
    return this.#state;
  }

  start(): void {
    if (this.#stopped || this.#source !== null || this.#mint !== null) return;
    this.#sweepTimer ??= setInterval(() => this.#sweep(), this.#options.sweepEveryMs);
    this.#patch({ status: 'connecting' });
    void this.#connect();
  }

  stop(): void {
    this.#stopped = true;
    this.#teardown();
    this.#stopSweep();
    this.#patch({ status: 'stopped' });
  }

  #stopSweep(): void {
    if (this.#sweepTimer !== null) {
      clearInterval(this.#sweepTimer);
      this.#sweepTimer = null;
    }
  }

  async #connect(): Promise<void> {
    const controller = new AbortController();
    this.#mint = controller;
    let ticket: LiveTicket;
    try {
      ticket = await this.#options.mintTicket(this.#options.teamId, controller.signal);
    } catch (error) {
      this.#mint = null;
      if (this.#stopped || controller.signal.aborted) return;
      if (this.#options.isTerminal(error)) {
        // Terminal: a role that may not watch this floor will not start being
        // allowed to by being asked again every second.
        this.#stopSweep();
        this.#patch({ status: 'refused', refusal: 'Your role may not watch this team’s floor.' });
        return;
      }
      this.#scheduleRetry();
      return;
    }
    this.#mint = null;
    if (this.#stopped || controller.signal.aborted) return;

    const source = this.#options.open(this.#options.streamUrl(this.#options.teamId, ticket.ticket));
    this.#source = source;

    source.onopen = () => {
      if (this.#source !== source) return;
      this.#patch({ status: 'live', attempt: 0 });
    };
    // The gateway names its frames `call`, so `onmessage` would never fire.
    source.addEventListener('call', (event) => {
      if (this.#source !== source) return;
      this.#ingest(event.data);
    });
    source.onerror = () => {
      if (this.#source !== source) return;
      // Closing is the point: leaving it open would have EventSource retry the URL
      // whose ticket the gateway has already consumed, forever.
      source.close();
      this.#source = null;
      this.#scheduleRetry();
    };
  }

  #ingest(raw: string): void {
    let event: LiveCallEvent;
    try {
      event = JSON.parse(raw) as LiveCallEvent;
    } catch {
      // A truncated frame is not worth tearing the stream down for; the next
      // snapshot arrives in seconds.
      return;
    }
    if (typeof event.call_id !== 'string' || event.call_id === '') return;
    const now = this.#options.now();
    this.#calls.set(event.call_id, { ...event, seenAt: now });
    this.#patch({ lastEventAt: now, calls: this.#rows() });
  }

  #sweep(): void {
    const cutoff = this.#options.now() - this.#options.staleAfterMs;
    let dropped = false;
    for (const [id, row] of this.#calls) {
      if (row.seenAt < cutoff) {
        this.#calls.delete(id);
        dropped = true;
      }
    }
    if (dropped) this.#patch({ calls: this.#rows() });
  }

  #scheduleRetry(): void {
    if (this.#stopped || this.#retryTimer !== null) return;
    const attempt = this.#state.attempt + 1;
    // Exponential with a ceiling: a gateway that is down should not be asked for a
    // ticket by every open floor view once a second.
    const delay = Math.min(this.#options.retryBaseMs * 2 ** (attempt - 1), this.#options.retryMaxMs);
    this.#patch({ status: 'reconnecting', attempt });
    this.#retryTimer = setTimeout(() => {
      this.#retryTimer = null;
      if (this.#stopped) return;
      void this.#connect();
    }, delay);
  }

  #teardown(): void {
    this.#mint?.abort();
    this.#mint = null;
    this.#source?.close();
    this.#source = null;
    if (this.#retryTimer !== null) {
      clearTimeout(this.#retryTimer);
      this.#retryTimer = null;
    }
  }

  #rows(): LiveRow[] {
    return [...this.#calls.values()].sort((a, b) => Date.parse(a.started_at) - Date.parse(b.started_at));
  }

  #patch(next: Partial<LiveState>): void {
    this.#state = { ...this.#state, ...next };
    this.#options.onChange(this.#state);
  }
}
