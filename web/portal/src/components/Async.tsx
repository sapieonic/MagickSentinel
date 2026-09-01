/**
 * The three states every remote list in the portal has, in one place so QA sees the
 * same wording everywhere and an empty result is never mistaken for a failure.
 */
import type { ReactNode } from 'react';
import { ApiError } from '@sentinel/shared';

export function LoadState({ label = 'Loading…' }: { label?: string }) {
  return <p className="pt-state sx-muted">{label}</p>;
}

export function EmptyState({ label }: { label: string }) {
  return <p className="pt-state sx-muted">{label}</p>;
}

export function ErrorState({ error, onRetry }: { error: unknown; onRetry?: () => void }) {
  return (
    <div className="pt-state sx-error">
      <p>{describe(error)}</p>
      {onRetry ? <button onClick={onRetry}>Retry</button> : null}
    </div>
  );
}

/**
 * Never renders a raw server message for 4xx/5xx bodies we did not author — an
 * upstream can put anything in there, including data the viewer should not see.
 * request_id is surfaced so support can find the log line.
 */
function describe(error: unknown): string {
  if (error instanceof ApiError) {
    if (error.isTransport) return 'Could not reach the server. Check your connection and retry.';
    if (error.isAuthFailure) return 'Your session expired. Sign in again.';
    if (error.isForbidden) return 'You do not have access to this.';
    if (error.isNotFound) return 'Not found, or not visible at your access level.';
    return error.requestId ? `Something went wrong (ref ${error.requestId}).` : 'Something went wrong.';
  }
  return 'Something went wrong.';
}

export function Panel({ title, actions, children }: { title: string; actions?: ReactNode; children: ReactNode }) {
  return (
    <section className="pt-panel">
      <header className="pt-panel__head">
        <h2>{title}</h2>
        {actions}
      </header>
      {children}
    </section>
  );
}
