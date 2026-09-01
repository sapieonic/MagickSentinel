//! Agent side of the control pipe.
//!
//! The codec lives in `sentinel_service::ipc::codec` and both processes link it, so
//! the framing cannot drift between them. This is only the transport.

use sentinel_service::ipc::codec::{Request, Response};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum IpcClientError {
    #[error("the service is not running or the pipe is not reachable")]
    Unavailable,
    #[error("ipc error: {0}")]
    Io(String),
    #[error("the service replied with {code}: {message}")]
    Service { code: String, message: String },
    #[error("unexpected reply to this request")]
    Unexpected,
}

/// A connection to the service's pipe.
pub trait ServiceClient: Send + Sync {
    fn request(&self, req: Request) -> Result<Response, IpcClientError>;
}

/// Never reaches a service. Used off Windows and in `--headless`, where the agent
/// falls back to reading its own config.
pub struct NullServiceClient;

impl ServiceClient for NullServiceClient {
    fn request(&self, _req: Request) -> Result<Response, IpcClientError> {
        Err(IpcClientError::Unavailable)
    }
}

/// How long to wait for the service to answer. The service does no I/O to serve
/// `GetConfig`, so anything slower than this means it is wedged and the agent is
/// better off continuing on cached state than blocking its own loop.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(windows)]
pub use win::PipeServiceClient;

#[cfg(windows)]
mod win {
    use super::*;
    use sentinel_service::ipc::codec::{self, FrameReader, PIPE_NAME};
    use std::io::{Read, Write};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, ERROR_PIPE_BUSY};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FILE_SHARE_MODE, OPEN_EXISTING,
    };
    use windows::Win32::System::Pipes::WaitNamedPipeW;

    pub struct PipeServiceClient;

    struct Pipe(HANDLE);

    impl Drop for Pipe {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut n = 0u32;
            unsafe { ReadFile(self.0, Some(buf), Some(&mut n), None) }
                .map_err(std::io::Error::other)?;
            Ok(n as usize)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut n = 0u32;
            unsafe { WriteFile(self.0, Some(buf), Some(&mut n), None) }
                .map_err(std::io::Error::other)?;
            Ok(n as usize)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn open() -> Result<Pipe, IpcClientError> {
        let name: Vec<u16> = PIPE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        // The service serves a bounded number of instances. ERROR_PIPE_BUSY means all
        // of them are in use right now, not that the service is gone — WaitNamedPipeW
        // is the documented way to queue for the next one, and treating busy as
        // "unavailable" would make the agent fall back to cached config every time
        // two threads asked at once.
        for _ in 0..3 {
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
            };
            match handle {
                Ok(h) => return Ok(Pipe(h)),
                Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => unsafe {
                    let _ = WaitNamedPipeW(PCWSTR(name.as_ptr()), 2_000);
                },
                Err(_) => return Err(IpcClientError::Unavailable),
            }
        }
        Err(IpcClientError::Unavailable)
    }

    impl ServiceClient for PipeServiceClient {
        fn request(&self, req: Request) -> Result<Response, IpcClientError> {
            let mut pipe = open()?;
            let frame = codec::encode(&req).map_err(|e| IpcClientError::Io(e.to_string()))?;
            pipe.write_all(&frame).map_err(|e| IpcClientError::Io(e.to_string()))?;

            let mut reader = FrameReader::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = pipe.read(&mut chunk).map_err(|e| IpcClientError::Io(e.to_string()))?;
                if n == 0 {
                    return Err(IpcClientError::Unavailable);
                }
                reader.feed(&chunk[..n]);
                if let Some(body) =
                    reader.next_frame().map_err(|e| IpcClientError::Io(e.to_string()))?
                {
                    let response: Response =
                        codec::decode(&body).map_err(|e| IpcClientError::Io(e.to_string()))?;
                    return match response {
                        Response::Error { code, message } => {
                            Err(IpcClientError::Service { code, message })
                        }
                        other => Ok(other),
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_null_client_reports_the_service_as_unavailable() {
        // Off Windows and in --headless there is no service; the agent must fall back
        // to its own config rather than refusing to start.
        let e = NullServiceClient.request(Request::GetConfig).unwrap_err();
        assert!(matches!(e, IpcClientError::Unavailable));
    }

    #[test]
    fn the_pipe_name_is_the_one_the_service_hosts() {
        assert_eq!(
            sentinel_service::ipc::codec::PIPE_NAME,
            r"\\.\pipe\magickvoice-sentinel-v1"
        );
    }
}
