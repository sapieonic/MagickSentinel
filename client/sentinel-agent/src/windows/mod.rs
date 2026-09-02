//! Windows-only parts of the agent.
//!
//! Everything the pipeline, the uplink and the auth flow can decide without a Windows
//! API lives outside this module, so it is tested on the Linux CI machine. What
//! remains here genuinely needs the platform.

pub mod capture_source;
