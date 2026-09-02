import type { HostErrorCause } from '../host/types.js';
import { errorCopy } from '../state.js';

export function ErrorView({
  cause,
  detail,
  onSignOut,
}: {
  cause: HostErrorCause;
  detail: string | undefined;
  onSignOut: () => void;
}) {
  const copy = errorCopy(cause);
  return (
    <div className="wg-panel">
      <div className="sx-error">
        <strong>{copy.title}</strong>
        <p className="wg-error__body">{copy.body}</p>
        {detail ? <p className="wg-error__detail sx-mono">{detail}</p> : null}
      </div>
      {/* No retry button: none of these causes clear because the widget asked. The
          agent process re-evaluates and pushes a new state when the condition ends. */}
      {!copy.recoverable ? (
        <button className="wg-block" onClick={onSignOut}>
          Sign out
        </button>
      ) : null}
    </div>
  );
}
