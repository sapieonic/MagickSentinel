//! The user-session half of the Sentinel endpoint client.
//!
//! It does capture and UI, because a SYSTEM service cannot: WASAPI is audio-session
//! scoped and services run in session 0. `sentinel-service` handles updates, config,
//! the watchdog and crash reporting.
//!
//! The pieces, in the order audio moves through them:
//!
//! | Module | Job |
//! |---|---|
//! | [`auth`] | RFC 8252 PKCE sign-in, token refresh, Credential Manager |
//! | [`agent`] | the orchestration loop that ticks all of the below in one order |
//! | [`identity`] | both-identities gate and the offline-grace clock |
//! | [`pipeline`] | pinned device → VAD → detector → Opus → spool |
//! | [`encode`] | 16 kHz / 24 kbps / 20 ms frames, 50 to a segment |
//! | [`uplink`] | WSS + mTLS, resume, ack-driven spool deletion |
//! | [`heartbeat`] | the 30 s tamper and health signal |
//! | [`widget`] | the WebView2 shell and its `sentinel.*` surface |
//! | [`api`] | the REST routes the agent needs |
//! | [`ipc`] | the agent side of the service's control pipe |
//!
//! Everything above is platform-neutral except the Credential Manager, the browser
//! launch, the widget window and the pipe client, each of which is `cfg(windows)` with
//! a headless counterpart so the whole pipeline runs in CI on Linux against
//! `WavReplaySource`.

pub mod agent;
pub mod api;
pub mod auth;
pub mod encode;
pub mod heartbeat;
pub mod identity;
pub mod instance;
pub mod ipc;
pub mod pipeline;
pub mod uplink;
pub mod widget;

#[cfg(windows)]
pub mod windows;

/// Build version, reported in every heartbeat.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Initialise structured logging.
///
/// JSON, no PII (spec 12.10, 15). Nothing in this crate logs transcript text, an
/// account reference or a borrower name; where a value could carry one — a gateway
/// error message, a UIA scrape — it is dropped rather than logged.
pub fn init_logging() {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_env("SENTINEL_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json().with_current_span(false))
        .try_init();
}
