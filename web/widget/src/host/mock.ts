/**
 * Browser-development stand-in for the native host object.
 *
 * This exists so the widget can be built and reviewed without a Windows agent
 * running. It is selected only when the real host object is absent, and it drives a
 * scripted call so every state in spec 13.1 can be reached from a browser.
 *
 * It is not a simulation of the agent: it makes no capture decisions and holds no
 * credentials. `getToken` deliberately returns null so a developer running the mock
 * gets an honest "no credentials" error from the API client rather than a silently
 * broken history tab.
 */
import type { CallConfirmation } from '@sentinel/shared';
import type { HostError, HostState, SentinelHost } from './types.js';

const INITIAL: HostState = {
  authState: 'signed_out',
  captureState: 'IDLE',
  tier: 'A',
  coverage: null,
  callId: null,
  error: null,
  pendingCall: null,
  displayName: 'Dev Agent',
};

export interface MockHostOptions {
  /** Start already signed in, for iterating on the in-call views. */
  initial?: Partial<HostState>;
  /** Injectable clock so tests do not depend on wall time. */
  now?: () => number;
}

export class MockSentinelHost implements SentinelHost {
  #state: HostState;
  readonly #listeners = new Set<(state: HostState) => void>();
  readonly #now: () => number;
  #timers: ReturnType<typeof setTimeout>[] = [];

  constructor(options: MockHostOptions = {}) {
    this.#state = { ...INITIAL, ...options.initial };
    this.#now = options.now ?? (() => Date.now());
  }

  async getState(): Promise<HostState> {
    return this.#state;
  }

  async signIn(): Promise<void> {
    this.#patch({ authState: 'signing_in' });
    this.#later(400, () => this.#patch({ authState: 'signed_in', captureState: 'IDLE', coverage: 0.83 }));
  }

  async signOut(): Promise<void> {
    this.#cancelTimers();
    this.#state = { ...INITIAL };
    this.#emit();
  }

  onStateChange(callback: (state: HostState) => void): () => void {
    this.#listeners.add(callback);
    return () => this.#listeners.delete(callback);
  }

  async confirmCall(id: string, _payload: CallConfirmation): Promise<void> {
    if (this.#state.pendingCall?.callId !== id) return;
    this.#patch({ pendingCall: null, callId: null });
  }

  async openPortal(path: string): Promise<void> {
    // In the real host this opens the default browser at the portal deep link.
    window.open(path, '_blank', 'noopener');
  }

  async getToken(): Promise<string | null> {
    return null;
  }

  /* -------------------------------------------------- dev-only scripting */

  /** Runs a whole call: ARMED -> IN_CALL -> WRAP -> post-call card. */
  simulateCall(callId = `dev-${Math.floor(this.#now() / 1000)}`): void {
    this.#patch({ captureState: 'ARMED', callId });
    this.#later(1500, () => this.#patch({ captureState: 'IN_CALL' }));
    this.#later(12_000, () => this.#patch({ captureState: 'WRAP' }));
    this.#later(15_000, () =>
      this.#patch({
        captureState: 'IDLE',
        pendingCall: { callId, endedAt: new Date(this.#now()).toISOString(), endedAtEpochMs: this.#now() },
      }),
    );
  }

  simulateError(error: HostError): void {
    this.#patch({ captureState: 'BLOCKED', error });
  }

  clearError(): void {
    this.#patch({ captureState: 'IDLE', error: null });
  }

  setTier(tier: HostState['tier']): void {
    this.#patch({ tier });
  }

  /* ---------------------------------------------------------- internals */

  #patch(patch: Partial<HostState>): void {
    this.#state = { ...this.#state, ...patch };
    this.#emit();
  }

  #emit(): void {
    for (const listener of this.#listeners) listener(this.#state);
  }

  #later(ms: number, fn: () => void): void {
    this.#timers.push(setTimeout(fn, ms));
  }

  #cancelTimers(): void {
    for (const timer of this.#timers) clearTimeout(timer);
    this.#timers = [];
  }
}
