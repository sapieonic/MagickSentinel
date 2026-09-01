//! `SentinelService.exe` — the SYSTEM service (spec 6.1).
//!
//! Started by the SCM as `Automatic (Delayed Start)` under LocalSystem. Delayed start
//! matters: the service's first act is to detect the capture tier and reach the API,
//! and doing that while Windows is still bringing up the network stack produces a
//! spurious "offline" on every boot.
//!
//! Run with `--console` to host the same body outside the SCM for debugging. That
//! path cannot launch an agent — `WTSQueryUserToken` needs `SE_TCB_NAME`, which an
//! interactive administrator does not have — but it does exercise the pipe host.

fn main() -> anyhow::Result<()> {
    init_logging();

    if std::env::args().any(|a| a == "--console") {
        let (_tx, rx) = std::sync::mpsc::channel();
        service_body_console(rx);
        return Ok(());
    }

    #[cfg(windows)]
    {
        // Re-assert recovery actions on every start: an MSI repair or an in-place
        // upgrade can drop them, and a watchdog that does not restart is not there.
        if let Err(e) = sentinel_service::recovery::apply_recovery_actions() {
            tracing::warn!(error = %e, "could not apply service recovery actions");
        }
        sentinel_service::windows::scm::run(win::service_body)?;
        return Ok(());
    }

    #[cfg(not(windows))]
    {
        // The shipping service is Windows-only. This branch exists so the workspace
        // builds and tests on the Linux CI machine.
        anyhow::bail!("SentinelService is a Windows service; run with --console for a dry run");
    }
}

fn init_logging() {
    use tracing_subscriber::prelude::*;
    // Structured JSON, no PII (spec 12.10 and 15). Nothing that reaches a log line in
    // this process holds transcript text, an account reference or a borrower name;
    // the IPC types are shaped so it cannot.
    let filter = tracing_subscriber::EnvFilter::try_from_env("SENTINEL_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json().with_current_span(false))
        .try_init();
}

/// The `--console` body: pipe host only, no session launching.
fn service_body_console(_rx: std::sync::mpsc::Receiver<()>) {
    tracing::info!(version = sentinel_service::VERSION, "sentinel service starting (console)");
    #[cfg(windows)]
    {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state = std::sync::Arc::new(win::ServiceState::new());
        if let Err(e) = sentinel_service::windows::pipe::serve(state, stop) {
            tracing::error!(error = %e, "pipe host stopped");
        }
    }
    #[cfg(not(windows))]
    tracing::info!("nothing to do off Windows");
}

#[cfg(windows)]
mod win {
    use sentinel_service::config_sync::ConfigStore;
    use sentinel_service::ipc::{ConfigSnapshot, Request, Response, UpdateStatus};
    use sentinel_service::supervisor::{Action, Supervisor};
    use sentinel_service::windows::launcher::{launch_agent, AgentProcess};
    use sentinel_service::windows::pipe::{self, RequestHandler};
    use sentinel_service::windows::scm::ServiceEvent;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Shared state the pipe handler serves from.
    pub struct ServiceState {
        config: Mutex<ConfigStore>,
        tier: Mutex<Option<String>>,
        os_build: Mutex<String>,
        restarts: AtomicU32,
        update_requested: AtomicBool,
    }

    impl ServiceState {
        pub fn new() -> Self {
            let dir = data_dir();
            let detection = sentinel_capture_tier();
            ServiceState {
                config: Mutex::new(ConfigStore::open(&dir)),
                tier: Mutex::new(detection.0),
                os_build: Mutex::new(detection.1),
                restarts: AtomicU32::new(0),
                update_requested: AtomicBool::new(false),
            }
        }

        pub fn set_restarts(&self, n: u32) {
            self.restarts.store(n, Ordering::Relaxed);
        }

        pub fn take_update_request(&self) -> bool {
            self.update_requested.swap(false, Ordering::Relaxed)
        }
    }

    /// Tier detection without depending on `sentinel-capture`.
    ///
    /// The service must not link the audio crate (see the crate docs), so it reads the
    /// build number from the registry itself and applies the same rule the capture
    /// crate's `tier::classify` documents: Windows 11 / Server 2022 and later are
    /// tier A, Windows 10 1903–22H2 are tier B, everything else is unsupported.
    /// `client/sentinel-capture/src/tier.rs` remains the authority; this is a
    /// deliberate, narrow duplication rather than a dependency edge that would put
    /// WASAPI code in session 0.
    fn sentinel_capture_tier() -> (Option<String>, String) {
        use windows::core::w;
        use windows::Win32::System::Registry::{
            RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
        };

        fn read(name: windows::core::PCWSTR) -> Option<String> {
            let mut buf = [0u16; 128];
            let mut size = (buf.len() * 2) as u32;
            unsafe {
                RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
                    name,
                    RRF_RT_REG_SZ,
                    None,
                    Some(buf.as_mut_ptr() as *mut _),
                    Some(&mut size),
                )
                .ok()
                .ok()?;
            }
            let chars = (size as usize / 2).saturating_sub(1);
            Some(String::from_utf16_lossy(&buf[..chars.min(buf.len())]))
        }

        let build: u32 = read(w!("CurrentBuildNumber"))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let is_server = read(w!("InstallationType")).as_deref() == Some("Server");
        let os_build = format!("10.0.{build}");
        let tier = if is_server && build >= 20348 {
            Some("A")
        } else if !is_server && build >= 22000 {
            Some("A")
        } else if !is_server && build >= 18362 {
            Some("B")
        } else {
            None
        };
        (tier.map(str::to_string), os_build)
    }

    fn data_dir() -> std::path::PathBuf {
        let base = std::env::var("PROGRAMDATA").unwrap_or_else(|_| "C:\\ProgramData".into());
        std::path::PathBuf::from(base).join("MagickVoice").join("Sentinel")
    }

    fn agent_exe() -> String {
        // Next to the service binary: both are installed into the same directory by
        // the MSI, and resolving via %PROGRAMFILES% would break a per-machine install
        // to a non-default location.
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("SentinelAgent.exe")))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "SentinelAgent.exe".into())
    }

    impl RequestHandler for ServiceState {
        fn handle(&self, req: Request) -> Response {
            match req {
                Request::GetConfig => {
                    let cfg = self.config.lock().unwrap();
                    Response::Config(ConfigSnapshot {
                        local: cfg.local().clone(),
                        policy: cfg.policy().cloned(),
                        capture_tier: self.tier.lock().unwrap().clone(),
                        os_build: self.os_build.lock().unwrap().clone(),
                        service_version: sentinel_service::VERSION.into(),
                        agent_restarts: self.restarts.load(Ordering::Relaxed),
                    })
                }
                Request::ReportHealth(h) => {
                    // Logged as machine state only; the type cannot carry an identity.
                    tracing::debug!(
                        capture_state = %h.capture_state,
                        spool_depth = h.spool_depth,
                        signed_in = h.user_signed_in,
                        "agent health"
                    );
                    Response::Ok
                }
                Request::RequestUpdateCheck => {
                    self.update_requested.store(true, Ordering::Relaxed);
                    Response::UpdateStatus(UpdateStatus {
                        current_version: sentinel_service::VERSION.into(),
                        staged_version: None,
                        checking: true,
                    })
                }
                Request::LogEvent(ev) => {
                    tracing::info!(kind = ?ev.kind, count = ?ev.count, "client event");
                    Response::Ok
                }
            }
        }
    }

    /// The service body, running on the SCM's `ServiceMain` thread.
    pub fn service_body(rx: Receiver<ServiceEvent>) {
        tracing::info!(version = sentinel_service::VERSION, "sentinel service running");
        let state = Arc::new(ServiceState::new());
        let stop = Arc::new(AtomicBool::new(false));

        let pipe_state = state.clone();
        let pipe_stop = stop.clone();
        std::thread::spawn(move || {
            if let Err(e) = pipe::serve(pipe_state, pipe_stop) {
                tracing::error!(error = %e, "pipe host stopped");
            }
        });

        let started = Instant::now();
        let now_ms = || started.elapsed().as_millis() as u64;
        let mut supervisor = Supervisor::new(now_ms());
        let mut processes: HashMap<u32, AgentProcess> = HashMap::new();
        let exe = agent_exe();

        loop {
            // Reap any agent that exited since the last pass.
            let dead: Vec<u32> = processes
                .iter()
                .filter(|(_, p)| p.has_exited())
                .map(|(&s, _)| s)
                .collect();
            for session in dead {
                processes.remove(&session);
                supervisor.on_exited(session, now_ms(), stop.load(Ordering::SeqCst));
                state.set_restarts(supervisor.restarts());
                tracing::warn!(session, restarts = supervisor.restarts(), "agent exited");
            }

            let wait = match supervisor.poll(now_ms()) {
                Action::Launch { session_id } => {
                    match launch_agent(session_id, &exe) {
                        Ok(p) => {
                            tracing::info!(session = session_id, pid = p.pid, "agent launched");
                            processes.insert(session_id, p);
                            supervisor.on_launched(session_id, now_ms());
                        }
                        Err(e) => {
                            // Feed the failure back through the same path a crash
                            // takes, so a session we cannot launch into backs off
                            // instead of spinning.
                            tracing::error!(session = session_id, error = %e, "agent launch failed");
                            supervisor.on_launched(session_id, now_ms());
                            supervisor.on_exited(session_id, now_ms(), false);
                        }
                    }
                    Duration::from_millis(200)
                }
                Action::Idle { next_deadline_ms } => next_deadline_ms
                    .map(|d| Duration::from_millis(d.saturating_sub(now_ms())))
                    .unwrap_or(Duration::from_secs(2))
                    .min(Duration::from_secs(2)),
            };

            match rx.recv_timeout(wait) {
                Ok(ServiceEvent::SessionLogon(s)) => {
                    tracing::info!(session = s, "session logon");
                    supervisor.on_logon(s, now_ms());
                }
                Ok(ServiceEvent::SessionLogoff(s)) => {
                    tracing::info!(session = s, "session logoff");
                    supervisor.on_logoff(s);
                    processes.remove(&s);
                }
                Ok(ServiceEvent::Stop) => break,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if sentinel_service::windows::scm::stopping() {
                break;
            }
        }

        stop.store(true, Ordering::SeqCst);
        tracing::info!("sentinel service stopping");
    }
}
