//! Core logic for the Sentinel endpoint agent.
//!
//! Everything in this crate is platform-neutral and unit-testable: the call state
//! machine, the encrypted spool, the wire protocol codec, the uplink's retry policy,
//! and the configuration types delivered by `GET /v1/policy`. The Windows-specific
//! audio work lives in `sentinel-capture`.

pub mod backoff;
pub mod config;
pub mod events;
pub mod protocol;
pub mod spool;
pub mod state;

pub use config::{Policy, VadConfig};
pub use events::{ClientEvent, EventKind};
pub use protocol::{Channel, ControlMessage, MediaRecord, MediaFlags, ProtocolError};
pub use spool::{Spool, SpoolStats, SegmentRow};
pub use state::{CallState, Detector, DetectorInput, Transition};
