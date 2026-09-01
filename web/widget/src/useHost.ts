/**
 * Subscribes the widget to native state.
 *
 * One read at mount, then push updates for the lifetime of the widget. There is no
 * polling and no network fallback: spec 6.7 forbids re-fetching over the API
 * anything the native layer already holds, and a poll would also mean the widget's
 * idea of "in call" could lag the recording indicator.
 */
import { useEffect, useState } from 'react';
import { normaliseHostState, resolveHost } from './host/bridge.js';
import type { HostState, SentinelHost } from './host/types.js';

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
        apply(await host.getState());
      } catch {
        // getState failing means the host object is there but the agent process is
        // not answering. BLOCKED/unknown is the honest rendering.
        apply({ captureState: 'BLOCKED' });
      }
    })();

    return () => {
      cancelled = true;
      unsubscribe?.();
    };
  }, []);

  return binding;
}
