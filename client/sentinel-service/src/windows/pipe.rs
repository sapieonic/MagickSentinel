//! Named-pipe host for `\\.\pipe\magickvoice-sentinel-v1` (spec 6.1).
//!
//! One thread per connection, byte-mode, synchronous. The agent's traffic is a
//! handful of small messages a minute, so an IOCP design here would be complexity
//! with nothing to show for it.
//!
//! Two Windows details that are easy to get wrong and silent when you do:
//!
//! * **The security descriptor must be attached to the *first* instance.** Every
//!   later instance of a named pipe inherits the first one's ACL, so creating the
//!   first instance with a null `lpSecurityAttributes` gives the pipe the default
//!   descriptor for the LocalSystem token — which denies `BUILTIN\Users` — and every
//!   subsequent `CreateNamedPipeW` that *does* pass a descriptor is ignored. The
//!   agent then gets `ERROR_ACCESS_DENIED` and nothing in the service looks wrong.
//! * **`PIPE_REJECT_REMOTE_CLIENTS`.** Without it the pipe is reachable over SMB from
//!   another machine by anyone in `BUILTIN\Users` of *that* machine's domain. This is
//!   a local IPC channel; remote clients have no business on it.

use crate::ipc::codec::{self, FrameReader, Request, Response, MAX_FRAME_BYTES, PIPE_NAME, PIPE_SDDL};
use std::io::{Read, Write};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, ERROR_BROKEN_PIPE, ERROR_PIPE_CONNECTED, LocalFree, HLOCAL,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// Backing buffer for one pipe instance, in bytes.
const PIPE_BUFFER: u32 = 64 * 1024;

/// Concurrent instances. One per interactive session plus headroom for a session
/// change that overlaps a still-draining connection.
const MAX_INSTANCES: u32 = 8;

/// Handles one request and produces the reply. Implemented by the service body.
pub trait RequestHandler: Send + Sync + 'static {
    fn handle(&self, req: Request) -> Response;
}

/// Owns the SDDL-derived security descriptor for the pipe's lifetime.
///
/// The descriptor must outlive every `CreateNamedPipeW` call that references it, and
/// `LocalFree` is the matching deallocator for what
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW` returns.
struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
}

impl PipeSecurity {
    fn from_sddl(sddl: &str) -> windows::core::Result<Self> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )?;
        }
        Ok(PipeSecurity { descriptor })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.descriptor.is_invalid() {
            unsafe {
                let _ = LocalFree(HLOCAL(self.descriptor.0));
            }
        }
    }
}

/// A connected pipe instance, closed on drop.
struct PipeInstance(HANDLE);

// `HANDLE` is a raw pointer, so it is not `Send` by default, but a kernel handle is
// process-wide and safe to use from any thread. The invariant that makes this sound
// is ownership: exactly one `PipeInstance` exists per handle and it is moved — never
// shared — into the thread that services the connection.
unsafe impl Send for PipeInstance {}

impl Drop for PipeInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = DisconnectNamedPipe(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

impl Read for PipeInstance {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut read = 0u32;
        let r = unsafe { ReadFile(self.0, Some(buf), Some(&mut read), None) };
        match r {
            Ok(()) => Ok(read as usize),
            // The agent exiting closes its end; that is an ordinary end of stream,
            // not a failure worth logging at error level on every shift change.
            Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(0),
            Err(e) => Err(std::io::Error::other(e)),
        }
    }
}

impl Write for PipeInstance {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut written = 0u32;
        unsafe { WriteFile(self.0, Some(buf), Some(&mut written), None) }
            .map_err(std::io::Error::other)?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Accept connections until `stop` is set, dispatching each to `handler`.
pub fn serve<H: RequestHandler>(
    handler: std::sync::Arc<H>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> windows::core::Result<()> {
    let security = PipeSecurity::from_sddl(PIPE_SDDL)?;
    let name: Vec<u16> = PIPE_NAME.encode_utf16().chain(std::iter::once(0)).collect();

    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let attrs = security.attributes();
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                // Byte mode both ways: the length prefix in `ipc::codec` is what
                // delimits messages, so message mode would only add a second,
                // redundant framing that can disagree with the first.
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                MAX_INSTANCES,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                Some(&attrs),
            )
        };
        if handle.is_invalid() {
            return Err(windows::core::Error::from_win32());
        }
        let instance = PipeInstance(handle);

        // ERROR_PIPE_CONNECTED means the client connected in the window between
        // CreateNamedPipeW and ConnectNamedPipe. It is a success, not a failure;
        // treating it as an error drops that client's connection.
        match unsafe { ConnectNamedPipe(handle, None) } {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
            Err(e) => {
                tracing::warn!(error = %e, "pipe connect failed");
                continue;
            }
        }

        let handler = handler.clone();
        std::thread::spawn(move || {
            if let Err(e) = pump(instance, handler.as_ref()) {
                tracing::debug!(error = %e, "pipe connection ended");
            }
        });
    }
    Ok(())
}

/// Read requests and write replies until the peer goes away.
fn pump<H: RequestHandler>(mut pipe: PipeInstance, handler: &H) -> std::io::Result<()> {
    let mut reader = FrameReader::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = pipe.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        reader.feed(&chunk[..n]);
        loop {
            match reader.next_frame() {
                Ok(None) => break,
                Ok(Some(body)) => {
                    let reply = match codec::decode::<Request>(&body) {
                        Ok(req) => handler.handle(req),
                        Err(e) => Response::Error {
                            code: "bad_message".into(),
                            message: e.to_string(),
                        },
                    };
                    let bytes = codec::encode(&reply).map_err(std::io::Error::other)?;
                    pipe.write_all(&bytes)?;
                }
                Err(e) => {
                    // A malformed length prefix means the stream is no longer
                    // synchronised; there is no safe point to resume from.
                    tracing::warn!(error = %e, "framing error on the control pipe");
                    return Ok(());
                }
            }
        }
        if reader.buffered() > MAX_FRAME_BYTES {
            tracing::warn!("peer buffered more than one maximum frame without completing it");
            return Ok(());
        }
    }
}
