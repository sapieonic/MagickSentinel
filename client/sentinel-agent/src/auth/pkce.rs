//! RFC 8252 / RFC 7636 desktop authorization-code flow with PKCE (spec 7.3).
//!
//! Everything here is a pure function over strings and a random source, so the whole
//! of the security-relevant logic — challenge derivation, URL construction, callback
//! parsing, `state` validation — is unit tested on any platform. Only opening the
//! browser and holding the loopback socket need the OS, and those live elsewhere.
//!
//! The flow deliberately opens the **system default browser**, never an embedded
//! WebView. Corporate IdPs increasingly refuse embedded webviews outright, which
//! breaks SSO and hardware MFA — and even where they do not, an embedded webview puts
//! this process in a position to read the user's corporate credentials, which is
//! exactly the position a security review will not let us occupy.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// Bytes of entropy behind the verifier, the `state` and the `nonce`.
///
/// RFC 7636 allows a verifier of 43–128 characters; 32 random bytes base64url-encode
/// to exactly 43, the shortest length the RFC permits, and 256 bits is far past what
/// an authorization code's lifetime requires.
const ENTROPY_BYTES: usize = 32;

/// A generated PKCE pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    /// Held locally, sent only in the token exchange.
    pub verifier: String,
    /// Sent in the authorize request.
    pub challenge: String,
}

impl Pkce {
    /// The only method we send. `plain` exists in the RFC for clients that cannot
    /// hash; we can, and offering `plain` would let a downgrade turn PKCE off.
    pub const METHOD: &'static str = "S256";
}

/// Source of unpredictable bytes. A parameter so tests can pin the output; production
/// callers pass [`OsEntropy`].
pub trait Entropy {
    fn fill(&mut self, out: &mut [u8]);
}

/// The operating system CSPRNG.
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn fill(&mut self, out: &mut [u8]) {
        use rand::RngCore;
        rand::thread_rng().fill_bytes(out);
    }
}

/// Derive the S256 challenge for a verifier.
///
/// `BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))`, unpadded — the padding is not
/// optional to omit: an authorization server comparing the string it received against
/// its own unpadded derivation will reject a padded one.
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Generate a verifier and its challenge.
pub fn generate_pkce(entropy: &mut dyn Entropy) -> Pkce {
    let mut bytes = [0u8; ENTROPY_BYTES];
    entropy.fill(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = challenge_for(&verifier);
    Pkce { verifier, challenge }
}

/// Generate a `state` or `nonce` value.
pub fn generate_nonce(entropy: &mut dyn Entropy) -> String {
    let mut bytes = [0u8; ENTROPY_BYTES];
    entropy.fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Identity Platform endpoints and client identity, from local config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcConfig {
    pub authorize_endpoint: String,
    pub token_endpoint: String,
    pub client_id: String,
    /// Identity Platform tenant for this BPO. Sent as `tenantId`, which is how
    /// Identity Platform scopes a sign-in to one customer's user pool.
    pub tenant_id: Option<String>,
    pub scopes: Vec<String>,
}

impl OidcConfig {
    /// Build the config from the machine-scoped local config the installer wrote.
    ///
    /// This is the whole of what moved when the endpoints stopped being hard-coded in
    /// `SentinelAgent`'s `main`. The authorize endpoint, the client id and the Identity
    /// Platform tenant come from `LocalConfig::oidc`, written per tenant by the
    /// installer; the token endpoint defaults to the gateway's
    /// `{api_base}/v1/oauth/token`, whose wire contract —
    /// `application/x-www-form-urlencoded` in, JSON out as
    /// [`crate::api`] parses it — is unchanged and not this function's business.
    ///
    /// OPEN-2 is deliberately not resolved here. Nothing in this mapping names a
    /// provider: the RFC 8252 + PKCE flow is identical against Identity Platform and
    /// against Entra ID, and `identity_platform_tenant` is `Option` precisely so a
    /// provider with no tenant parameter simply leaves it unset.
    pub fn from_local(local: &sentinel_core::config::LocalConfig) -> Self {
        OidcConfig {
            authorize_endpoint: local.oidc.authorize_endpoint.trim().to_string(),
            token_endpoint: local.token_endpoint(),
            client_id: local.oidc.client_id.trim().to_string(),
            // The installer's `TENANTHINT` is the fallback: it is the same value, and
            // an installation that filled in one of the two and not the other should
            // sign in rather than fail on a field that is present under another name.
            tenant_id: local
                .oidc
                .identity_platform_tenant
                .clone()
                .filter(|t| !t.trim().is_empty())
                .or_else(|| local.tenant_hint.clone().filter(|t| !t.trim().is_empty())),
            scopes: local.oidc.scopes.clone(),
        }
    }
}

/// One in-flight sign-in attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthAttempt {
    pub pkce: Pkce,
    pub state: String,
    pub nonce: String,
    pub redirect_uri: String,
}

impl AuthAttempt {
    /// Start an attempt against a loopback listener already bound to `port`.
    ///
    /// RFC 8252 section 7.3 requires the literal `127.0.0.1`, not `localhost`: on a
    /// machine where `localhost` also resolves to `::1`, the browser may connect to
    /// an address the listener is not on, and some IdPs reject a hostname redirect
    /// for a native client outright.
    pub fn new(entropy: &mut dyn Entropy, port: u16) -> Self {
        AuthAttempt {
            pkce: generate_pkce(entropy),
            state: generate_nonce(entropy),
            nonce: generate_nonce(entropy),
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
        }
    }
}

/// Build the URL to open in the system browser.
pub fn authorize_url(cfg: &OidcConfig, attempt: &AuthAttempt) -> String {
    let mut params: Vec<(&str, String)> = vec![
        ("response_type", "code".into()),
        ("client_id", cfg.client_id.clone()),
        ("redirect_uri", attempt.redirect_uri.clone()),
        ("scope", cfg.scopes.join(" ")),
        ("state", attempt.state.clone()),
        ("nonce", attempt.nonce.clone()),
        ("code_challenge", attempt.pkce.challenge.clone()),
        ("code_challenge_method", Pkce::METHOD.into()),
    ];
    if let Some(t) = &cfg.tenant_id {
        params.push(("tenantId", t.clone()));
    }
    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let sep = if cfg.authorize_endpoint.contains('?') { '&' } else { '?' };
    format!("{}{sep}{query}", cfg.authorize_endpoint)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CallbackError {
    #[error("callback arrived on {0}, expected /callback")]
    WrongPath(String),
    #[error("callback carried no authorization code")]
    NoCode,
    #[error("callback carried no state")]
    NoState,
    #[error("state mismatch: the callback did not come from the request we started")]
    StateMismatch,
    #[error("authorization server returned error {code}{}", .description.as_deref().map(|d| format!(": {d}")).unwrap_or_default())]
    Provider { code: String, description: Option<String> },
    #[error("malformed callback request")]
    Malformed,
}

/// What the browser handed back on the loopback socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub state: String,
}

/// Parse the request target of the loopback callback — the `/callback?...` part of
/// the HTTP request line.
///
/// Provider errors are surfaced as errors rather than as a missing code: `access_denied`
/// because the user cancelled and `invalid_client` because the deployment is
/// misconfigured need very different messages in the widget.
pub fn parse_callback(target: &str) -> Result<Callback, CallbackError> {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    if path != "/callback" {
        return Err(CallbackError::WrongPath(path.to_string()));
    }

    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = percent_decode(v);
        match k {
            "code" => code = Some(v),
            "state" => state = Some(v),
            "error" => error = Some(v),
            "error_description" => error_description = Some(v),
            _ => {}
        }
    }

    if let Some(code) = error {
        return Err(CallbackError::Provider { code, description: error_description });
    }
    let state = state.ok_or(CallbackError::NoState)?;
    let code = code.filter(|c| !c.is_empty()).ok_or(CallbackError::NoCode)?;
    Ok(Callback { code, state })
}

/// Validate a parsed callback against the attempt that started it.
///
/// The `state` check is what stops an attacker who can reach the loopback port from
/// injecting their own authorization code into our session. The comparison is
/// length-then-constant-time: the values are the same length in every honest case, so
/// leaking "wrong length" leaks nothing an attacker did not already choose.
pub fn validate(attempt: &AuthAttempt, cb: &Callback) -> Result<String, CallbackError> {
    if !constant_time_eq(attempt.state.as_bytes(), cb.state.as_bytes()) {
        return Err(CallbackError::StateMismatch);
    }
    Ok(cb.code.clone())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Percent-encode for `application/x-www-form-urlencoded` query values.
///
/// The unreserved set from RFC 3986 and nothing else. Notably a space becomes `%20`
/// rather than `+`: `+` is only correct in a form body, and an IdP that treats the
/// query strictly will hand back a scope list containing literal plus signs.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decode a query-string value. Both `%XX` and `+` are accepted, because browsers do
/// emit `+` for spaces when following a form-encoded redirect.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    // A stray `%` is kept literally rather than dropped; silently
                    // eating bytes from an authorization code produces a token
                    // exchange failure with no clue as to why.
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use sentinel_core::config::LocalConfig;

    #[test]
    fn the_oidc_config_comes_from_local_config_rather_than_from_constants() {
        let mut local = LocalConfig {
            api_base_url: "https://api.acme-bpo.example.com/".into(),
            tenant_hint: Some("tenant-from-installer".into()),
            ..LocalConfig::default()
        };
        local.oidc.authorize_endpoint = "https://idp.acme.example.com/o/oauth2/v2/auth".into();
        local.oidc.client_id = "acme-desktop".into();
        local.oidc.identity_platform_tenant = Some("ip-tenant-acme".into());

        let cfg = OidcConfig::from_local(&local);
        assert_eq!(cfg.authorize_endpoint, "https://idp.acme.example.com/o/oauth2/v2/auth");
        assert_eq!(cfg.client_id, "acme-desktop");
        assert_eq!(cfg.tenant_id.as_deref(), Some("ip-tenant-acme"));
        // The token endpoint's URL moved into config; its contract did not.
        assert_eq!(cfg.token_endpoint, "https://api.acme-bpo.example.com/v1/oauth/token");
        assert_eq!(cfg.scopes, vec!["openid", "email", "profile"]);
    }

    #[test]
    fn the_installers_tenant_hint_is_the_fallback_for_the_identity_platform_tenant() {
        // Two properties carrying the same value is a deployment reality; failing on
        // the one that was left blank would be a support ticket, not a safeguard.
        let mut local = LocalConfig {
            tenant_hint: Some("tenant-from-installer".into()),
            ..LocalConfig::default()
        };
        assert_eq!(
            OidcConfig::from_local(&local).tenant_id.as_deref(),
            Some("tenant-from-installer")
        );
        local.oidc.identity_platform_tenant = Some("  ".into());
        assert_eq!(
            OidcConfig::from_local(&local).tenant_id.as_deref(),
            Some("tenant-from-installer"),
            "a blank tenant is not a tenant"
        );
        local.tenant_hint = None;
        local.oidc.identity_platform_tenant = None;
        assert_eq!(
            OidcConfig::from_local(&local).tenant_id, None,
            "a provider with no tenant concept sends no tenantId at all"
        );
    }

    #[test]
    fn a_configured_token_endpoint_overrides_the_gateways() {
        let mut local = LocalConfig {
            api_base_url: "https://api.example.com".into(),
            ..LocalConfig::default()
        };
        local.oidc.token_endpoint = Some("https://idp.example.com/oauth2/v2/token".into());
        assert_eq!(
            OidcConfig::from_local(&local).token_endpoint,
            "https://idp.example.com/oauth2/v2/token"
        );
    }

    use super::*;

    /// Deterministic entropy: each call yields a distinct, reproducible block.
    struct Counter(u8);
    impl Entropy for Counter {
        fn fill(&mut self, out: &mut [u8]) {
            for (i, b) in out.iter_mut().enumerate() {
                *b = self.0.wrapping_add(i as u8);
            }
            self.0 = self.0.wrapping_add(97);
        }
    }

    fn config() -> OidcConfig {
        OidcConfig {
            authorize_endpoint: "https://idp.example.com/o/oauth2/v2/auth".into(),
            token_endpoint: "https://idp.example.com/token".into(),
            client_id: "sentinel-desktop".into(),
            tenant_id: Some("bpo-alpha".into()),
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
        }
    }

    #[test]
    fn s256_matches_the_rfc_7636_test_vector() {
        // RFC 7636 appendix B. If this drifts, every sign-in fails at the token
        // endpoint with a message that says nothing about hashing.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(challenge_for(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn the_verifier_is_a_legal_length_and_charset() {
        let p = generate_pkce(&mut OsEntropy);
        assert_eq!(p.verifier.len(), 43, "RFC 7636 permits 43–128 characters");
        assert!(
            p.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)),
            "the verifier must be unreserved characters only: {}",
            p.verifier
        );
        assert!(!p.challenge.contains('='), "the challenge is base64url without padding");
        assert!(!p.challenge.contains('+') && !p.challenge.contains('/'), "url-safe alphabet");
        assert_eq!(p.challenge, challenge_for(&p.verifier));
    }

    #[test]
    fn successive_attempts_do_not_repeat_their_secrets() {
        let a = AuthAttempt::new(&mut OsEntropy, 5000);
        let b = AuthAttempt::new(&mut OsEntropy, 5000);
        assert_ne!(a.pkce.verifier, b.pkce.verifier);
        assert_ne!(a.state, b.state);
        assert_ne!(a.nonce, b.nonce);
        // state and nonce serve different purposes and must not be the same value.
        assert_ne!(a.state, a.nonce);
        assert_ne!(a.state, a.pkce.verifier);
    }

    #[test]
    fn the_redirect_uri_is_a_literal_loopback_address() {
        // RFC 8252 §7.3: `localhost` may resolve to ::1 and miss the listener, and
        // some IdPs reject a hostname redirect for a native client.
        let a = AuthAttempt::new(&mut Counter(1), 49_712);
        assert_eq!(a.redirect_uri, "http://127.0.0.1:49712/callback");
        assert!(!a.redirect_uri.contains("localhost"));
    }

    #[test]
    fn the_authorize_url_carries_every_required_parameter_encoded() {
        let a = AuthAttempt::new(&mut Counter(3), 49_712);
        let url = authorize_url(&config(), &a);
        let (base, query) = url.split_once('?').expect("a query is appended");
        assert_eq!(base, "https://idp.example.com/o/oauth2/v2/auth");

        let params: std::collections::BTreeMap<&str, String> = query
            .split('&')
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap();
                (k, percent_decode(v))
            })
            .collect();
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["client_id"], "sentinel-desktop");
        assert_eq!(params["redirect_uri"], "http://127.0.0.1:49712/callback");
        assert_eq!(params["scope"], "openid email profile");
        assert_eq!(params["code_challenge"], a.pkce.challenge);
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["state"], a.state);
        assert_eq!(params["nonce"], a.nonce);
        assert_eq!(params["tenantId"], "bpo-alpha");

        // The verifier is the secret half; it must never leave the machine here.
        assert!(!url.contains(&a.pkce.verifier));
        // Reserved characters in the redirect must be escaped, not passed through.
        assert!(query.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A49712%2Fcallback"));
        assert!(query.contains("scope=openid%20email%20profile"), "space is %20, not +");
    }

    #[test]
    fn the_tenant_is_omitted_when_the_customer_has_no_idp_tenant() {
        let mut cfg = config();
        cfg.tenant_id = None;
        let url = authorize_url(&cfg, &AuthAttempt::new(&mut Counter(1), 1));
        assert!(!url.contains("tenantId"));
    }

    #[test]
    fn an_endpoint_that_already_has_a_query_gets_an_ampersand() {
        let mut cfg = config();
        cfg.authorize_endpoint = "https://idp.example.com/auth?hd=bpo.example".into();
        let url = authorize_url(&cfg, &AuthAttempt::new(&mut Counter(1), 1));
        assert!(url.starts_with("https://idp.example.com/auth?hd=bpo.example&response_type=code"));
    }

    #[test]
    fn a_successful_callback_parses() {
        let cb = parse_callback("/callback?code=4%2F0AY0e&state=abc123&scope=openid").unwrap();
        assert_eq!(cb.code, "4/0AY0e", "percent escapes in the code are decoded");
        assert_eq!(cb.state, "abc123");
    }

    #[test]
    fn a_callback_on_another_path_is_rejected() {
        // The browser also requests /favicon.ico against the loopback listener; that
        // must not be mistaken for the callback and must not tear the listener down.
        assert_eq!(
            parse_callback("/favicon.ico"),
            Err(CallbackError::WrongPath("/favicon.ico".into()))
        );
        assert!(matches!(parse_callback("/?code=x&state=y"), Err(CallbackError::WrongPath(_))));
    }

    #[test]
    fn a_provider_error_is_reported_as_itself() {
        // "the user cancelled" and "the deployment is misconfigured" need different
        // messages in the widget, so they must not collapse into "no code".
        let e = parse_callback("/callback?error=access_denied&error_description=User%20cancelled&state=s")
            .unwrap_err();
        assert_eq!(
            e,
            CallbackError::Provider {
                code: "access_denied".into(),
                description: Some("User cancelled".into())
            }
        );
        let e = parse_callback("/callback?error=invalid_client").unwrap_err();
        assert!(matches!(e, CallbackError::Provider { .. }));
    }

    #[test]
    fn a_callback_missing_code_or_state_is_rejected() {
        assert_eq!(parse_callback("/callback?state=s"), Err(CallbackError::NoCode));
        assert_eq!(parse_callback("/callback?code=&state=s"), Err(CallbackError::NoCode));
        assert_eq!(parse_callback("/callback?code=c"), Err(CallbackError::NoState));
        assert_eq!(parse_callback("/callback"), Err(CallbackError::NoState));
    }

    #[test]
    fn a_forged_state_is_rejected() {
        // Anything on the machine can reach the loopback port. Without this check an
        // attacker could inject their own authorization code and have the agent
        // capture a shift's calls under their account.
        let attempt = AuthAttempt::new(&mut Counter(7), 49_712);
        let good = Callback { code: "c".into(), state: attempt.state.clone() };
        assert_eq!(validate(&attempt, &good).unwrap(), "c");

        for forged in ["", "wrong", &attempt.state[..attempt.state.len() - 1]] {
            let bad = Callback { code: "c".into(), state: forged.to_string() };
            assert_eq!(validate(&attempt, &bad), Err(CallbackError::StateMismatch));
        }
        // A state differing in one character, same length.
        let mut near = attempt.state.clone();
        let last = near.pop().unwrap();
        near.push(if last == 'A' { 'B' } else { 'A' });
        let bad = Callback { code: "c".into(), state: near };
        assert_eq!(validate(&attempt, &bad), Err(CallbackError::StateMismatch));
    }

    #[test]
    fn percent_coding_round_trips_including_the_awkward_bytes() {
        for s in ["", "plain", "a b", "4/0AY0e-Cg", "sc:ope#frag?q=1", "ünïcødé", "100%"] {
            assert_eq!(percent_decode(&percent_encode(s)), s, "round trip of {s:?}");
        }
        assert_eq!(percent_decode("a+b"), "a b", "browsers do emit + for space");
        assert_eq!(percent_decode("100%"), "100%", "a trailing stray % is kept, not eaten");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
