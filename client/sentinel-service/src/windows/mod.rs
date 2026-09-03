//! Windows-only halves of the service: the SCM contract, the named-pipe host, and
//! launching the agent into an interactive session.
//!
//! Everything that can be decided without a Windows API call lives outside this
//! module so it can be tested on any platform. What is left here is genuinely
//! platform work.

pub mod launcher;
pub mod machine;
pub mod pipe;
pub mod scm;
