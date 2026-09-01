//! The one-shot loopback listener the browser redirects back to (spec 7.3 step 3–5).
//!
//! Plain `std::net`, so it behaves identically on the CI machine and on the endpoint.
//!
//! "One-shot" is a security property, not a simplification: the listener exists only
//! for the seconds between opening the browser and receiving the code, and is dropped
//! the instant a callback validates. A loopback port left open is a port anything else
//! on the machine — including whatever the agent is meant to be watching — can post an
//! authorization code to.

use super::pkce::{parse_callback, Callback, CallbackError};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// How long to wait for the user to finish signing in before giving up. Corporate SSO
/// with a hardware key is not fast; two minutes would time out honest users.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest request line we will read. An authorization code plus state is a few
/// hundred bytes; anything larger is not a browser.
const MAX_REQUEST_LINE: u64 = 8 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LoopbackError {
    #[error("no callback arrived within the sign-in window")]
    TimedOut,
    #[error(transparent)]
    Callback(#[from] CallbackError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A bound, not-yet-accepted loopback listener.
pub struct LoopbackListener {
    listener: TcpListener,
    port: u16,
}

impl LoopbackListener {
    /// Bind an ephemeral port on `127.0.0.1`.
    ///
    /// Explicitly the loopback address, not `0.0.0.0`: binding the wildcard would put
    /// the callback endpoint on the floor's LAN, where any other machine could post a
    /// code to it.
    pub fn bind() -> std::io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let port = listener.local_addr()?.port();
        Ok(LoopbackListener { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Wait for the callback, then shut the listener down.
    ///
    /// Consumes `self`: there is no way to accidentally keep serving. Requests for
    /// anything other than `/callback` — the browser's automatic `/favicon.ico`, most
    /// often — are answered with 404 and do not end the wait.
    pub fn wait_for_callback(self, timeout: Duration) -> Result<Callback, LoopbackError> {
        let deadline = Instant::now() + timeout;
        self.listener.set_nonblocking(true)?;

        loop {
            if Instant::now() >= deadline {
                return Err(LoopbackError::TimedOut);
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    match Self::serve_one(stream) {
                        Ok(cb) => return Ok(cb),
                        Err(LoopbackError::Callback(CallbackError::WrongPath(_))) => continue,
                        Err(e) => return Err(e),
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn serve_one(mut stream: TcpStream) -> Result<Callback, LoopbackError> {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut line = String::new();
        BufReader::new(stream.try_clone()?)
            .take(MAX_REQUEST_LINE)
            .read_line(&mut line)?;

        let target = request_target(&line).ok_or(CallbackError::Malformed)?;
        let result = parse_callback(target);

        let (status, body) = match &result {
            Ok(_) => ("200 OK", SUCCESS_PAGE),
            Err(CallbackError::WrongPath(_)) => ("404 Not Found", ""),
            Err(_) => ("400 Bad Request", FAILURE_PAGE),
        };
        // Close the connection explicitly: a keep-alive here would leave the browser
        // holding a socket to a listener we are about to drop.
        let response = format!(
            "HTTP/1.1 {status}\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Cache-Control: no-store\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();

        Ok(result?)
    }
}

/// Extract the request target from an HTTP request line.
///
/// `GET /callback?code=… HTTP/1.1` → `/callback?code=…`. Only `GET` is accepted: the
/// authorization response is a redirect, and anything POSTing here is not the browser
/// we sent out.
pub fn request_target(line: &str) -> Option<&str> {
    let mut parts = line.trim_end_matches(['\r', '\n']).split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if method != "GET" || !version.starts_with("HTTP/") || target.is_empty() {
        return None;
    }
    Some(target)
}

const SUCCESS_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Signed in</title>\
<body style=\"font:16px system-ui;padding:3rem;text-align:center\">\
<p>Signed in to MagickVoice Sentinel.</p><p>You can close this tab.</p>";

const FAILURE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Sign-in failed</title>\
<body style=\"font:16px system-ui;padding:3rem;text-align:center\">\
<p>Sign-in did not complete.</p><p>Close this tab and try again from the widget.</p>";

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn request_targets_are_parsed_and_non_get_is_refused() {
        assert_eq!(request_target("GET /callback?code=a HTTP/1.1\r\n"), Some("/callback?code=a"));
        assert_eq!(request_target("GET / HTTP/1.0"), Some("/"));
        assert_eq!(request_target("POST /callback HTTP/1.1"), None);
        assert_eq!(request_target("GET /callback"), None, "no version");
        assert_eq!(request_target(""), None);
        assert_eq!(request_target("garbage"), None);
    }

    /// Send one HTTP request to the listener and return its response.
    fn get(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .unwrap();
        let mut buf = String::new();
        let _ = s.read_to_string(&mut buf);
        buf
    }

    #[test]
    fn the_callback_is_received_and_the_port_closes_immediately_after() {
        let l = LoopbackListener::bind().unwrap();
        let port = l.port();
        let client = std::thread::spawn(move || get(port, "/callback?code=abc&state=xyz"));

        let cb = l.wait_for_callback(Duration::from_secs(5)).unwrap();
        assert_eq!(cb.code, "abc");
        assert_eq!(cb.state, "xyz");
        assert!(client.join().unwrap().starts_with("HTTP/1.1 200 OK"));

        // A loopback port left open is a port anything on the machine can post an
        // authorization code to, so `wait_for_callback` consuming the listener has to
        // actually free it.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            TcpStream::connect(("127.0.0.1", port)).is_err(),
            "the listener must be gone once the code is in hand"
        );
    }

    #[test]
    fn a_favicon_request_does_not_end_the_wait() {
        // Browsers request /favicon.ico against the loopback origin unprompted. An
        // implementation that accepts once and gives up loses the real callback.
        let l = LoopbackListener::bind().unwrap();
        let port = l.port();
        std::thread::spawn(move || {
            assert!(get(port, "/favicon.ico").starts_with("HTTP/1.1 404"));
            get(port, "/callback?code=real&state=s");
        });
        let cb = l.wait_for_callback(Duration::from_secs(5)).unwrap();
        assert_eq!(cb.code, "real");
    }

    #[test]
    fn a_provider_error_reaches_the_caller_and_the_browser_is_told() {
        let l = LoopbackListener::bind().unwrap();
        let port = l.port();
        let client =
            std::thread::spawn(move || get(port, "/callback?error=access_denied&state=s"));
        let err = l.wait_for_callback(Duration::from_secs(5)).unwrap_err();
        assert!(
            matches!(err, LoopbackError::Callback(CallbackError::Provider { ref code, .. }) if code == "access_denied"),
            "got {err:?}"
        );
        assert!(client.join().unwrap().starts_with("HTTP/1.1 400"));
    }

    #[test]
    fn the_wait_times_out_rather_than_hanging_forever() {
        let l = LoopbackListener::bind().unwrap();
        let port = l.port();
        let started = Instant::now();
        let err = l.wait_for_callback(Duration::from_millis(150)).unwrap_err();
        assert!(matches!(err, LoopbackError::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "the port is released");
    }

    #[test]
    fn the_listener_is_bound_to_loopback_only() {
        // Binding the wildcard would expose the callback endpoint to the floor's LAN.
        let l = LoopbackListener::bind().unwrap();
        assert_eq!(l.listener.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(l.port(), 0, "an ephemeral port is resolved before the browser opens");
    }
}
