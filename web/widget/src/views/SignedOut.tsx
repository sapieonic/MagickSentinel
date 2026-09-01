export function SignedOut({ signingIn, onSignIn }: { signingIn: boolean; onSignIn: () => void }) {
  return (
    <div className="wg-panel wg-signed-out">
      <div className="wg-lock" aria-hidden="true">
        🔒
      </div>
      <p className="wg-signed-out__text">Sign in to start</p>
      <button className="sx-btn--primary wg-block" onClick={onSignIn} disabled={signingIn}>
        {signingIn ? 'Opening browser…' : 'Sign in'}
      </button>
    </div>
  );
}
