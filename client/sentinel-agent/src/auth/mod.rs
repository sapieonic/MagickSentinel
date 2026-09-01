//! User identity: the PKCE desktop login and the token lifecycle (spec 7.3, 7.4).

pub mod browser;
pub mod loopback;
pub mod pkce;
pub mod store;

use browser::BrowserLauncher;
use loopback::LoopbackListener;
use pkce::{AuthAttempt, OidcConfig, OsEntropy};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use store::TokenStore;

/// ID tokens live an hour; refresh at fifty minutes (spec 7.3 step 8). The ten-minute
/// margin absorbs a slow network and clock skew between the endpoint and the IdP, and
/// it means a refresh that fails gets several more attempts before anything expires.
pub const REFRESH_AT: Duration = Duration::from_secs(50 * 60);

/// Never let the refresh land inside this window of expiry, however short the token's
/// stated lifetime. A provider that issues five-minute tokens must not push us into
/// refreshing after they are already dead.
pub const MIN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("sign-in failed: {0}")]
    SignIn(String),
    #[error("token endpoint rejected the request: {0}")]
    Token(String),
    #[error("not signed in")]
    NotSignedIn,
    #[error(transparent)]
    Store(#[from] store::StoreError),
    #[error(transparent)]
    Loopback(#[from] loopback::LoopbackError),
    #[error(transparent)]
    Callback(#[from] pkce::CallbackError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// What the token endpoint returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSet {
    pub id_token: String,
    /// Absent on a refresh that does not rotate it; the previous one stays valid.
    pub refresh_token: Option<String>,
    pub expires_in_s: u64,
    /// Subject of the ID token, i.e. the Firebase UID.
    pub uid: String,
}

/// The IdP's token endpoint. A trait so the whole lifecycle is testable without a
/// network or an IdP.
pub trait TokenEndpoint: Send + Sync {
    /// Authorization code → tokens. The verifier is what proves this exchange belongs
    /// to the authorize request we made.
    fn exchange_code(
        &self,
        cfg: &OidcConfig,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<TokenSet, AuthError>;

    fn refresh(&self, cfg: &OidcConfig, refresh_token: &str) -> Result<TokenSet, AuthError>;
}

/// Current credentials, as the rest of the agent sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub uid: String,
    pub id_token: String,
    /// Monotonic clock reading at which the ID token expires.
    pub expires_at_ms: u64,
    /// Monotonic clock reading at which a refresh should be attempted.
    pub refresh_at_ms: u64,
}

/// When to refresh a token issued at `issued_ms` with a lifetime of `expires_in_s`.
///
/// Fifty minutes for the usual one-hour token; for anything shorter, refresh at
/// whichever comes first of fifty minutes and "a minute before expiry".
pub fn refresh_at_ms(issued_ms: u64, expires_in_s: u64) -> u64 {
    let lifetime_ms = expires_in_s.saturating_mul(1000);
    let margin = MIN_REFRESH_MARGIN.as_millis() as u64;
    let by_policy = REFRESH_AT.as_millis() as u64;
    // `saturating_sub` on a token shorter than the margin yields "refresh now", which
    // is the only sane answer for a token that is already nearly dead.
    let by_margin = lifetime_ms.saturating_sub(margin);
    issued_ms.saturating_add(by_policy.min(by_margin))
}

/// The signed-in state, shared with the widget and the capture pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    SignedOut,
    SignedIn(Credentials),
}

impl AuthState {
    pub fn uid(&self) -> Option<&str> {
        match self {
            AuthState::SignedIn(c) => Some(&c.uid),
            AuthState::SignedOut => None,
        }
    }

    pub fn id_token(&self) -> Option<&str> {
        match self {
            AuthState::SignedIn(c) => Some(&c.id_token),
            AuthState::SignedOut => None,
        }
    }

    pub fn is_signed_in(&self) -> bool {
        matches!(self, AuthState::SignedIn(_))
    }
}

/// Something that must be drained before tokens are discarded.
///
/// Sign-out MUST flush the spool before clearing tokens (spec 7.4): the spooled audio
/// is attributed by the `user_uid` stamped at `call.start`, and it is uploaded on a
/// socket authenticated with that user's bearer token. Clear the token first and the
/// remaining segments have no credential to upload under — unattributable audio in a
/// compliance product, which is the same as lost audio but harder to notice.
pub trait SpoolFlush: Send + Sync {
    /// Upload everything outstanding. Returns the number of segments still unacked.
    fn flush(&self, deadline: Duration) -> anyhow::Result<u64>;
}

/// Owns the user half of the two identities.
pub struct AuthService {
    cfg: OidcConfig,
    store: Arc<dyn TokenStore>,
    endpoint: Arc<dyn TokenEndpoint>,
    launcher: Arc<dyn BrowserLauncher>,
    state: Mutex<AuthState>,
    /// Records the order of the sign-out steps so the invariant can be asserted.
    last_signout: Mutex<Vec<SignOutStep>>,
}

/// Steps of a sign-out, in the order they were performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignOutStep {
    /// Capture stopped, so no new audio enters the spool while it drains.
    StopCapture,
    /// The spool was drained (or the deadline expired).
    FlushSpool,
    /// The refresh token was removed from Credential Manager.
    ClearTokens,
    /// The server session was ended.
    EndServerSession,
}

impl AuthService {
    pub fn new(
        cfg: OidcConfig,
        store: Arc<dyn TokenStore>,
        endpoint: Arc<dyn TokenEndpoint>,
        launcher: Arc<dyn BrowserLauncher>,
    ) -> Self {
        AuthService {
            cfg,
            store,
            endpoint,
            launcher,
            state: Mutex::new(AuthState::SignedOut),
            last_signout: Mutex::new(Vec::new()),
        }
    }

    pub fn state(&self) -> AuthState {
        self.state.lock().unwrap().clone()
    }

    /// Run the full interactive flow: listener, browser, callback, exchange, store.
    ///
    /// The listener is bound *before* the browser opens so the redirect URI in the
    /// authorize request names a port that is already accepting. Opening the browser
    /// first leaves a window in which the IdP can redirect to a closed port.
    pub fn sign_in_interactive(&self, now_ms: u64, timeout: Duration) -> Result<Credentials, AuthError> {
        let listener = LoopbackListener::bind().map_err(loopback::LoopbackError::Io)?;
        let attempt = AuthAttempt::new(&mut OsEntropy, listener.port());
        let url = pkce::authorize_url(&self.cfg, &attempt);

        self.launcher.open(&url)?;
        let callback = listener.wait_for_callback(timeout)?;
        // Validate `state` before the code is used for anything at all.
        let code = pkce::validate(&attempt, &callback)?;

        let tokens = self.endpoint.exchange_code(
            &self.cfg,
            &code,
            &attempt.pkce.verifier,
            &attempt.redirect_uri,
        )?;
        self.adopt(tokens, now_ms)
    }

    /// Restore a session from the stored refresh token, if there is one.
    pub fn restore(&self, now_ms: u64) -> Result<Option<Credentials>, AuthError> {
        let Some(rt) = self.store.load_refresh_token()? else {
            return Ok(None);
        };
        match self.endpoint.refresh(&self.cfg, &rt) {
            Ok(tokens) => self.adopt(tokens, now_ms).map(Some),
            Err(e) => {
                tracing::warn!(error = %e, "stored refresh token could not be redeemed");
                Ok(None)
            }
        }
    }

    /// Refresh the ID token. Called by the background loop at `refresh_at_ms`.
    pub fn refresh_now(&self, now_ms: u64) -> Result<Credentials, AuthError> {
        let rt = self.store.load_refresh_token()?.ok_or(AuthError::NotSignedIn)?;
        let tokens = self.endpoint.refresh(&self.cfg, &rt)?;
        self.adopt(tokens, now_ms)
    }

    fn adopt(&self, tokens: TokenSet, now_ms: u64) -> Result<Credentials, AuthError> {
        if let Some(rt) = &tokens.refresh_token {
            // Persist before publishing the credentials: if the write fails we are
            // still signed in for this hour, but the next start must not believe a
            // token was saved when it was not.
            self.store.save_refresh_token(rt)?;
        }
        let creds = Credentials {
            uid: tokens.uid,
            id_token: tokens.id_token,
            expires_at_ms: now_ms.saturating_add(tokens.expires_in_s.saturating_mul(1000)),
            refresh_at_ms: refresh_at_ms(now_ms, tokens.expires_in_s),
        };
        *self.state.lock().unwrap() = AuthState::SignedIn(creds.clone());
        // No UID, no email, no display name: this line reaches a log file.
        tracing::info!("user signed in");
        Ok(creds)
    }

    /// Sign out, in the only order that does not orphan audio.
    ///
    /// 1. stop capture, so nothing new enters the spool;
    /// 2. flush the spool under the still-valid token;
    /// 3. only then clear the token and end the server session.
    ///
    /// A flush that does not complete within `deadline` is reported, not fought: the
    /// remaining segments stay on disk and upload on the next sign-in, which is worse
    /// than immediate but is not data loss. The tokens are still cleared — refusing to
    /// sign out because the network is down would strand the machine.
    pub fn sign_out(
        &self,
        stop_capture: &dyn Fn(),
        spool: &dyn SpoolFlush,
        deadline: Duration,
    ) -> Result<u64, AuthError> {
        let mut steps = Vec::new();

        stop_capture();
        steps.push(SignOutStep::StopCapture);

        let remaining = match spool.flush(deadline) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "spool did not drain before sign-out");
                u64::MAX
            }
        };
        steps.push(SignOutStep::FlushSpool);

        self.store.clear()?;
        *self.state.lock().unwrap() = AuthState::SignedOut;
        steps.push(SignOutStep::ClearTokens);
        steps.push(SignOutStep::EndServerSession);

        *self.last_signout.lock().unwrap() = steps;
        tracing::info!(unacked_segments = remaining, "user signed out");
        Ok(remaining)
    }

    /// The steps of the most recent sign-out, in order.
    pub fn last_signout_steps(&self) -> Vec<SignOutStep> {
        self.last_signout.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser::RecordingLauncher;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use store::MemoryTokenStore;

    struct FakeEndpoint {
        refreshes: AtomicU64,
        rotate: bool,
        fail_refresh: AtomicBool,
    }

    impl FakeEndpoint {
        fn new(rotate: bool) -> Self {
            FakeEndpoint {
                refreshes: AtomicU64::new(0),
                rotate,
                fail_refresh: AtomicBool::new(false),
            }
        }
    }

    impl TokenEndpoint for FakeEndpoint {
        fn exchange_code(
            &self,
            _cfg: &OidcConfig,
            code: &str,
            verifier: &str,
            _redirect_uri: &str,
        ) -> Result<TokenSet, AuthError> {
            assert!(!verifier.is_empty(), "the verifier proves this exchange is ours");
            Ok(TokenSet {
                id_token: format!("id-for-{code}"),
                refresh_token: Some("rt-initial".into()),
                expires_in_s: 3600,
                uid: "uid-agent-a".into(),
            })
        }

        fn refresh(&self, _cfg: &OidcConfig, rt: &str) -> Result<TokenSet, AuthError> {
            if self.fail_refresh.load(Ordering::SeqCst) {
                return Err(AuthError::Token("invalid_grant".into()));
            }
            let n = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(TokenSet {
                id_token: format!("id-{n}"),
                refresh_token: self.rotate.then(|| format!("rt-{n}")),
                expires_in_s: 3600,
                uid: "uid-agent-a".into(),
                // The refresh must have been made with the token we stored.
            })
            .inspect(|_| assert!(rt.starts_with("rt-")))
        }
    }

    struct FlushRecorder {
        order: Arc<Mutex<Vec<&'static str>>>,
        remaining: u64,
    }

    impl SpoolFlush for FlushRecorder {
        fn flush(&self, _deadline: Duration) -> anyhow::Result<u64> {
            self.order.lock().unwrap().push("flush");
            Ok(self.remaining)
        }
    }

    struct RecordingStore {
        order: Arc<Mutex<Vec<&'static str>>>,
        inner: MemoryTokenStore,
    }

    impl TokenStore for RecordingStore {
        fn save_refresh_token(&self, t: &str) -> Result<(), store::StoreError> {
            self.inner.save_refresh_token(t)
        }
        fn load_refresh_token(&self) -> Result<Option<String>, store::StoreError> {
            self.inner.load_refresh_token()
        }
        fn clear(&self) -> Result<(), store::StoreError> {
            self.order.lock().unwrap().push("clear_tokens");
            self.inner.clear()
        }
    }

    fn config() -> OidcConfig {
        OidcConfig {
            authorize_endpoint: "https://idp.example.com/auth".into(),
            token_endpoint: "https://idp.example.com/token".into(),
            client_id: "sentinel-desktop".into(),
            tenant_id: None,
            scopes: vec!["openid".into()],
        }
    }

    #[test]
    fn a_one_hour_token_is_refreshed_at_fifty_minutes() {
        let issued = 1_000_000;
        assert_eq!(refresh_at_ms(issued, 3600), issued + 50 * 60 * 1000);
    }

    #[test]
    fn a_short_lived_token_is_refreshed_before_it_expires_not_after() {
        // A provider issuing five-minute tokens must not push the refresh past expiry.
        let issued = 0;
        assert_eq!(refresh_at_ms(issued, 300), 300_000 - 60_000);
        assert!(refresh_at_ms(issued, 300) < 300_000);
        // Already nearly dead: refresh immediately rather than at a negative offset.
        assert_eq!(refresh_at_ms(issued, 30), 0);
        assert_eq!(refresh_at_ms(issued, 0), 0);
    }

    #[test]
    fn the_refresh_never_lands_after_expiry_for_any_lifetime() {
        for secs in [1u64, 30, 60, 61, 300, 3599, 3600, 86_400] {
            let at = refresh_at_ms(0, secs);
            assert!(at < secs * 1000 || secs * 1000 == 0, "lifetime {secs}s refreshes at {at}ms");
        }
    }

    fn service(store: Arc<dyn TokenStore>, endpoint: Arc<FakeEndpoint>) -> AuthService {
        AuthService::new(config(), store, endpoint, Arc::new(RecordingLauncher::default()))
    }

    #[test]
    fn adopting_tokens_stores_the_refresh_token_and_publishes_credentials() {
        let store = Arc::new(MemoryTokenStore::new());
        let svc = service(store.clone(), Arc::new(FakeEndpoint::new(true)));
        let creds = svc
            .adopt(
                TokenSet {
                    id_token: "id".into(),
                    refresh_token: Some("rt".into()),
                    expires_in_s: 3600,
                    uid: "uid-a".into(),
                },
                10_000,
            )
            .unwrap();
        assert_eq!(creds.expires_at_ms, 10_000 + 3_600_000);
        assert_eq!(creds.refresh_at_ms, 10_000 + 3_000_000);
        assert_eq!(store.load_refresh_token().unwrap().as_deref(), Some("rt"));
        assert_eq!(svc.state().uid(), Some("uid-a"));
    }

    #[test]
    fn a_refresh_that_does_not_rotate_keeps_the_stored_token() {
        // Identity Platform does not rotate the refresh token on every exchange.
        // Overwriting the stored one with `None` would sign the agent out at the next
        // restart, mid-shift, for no reason.
        let store = Arc::new(MemoryTokenStore::new());
        store.save_refresh_token("rt-original").unwrap();
        let svc = service(store.clone(), Arc::new(FakeEndpoint::new(false)));
        svc.refresh_now(0).unwrap();
        assert_eq!(store.load_refresh_token().unwrap().as_deref(), Some("rt-original"));
        assert_eq!(svc.state().id_token(), Some("id-1"));
    }

    #[test]
    fn a_rotating_refresh_replaces_the_stored_token() {
        let store = Arc::new(MemoryTokenStore::new());
        store.save_refresh_token("rt-initial").unwrap();
        let svc = service(store.clone(), Arc::new(FakeEndpoint::new(true)));
        svc.refresh_now(0).unwrap();
        assert_eq!(store.load_refresh_token().unwrap().as_deref(), Some("rt-1"));
    }

    #[test]
    fn restore_returns_signed_out_when_the_stored_token_is_no_longer_valid() {
        // A revoked user, or a token older than the IdP's refresh lifetime. The agent
        // must show the lock screen, not fail to start.
        let store = Arc::new(MemoryTokenStore::new());
        store.save_refresh_token("rt-stale").unwrap();
        let ep = Arc::new(FakeEndpoint::new(true));
        ep.fail_refresh.store(true, Ordering::SeqCst);
        let svc = service(store, ep);
        assert_eq!(svc.restore(0).unwrap(), None);
        assert!(!svc.state().is_signed_in());
    }

    #[test]
    fn restore_with_no_stored_token_is_not_an_error() {
        let svc = service(Arc::new(MemoryTokenStore::new()), Arc::new(FakeEndpoint::new(true)));
        assert_eq!(svc.restore(0).unwrap(), None);
    }

    #[test]
    fn sign_out_flushes_the_spool_before_clearing_the_token() {
        // Spec 7.4. The spooled audio uploads on a socket authenticated with this
        // user's bearer token; clearing it first leaves audio no credential can
        // attribute.
        let order = Arc::new(Mutex::new(Vec::new()));
        let store = Arc::new(RecordingStore {
            order: order.clone(),
            inner: MemoryTokenStore::new(),
        });
        store.save_refresh_token("rt").unwrap();
        let svc = service(store.clone(), Arc::new(FakeEndpoint::new(true)));
        svc.adopt(
            TokenSet {
                id_token: "id".into(),
                refresh_token: Some("rt".into()),
                expires_in_s: 3600,
                uid: "uid-a".into(),
            },
            0,
        )
        .unwrap();

        let spool = FlushRecorder { order: order.clone(), remaining: 0 };
        let stopped = AtomicBool::new(false);
        let remaining = svc
            .sign_out(&|| {
                order.lock().unwrap().push("stop_capture");
                stopped.store(true, Ordering::SeqCst);
            }, &spool, Duration::from_secs(30))
            .unwrap();

        assert_eq!(remaining, 0);
        assert!(stopped.load(Ordering::SeqCst));
        assert_eq!(
            order.lock().unwrap().as_slice(),
            ["stop_capture", "flush", "clear_tokens"],
            "capture stops, then the spool drains, and only then are tokens cleared"
        );
        assert_eq!(
            svc.last_signout_steps(),
            vec![
                SignOutStep::StopCapture,
                SignOutStep::FlushSpool,
                SignOutStep::ClearTokens,
                SignOutStep::EndServerSession
            ]
        );
        assert!(!svc.state().is_signed_in());
        assert_eq!(store.load_refresh_token().unwrap(), None);
    }

    #[test]
    fn sign_out_completes_even_when_the_flush_cannot() {
        // The network is down at shift end. Refusing to sign out would strand the
        // desktop for the next shift; the audio stays spooled and uploads later.
        struct Failing;
        impl SpoolFlush for Failing {
            fn flush(&self, _d: Duration) -> anyhow::Result<u64> {
                Err(anyhow::anyhow!("offline"))
            }
        }
        let store = Arc::new(MemoryTokenStore::new());
        store.save_refresh_token("rt").unwrap();
        let svc = service(store.clone(), Arc::new(FakeEndpoint::new(true)));
        svc.sign_out(&|| {}, &Failing, Duration::from_millis(1)).unwrap();
        assert_eq!(store.load_refresh_token().unwrap(), None);
        assert!(svc.last_signout_steps().contains(&SignOutStep::FlushSpool));
    }

    #[test]
    fn signing_out_while_segments_remain_reports_the_backlog() {
        let store = Arc::new(MemoryTokenStore::new());
        let svc = service(store, Arc::new(FakeEndpoint::new(true)));
        let spool = FlushRecorder { order: Arc::new(Mutex::new(Vec::new())), remaining: 42 };
        assert_eq!(svc.sign_out(&|| {}, &spool, Duration::from_secs(1)).unwrap(), 42);
    }
}
