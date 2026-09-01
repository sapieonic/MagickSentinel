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
//! identity: one definition of each, shared by both processes.

pub mod config_sync;
pub mod crashdump;
pub mod device;
pub mod ipc;
pub mod recovery;
pub mod supervisor;
pub mod update;

#[cfg(windows)]
pub mod windows;

/// Build version, reported in heartbeats and used for update comparison.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
