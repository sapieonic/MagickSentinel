import { useCallback, useEffect, useState } from 'react';

export interface AsyncState<T> {
  data: T | null;
  error: unknown;
  loading: boolean;
  reload: () => void;
}

/**
 * Minimal request hook: no cache, no dedupe, no library.
 *
 * Aborts in-flight work on dependency change so a fast filter change cannot land an
 * older response after a newer one — the bug that makes a filtered list show rows
 * that do not match the filter.
 */
export function useAsync<T>(run: (signal: AbortSignal) => Promise<T>, deps: readonly unknown[]): AsyncState<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [loading, setLoading] = useState(true);
  const [nonce, setNonce] = useState(0);

  const reload = useCallback(() => setNonce((n) => n + 1), []);

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setError(null);
    run(controller.signal)
      .then((value) => {
        if (!controller.signal.aborted) setData(value);
      })
      .catch((cause: unknown) => {
        if (!controller.signal.aborted) setError(cause);
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
    // `run` is intentionally not a dependency: callers pass an inline closure, and
    // the explicit dep list is what decides when a refetch is warranted.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, nonce]);

  return { data, error, loading, reload };
}

/**
 * Debounce for the free-text search boxes. 300 ms is the usual tradeoff point:
 * short enough to feel live, long enough that a full-text query over transcripts is
 * not issued per keystroke — that query is expensive server-side and typing "unpaid"
 * would otherwise fire six of them.
 */
export function useDebounced<T>(value: T, ms = 300): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setDebounced(value), ms);
    return () => clearTimeout(id);
  }, [value, ms]);
  return debounced;
}
