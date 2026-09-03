/**
 * Subscribes the widget to native state.
 *
 * One read at mount, then push updates for the lifetime of the widget. There is no
 * polling and no network fallback: spec 6.7 forbids re-fetching over the API
 * anything the native layer already holds, and a poll would also mean the widget's
 * idea of "in call" could lag the recording indicator.
 */
import { useEffect, useState } from 'react';
import { normaliseHostState, resolveHost, withHostTimeout } from './host/bridge.js';
import type { HostState, SentinelHost } from './host/types.js';

/**
 * Deadline on the one blocking host call the widget makes at boot.
 *
 * Two seconds because this is on the critical path to the first frame: the widget must
 * show the agent *something* truthful quickly, and an unresponsive agent process is
 * itself something truthful.
 */
const STATE_READ_TIMEOUT_MS = 2000;

/**
 * What we render when the host object exists but the agent process will not answer.
 * BLOCKED/unknown rather than IDLE, for the same reason `normaliseHostState` fails
 * closed: claiming idle would hide the recording indicator while capture may still be
 * running, and the indicator being wrong is a compliance regression, not a glitch.
 */
const UNRESPONSIVE: unknown = { captureState: 'BLOCKED' };

export interface HostBinding {
  state: HostState | null;
  host: SentinelHost | null;
  native: boolean;
}

const BOOTING: HostBinding = { state: null, host: null, native: false };

export function useHost(): HostBinding {
  const [binding, setBinding] = useState<HostBinding>(BOOTING);

  useEffect(() => {
    let cancelled = false;
    let unsubscribe: (() => void) | undefined;

    void (async () => {
      const { host, native } = await resolveHost();
      if (cancelled) return;

      const apply = (raw: unknown) => {
        if (!cancelled) setBinding({ state: normaliseHostState(raw), host, native });
      };

      // Subscribe before the first read so a transition that lands between the two
      // is not lost — the agent can hang up while the widget is still booting.
      const result = host.onStateChange(apply);
      const resolved = result instanceof Promise ? await result : result;
      if (typeof resolved === 'function') unsubscribe = resolved;

      try {
        // Bounded. A rejection means the agent process answered with an error; an
        // unsettled promise means it did not answer at all, which is the more common
        // of the two and the one that used to leave the widget on "Starting Sentinel"
        // indefinitely. Both land on the same honest rendering.
        apply(await withHostTimeout<unknown>(host.getState(), STATE_READ_TIMEOUT_MS, UNRESPONSIVE));
      } catch {
        apply(UNRESPONSIVE);
      }
    })();

    return () => {
      cancelled = true;
      unsubscribe?.();
    };
  }, []);

  return binding;
}
