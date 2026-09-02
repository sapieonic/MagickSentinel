//! Service Control Manager plumbing.
//!
//! The service is `Automatic (Delayed Start)` and runs as LocalSystem (spec 6.1);
//! both are set by the installer, since the SCM start type cannot be chosen by the
//! process itself.
//!
//! The one thing this module exists for is `SERVICE_CONTROL_SESSIONCHANGE`. A service
//! receives session notifications only if it declares
//! `SERVICE_ACCEPT_SESSIONCHANGE` in its status *and* registers its handler with
//! `RegisterServiceCtrlHandlerExW` — the older `RegisterServiceCtrlHandlerW` has no
//! parameter through which the session id could arrive, so a service registered that
//! way is told a session changed but never which one.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Mutex, OnceLock};
use windows::core::PWSTR;
use windows::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR};
use windows::Win32::System::RemoteDesktop::WTSSESSION_NOTIFICATION;
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE,
    SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

/// `WTS_SESSION_LOGON` / `WTS_SESSION_LOGOFF`, which `windows-rs` 0.58 exposes only as
/// bare constants in the RemoteDesktop module; spelled out here so the match below
/// reads as the documentation does.
const WTS_SESSION_LOGON: u32 = 0x5;
const WTS_SESSION_LOGOFF: u32 = 0x6;

/// What the control handler reports to the service body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceEvent {
    SessionLogon(u32),
    SessionLogoff(u32),
    Stop,
}

struct Shared {
    status_handle: SERVICE_STATUS_HANDLE,
    tx: SyncSender<ServiceEvent>,
}

// The SCM calls our control handler on its own threads, so the status handle has to
// be reachable from them. It is an opaque SCM-owned token, valid process-wide for the
// life of the service and only ever passed back to `SetServiceStatus`, which is
// documented as safe to call from any thread.
unsafe impl Send for Shared {}

static SHARED: OnceLock<Mutex<Option<Shared>>> = OnceLock::new();
static CHECKPOINT: AtomicU32 = AtomicU32::new(0);
static STOPPING: AtomicBool = AtomicBool::new(false);

/// Set by `run` before handing control to the SCM.
static SERVICE_MAIN: OnceLock<fn(Receiver<ServiceEvent>)> = OnceLock::new();

fn shared() -> &'static Mutex<Option<Shared>> {
    SHARED.get_or_init(|| Mutex::new(None))
}

/// True once a stop or shutdown control has arrived.
pub fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}

fn set_status(state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE, accept: u32) {
    let guard = shared().lock().unwrap();
    let Some(s) = guard.as_ref() else { return };
    let pending = matches!(state, SERVICE_START_PENDING | SERVICE_STOP_PENDING);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accept,
        dwWin32ExitCode: NO_ERROR.0,
        dwServiceSpecificExitCode: 0,
        // The SCM only reads dwCheckPoint/dwWaitHint while a *_PENDING state is
        // reported. Leaving a stale non-zero checkpoint in the RUNNING status makes
        // some management tooling display the service as still starting.
        dwCheckPoint: if pending { CHECKPOINT.fetch_add(1, Ordering::SeqCst) } else { 0 },
        dwWaitHint: if pending { 10_000 } else { 0 },
    };
    unsafe {
        let _ = SetServiceStatus(s.status_handle, &status);
    }
}

/// The SCM's control callback. Runs on an SCM-owned thread, so it must return
/// promptly: all it does is translate the control into an event and post it.
unsafe extern "system" fn handler(
    control: u32,
    event_type: u32,
    event_data: *mut std::ffi::c_void,
    _context: *mut std::ffi::c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOPPING.store(true, Ordering::SeqCst);
            set_status(SERVICE_STOP_PENDING, 0);
            post(ServiceEvent::Stop);
            NO_ERROR.0
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            if !event_data.is_null() {
                let n = &*(event_data as *const WTSSESSION_NOTIFICATION);
                match event_type {
                    WTS_SESSION_LOGON => post(ServiceEvent::SessionLogon(n.dwSessionId)),
                    WTS_SESSION_LOGOFF => post(ServiceEvent::SessionLogoff(n.dwSessionId)),
                    // Unlock, remote connect/disconnect and console connect all
                    // arrive here too. The supervisor treats logon as idempotent, so
                    // ignoring them is correct rather than merely convenient.
                    _ => {}
                }
            }
            NO_ERROR.0
        }
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

fn post(ev: ServiceEvent) {
    let guard = shared().lock().unwrap();
    if let Some(s) = guard.as_ref() {
        // A full channel means the service body is wedged; dropping the event is
        // better than blocking an SCM callback thread, which would hang shutdown.
        let _ = s.tx.try_send(ev);
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name: Vec<u16> = crate::recovery::SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let (tx, rx) = std::sync::mpsc::sync_channel::<ServiceEvent>(64);

    let Ok(status_handle) = RegisterServiceCtrlHandlerExW(
        windows::core::PCWSTR(name.as_ptr()),
        Some(handler),
        None,
    ) else {
        return;
    };
    *shared().lock().unwrap() = Some(Shared { status_handle, tx });

    set_status(SERVICE_START_PENDING, 0);
    // SESSIONCHANGE must be in dwControlsAccepted or the SCM never delivers it, and
    // the agent then never launches at logon.
    let accepted = SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE;
    set_status(SERVICE_RUNNING, accepted);

    if let Some(body) = SERVICE_MAIN.get() {
        body(rx);
    }

    set_status(SERVICE_STOPPED, 0);
}

/// Hand control to the SCM. Returns when the service has stopped.
///
/// `body` runs on the SCM's `ServiceMain` thread and must return when it sees
/// [`ServiceEvent::Stop`] or [`stopping`] turns true.
pub fn run(body: fn(Receiver<ServiceEvent>)) -> windows::core::Result<()> {
    let _ = SERVICE_MAIN.set(body);
    let mut name: Vec<u16> = crate::recovery::SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }
}
