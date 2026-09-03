//! The SYSTEM-side half of the Sentinel endpoint client.
//!
//! Responsibilities (spec 6.1): pull tenant config, stage and apply updates, host the
//! named pipe, watchdog the agent, ship crash dumps.
//!
//! **It MUST NOT touch audio.** A Windows service runs in session 0, where WASAPI
//! cannot reach a user's audio session — that is the entire reason the client is two
//! processes rather than one. `tests/manifest.rs` asserts this crate does not even
//! depend on `sentinel-capture`, so the rule cannot be broken by accident.
//!
//! The library half exists so `sentinel-agent` can link the IPC codec and the device
//! identity: one definition of each, shared by both processes. The same reasoning puts
//! four more things here rather than in the agent:
//!
//! * [`devicekey`], [`csr`], [`der`] and [`enroll`] — the device credential. The
//!   certificate and its key are *machine* state: the service creates and renews them,
//!   the agent presents them over mTLS, and there must be exactly one definition of
//!   what is on disk and what a CSR looks like.
//! * [`spoolkey`] — the SQLCipher key, wrapped with DPAPI at **machine** scope
//!   precisely because these two processes are different principals.
//! * [`telemetry`] — OTLP export, parameterised by `service.name`, so both processes
//!   emit the same shape.
//! * [`http`] — the one blocking HTTP client the service needs.
//!
//! None of that is audio, and none of it links `sentinel-capture`.

pub mod config_sync;
pub mod crashdump;
pub mod csr;
pub mod der;
pub mod device;
pub mod devicekey;
pub mod enroll;
pub mod http;
pub mod ipc;
pub mod recovery;
pub mod spoolkey;
pub mod supervisor;
pub mod telemetry;
pub mod update;

#[cfg(windows)]
pub mod windows;

/// Build version, reported in heartbeats and used for update comparison.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The machine-scoped data directory: `%PROGRAMDATA%\MagickVoice\Sentinel`.
///
/// Holds the SQLCipher spool, the device credential, staged updates and crash dumps.
/// `client/installer/README.md` documents its ACL — `Users` get modify on the root
/// because the agent writes the spool, and read-only on `device\` because that is
/// machine identity.
pub fn data_dir() -> std::path::PathBuf {
    if cfg!(windows) {
        let base = std::env::var("PROGRAMDATA").unwrap_or_else(|_| "C:\\ProgramData".into());
        std::path::PathBuf::from(base).join("MagickVoice").join("Sentinel")
    } else {
        // Development only; the shipping service is Windows-only.
        std::path::PathBuf::from("/var/lib/magickvoice-sentinel")
    }
}
