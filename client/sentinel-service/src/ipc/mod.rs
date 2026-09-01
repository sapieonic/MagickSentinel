//! IPC between the SYSTEM service and the user-session agent.
//!
//! [`codec`] is the shared definition — both processes link it, so the wire format
//! cannot drift between them. [`crate::windows::pipe`] is the service-side transport.

pub mod codec;

pub use codec::{
    decode, encode, ConfigSnapshot, FrameReader, HealthReport, IpcError, Request, Response,
    UpdateStatus, MAX_FRAME_BYTES, PIPE_NAME, PIPE_SDDL,
};
