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
    // Configuration first, because whether telemetry is exported and where to is part
    // of it, and a `tracing` subscriber can only be installed once. The handle owns the
    // exporter thread; holding it in `main` keeps it alive for the life of the process.
    let local = sentinel_service::config_sync::ConfigStore::open(&sentinel_service::data_dir())
        .local()
        .clone();
    let _telemetry = init_logging(&local);

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
        Ok(())
    }

    #[cfg(not(windows))]
    {
        // The shipping service is Windows-only. This branch exists so the workspace
        // builds and tests on the Linux CI machine.
        anyhow::bail!("SentinelService is a Windows service; run with --console for a dry run");
    }
}

/// Install logging, and OTLP export if this tenant turned it on.
///
/// Telemetry is off unless configured, and when it is on it goes through the gateway
/// rather than to a collector — see `sentinel_service::telemetry` for why that is a
/// security decision and not a routing preference.
fn init_logging(
    local: &sentinel_core::config::LocalConfig,
) -> Option<sentinel_service::telemetry::TelemetryHandle> {
    use sentinel_service::telemetry;
    use tracing_subscriber::prelude::*;

    // Structured JSON, no PII (spec 12.10 and 15). Nothing that reaches a log line in
    // this process holds transcript text, an account reference or a borrower name;
    // the IPC types are shaped so it cannot.
    let filter = tracing_subscriber::EnvFilter::try_from_env("SENTINEL_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt::layer().json().with_current_span(false);

    let Some(url) = local.otlp_logs_url() else {
        let _ = tracing_subscriber::registry().with(filter).with(fmt).try_init();
        return None;
    };
    let via_gateway = local.otlp_goes_via_gateway(&url);
    let (layer, handle) = telemetry::OtlpLayer::new(
        telemetry::Resource {
            service_name: "sentinel-service".into(),
            service_version: sentinel_service::VERSION.into(),
            tenant_id: local.tenant_hint.clone(),
            device_id: local.device_id.clone(),
        },
        Box::new(sentinel_service::http::HttpOtlpShipper::new(url.clone())),
    );
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt)
        .with(layer)
        .try_init();
    if via_gateway {
        tracing::info!(endpoint = %url, "telemetry export enabled, relayed by the gateway");
    } else {
        tracing::warn!(
            endpoint = %url,
            "telemetry is being sent DIRECTLY to a collector rather than through the \
             gateway. This opens a second egress from this endpoint and is intended \
             for development only."
        );
    }
    Some(handle)
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
    use sentinel_service::enroll::{self, EnsureOutcome};
    use sentinel_service::ipc::{ConfigSnapshot, Request, Response, UpdateStatus};
    use sentinel_service::supervisor::{Action, Supervisor};
    use sentinel_service::update;
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
        staged: Mutex<Option<String>>,
        /// The agent's last reported capture state, from `ReportHealth`.
        agent_state: Mutex<String>,
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
                staged: Mutex::new(None),
                agent_state: Mutex::new("BLOCKED".into()),
            }
        }

        pub fn set_restarts(&self, n: u32) {
            self.restarts.store(n, Ordering::Relaxed);
        }

        pub fn take_update_request(&self) -> bool {
            self.update_requested.swap(false, Ordering::Relaxed)
        }

        /// The newest version staged on disk and verified against its manifest hash.
        pub fn staged_version(&self) -> Option<String> {
            self.staged.lock().unwrap().clone()
        }

        /// Whether the agent last reported itself mid-call. An update that lands here
        /// waits.
        pub fn agent_in_call(&self) -> bool {
            *self.agent_state.lock().unwrap() == "IN_CALL"
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
        // Server 2022 is build 20348; the Windows 11 client floor is 22000. The two
        // thresholds differ and conflating them is the mistake `sentinel-capture`'s
        // tier module exists to warn about — 20348 is a *server* build number and
        // using it as a client threshold would report every Windows 10 desktop as
        // tier A, then silently fail to arm on all of them.
        let floor = if is_server { 20348 } else { 22000 };
        let tier = if build >= floor {
            Some("A")
        } else if !is_server && build >= 18362 {
            Some("B")
        } else {
            None
        };
        (tier.map(str::to_string), os_build)
    }

    fn data_dir() -> std::path::PathBuf {
        sentinel_service::data_dir()
    }

    /// How often the device credential is re-checked once the service is up.
    ///
    /// Six hours. The renewal window is thirty days wide, so nothing here is urgent;
    /// what this interval is really for is the machine that was installed without a
    /// token and had one dropped in later, and the one whose first enrollment attempt
    /// hit a gateway answering `503 no_ca`. Both should recover within a shift without
    /// anybody restarting a service.
    const CREDENTIAL_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);

    /// Enroll this machine, or renew its certificate, or explain why neither happened.
    ///
    /// Runs as LocalSystem, which is what it takes to create a **machine** CNG key and
    /// to write `device\`. The agent deliberately cannot do this: it runs as the
    /// signed-in user, it has read-only access to that directory, and a process that
    /// could mint machine identity on demand is a process a revocation cannot stop.
    ///
    /// Everything about the decision lives in `enroll::ensure_enrolled`, which is
    /// platform-neutral and tested. What is here is sourcing the machine facts and the
    /// token, both of which are registry reads, and clearing the token afterwards.
    fn ensure_device_credential(state: &ServiceState) {
        use sentinel_service::windows::machine;

        let dir = sentinel_service::device::credential_dir();
        let (tier, os_build) = {
            (
                state.tier.lock().unwrap().clone(),
                state.os_build.lock().unwrap().clone(),
            )
        };
        let facts = machine::machine_facts(tier, os_build);

        // The key is opened — or created, on the very first run — before the token is
        // read, so a machine whose KSP refuses fails without spending one.
        let key = match sentinel_service::devicekey::open_or_create(&dir) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(
                    target: sentinel_service::telemetry::TARGET,
                    event = "device_key.unavailable",
                    error = %e,
                    "no device key: this machine cannot enroll and cannot present a \
                     client certificate, so capture will spool and never upload"
                );
                return;
            }
        };

        // The renewal token file takes precedence over the installer's registry value:
        // on a machine that is already enrolled the registry value is a leftover from
        // the original install and is certainly spent, while the file is what an
        // operator just put there.
        let renewal = enroll::read_renewal_token(&dir);
        let token = renewal.clone().or_else(machine::enrollment_token);

        let api_base = state.config.lock().unwrap().local().api_base_url.clone();
        let now_ms = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
        let outcome = enroll::ensure_enrolled(
            &dir,
            &api_base,
            &facts,
            now_ms,
            token.as_deref(),
            key.as_ref(),
            &sentinel_service::http::HttpEnrollTransport::new(),
            sentinel_service::spoolkey::wrapper().as_ref(),
        );

        match &outcome {
            EnsureOutcome::Enrolled { device_id, not_after, renewed, key_kind } => {
                tracing::info!(
                    target: sentinel_service::telemetry::TARGET,
                    event = "device.enrolled",
                    device_id = %device_id,
                    not_after = %not_after,
                    renewed,
                    key_kind = key_kind.as_str(),
                    meets_non_exportable_requirement =
                        key_kind.meets_non_exportable_requirement(),
                    "device credential is current"
                );
                // Only now. A token cleared before the exchange succeeded would leave a
                // machine that failed transiently with no way to retry.
                if renewal.is_some() {
                    if let Err(e) = enroll::clear_renewal_token(&dir) {
                        tracing::warn!(error = %e, "the renewal token could not be removed");
                    }
                } else {
                    machine::clear_enrollment_token();
                }
            }
            EnsureOutcome::AlreadyEnrolled { device_id, not_after } => {
                tracing::debug!(device_id = %device_id, not_after = %not_after, "device credential is current");
            }
            EnsureOutcome::NotEnrolledNoToken => {
                tracing::error!(
                    target: sentinel_service::telemetry::TARGET,
                    event = "device.not_enrolled",
                    "this machine has no device certificate and no enrollment token. \
                     Capture will spool locally and never upload until it is enrolled: \
                     re-run the MSI with ENROLLMENTTOKEN, or drop a token into the \
                     credential directory."
                );
            }
            EnsureOutcome::RenewalBlockedNoToken { .. } | EnsureOutcome::Failed(_) => {
                // Both are already reported, with their reasons, inside
                // `ensure_enrolled`.
            }
        }
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
                    Response::Config(Box::new(ConfigSnapshot {
                        local: cfg.local().clone(),
                        policy: cfg.policy().cloned(),
                        capture_tier: self.tier.lock().unwrap().clone(),
                        os_build: self.os_build.lock().unwrap().clone(),
                        service_version: sentinel_service::VERSION.into(),
                        agent_restarts: self.restarts.load(Ordering::Relaxed),
                    }))
                }
                Request::ReportHealth(h) => {
                    *self.agent_state.lock().unwrap() = h.capture_state.clone();
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
                        staged_version: self.staged_version(),
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

        // Before the watchdog launches anything: an agent started against a machine
        // with no credential spools audio it can never upload, and the sooner the
        // reason for that is in the log the better.
        ensure_device_credential(&state);
        let mut last_credential_check = Instant::now();

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

            // An agent that asked for an update check gets one on the next pass.
            // Applying it is deferred while a call is in progress: cutting a live
            // recording is data loss in a compliance product, and no fix is worth a
            // hole in the evidence.
            if state.take_update_request() {
                let staged = state.staged_version();
                match update::decide(
                    sentinel_service::VERSION,
                    staged.as_deref(),
                    state.agent_in_call(),
                    false,
                ) {
                    update::ApplyDecision::Apply => {
                        tracing::info!(version = ?staged, "staged update ready to apply");
                    }
                    update::ApplyDecision::DeferInCall => {
                        tracing::info!("update deferred: the agent is on a call");
                    }
                    update::ApplyDecision::Nothing => {
                        tracing::debug!("update check found nothing newer");
                    }
                }
            }

            if last_credential_check.elapsed() >= CREDENTIAL_CHECK_INTERVAL {
                last_credential_check = Instant::now();
                ensure_device_credential(&state);
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
