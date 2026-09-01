//! The Win32 window the WebView2 control lives in (spec 6.7).
//!
//! Always-on-top, frameless, draggable, snap-to-edge, position persisted per user.
//! This module owns the window; the WebView2 controller that fills it is noted below.
//!
//! Frameless plus draggable is the awkward combination. A `WS_POPUP` window has no
//! title bar, so Windows has nothing to drag it by — the standard answer is to return
//! `HTCAPTION` from `WM_NCHITTEST` for the parts of the client area that are not
//! interactive, which makes the non-window-manager code believe it is a caption. That
//! only works for the parts the WebView2 control does not cover: once a child HWND is
//! over a pixel, the hit test goes to the child. The bundle therefore declares its own
//! drag region and posts `sentinel.beginDrag`, which forwards `WM_NCLBUTTONDOWN` with
//! `HTCAPTION` here.
//!
//! Other details that are easy to get wrong:
//!
//! * **`WS_EX_TOOLWINDOW`** keeps the widget out of the Alt-Tab list and off the
//!   taskbar. Without it an always-on-top window an agent cannot close becomes an
//!   Alt-Tab entry they will try to close, repeatedly.
//! * **`WS_EX_NOACTIVATE`** is deliberately *not* set. The agent has to be able to
//!   click Sign in and type into the post-call card.
//! * **Per-monitor DPI.** `SetProcessDpiAwarenessContext` must be called before the
//!   first window is created; afterwards it fails and the process stays
//!   system-DPI-aware, so the widget renders blurry on the second monitor and the
//!   remembered position lands in the wrong place.

use super::{HostCall, Rect, WidgetShell, WidgetState, SNAP_THRESHOLD};
use std::sync::{Arc, Mutex};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowRect, PostMessageW, RegisterClassW,
    SetWindowPos, ShowWindow, HTCAPTION, HTCLIENT, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WM_DESTROY, WM_EXITSIZEMOVE, WM_NCHITTEST,
    WM_NCLBUTTONDOWN, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

const CLASS_NAME: PCWSTR = w!("MagickVoiceSentinelWidget");

/// Shared between the window procedure and the agent thread.
#[derive(Default)]
struct Shared {
    /// Host calls posted by the WebView, waiting to be drained.
    inbox: Vec<HostCall>,
    /// Set by `WM_EXITSIZEMOVE`: the agent drag-moved the window and the new position
    /// should be snapped and persisted.
    moved: bool,
}

/// The widget window.
pub struct WebView2Widget {
    hwnd: HWND,
    shared: Arc<Mutex<Shared>>,
    rect: Rect,
}

// The HWND is owned by this value and only touched from the thread that created it;
// `Shared` behind a mutex is what actually crosses threads.
unsafe impl Send for WebView2Widget {}

impl WebView2Widget {
    /// Create the window at `rect`.
    ///
    /// Call before any other window in the process exists: the DPI awareness context
    /// can only be set while none has been created.
    pub fn create(rect: Rect) -> windows::core::Result<Self> {
        unsafe {
            // Failure here is not fatal — an older build that does not know this
            // context still runs, just system-DPI-aware and blurry on a second
            // monitor — so it is logged rather than propagated.
            if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
                tracing::debug!("per-monitor DPI awareness unavailable; widget may render blurry");
            }

            let class = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            // A zero return can mean "already registered" from a previous attempt in
            // the same process, which is not an error worth failing the widget over.
            let _ = RegisterClassW(&class);

            let hwnd = CreateWindowExW(
                // TOPMOST so the recording indicator stays visible over the dialer;
                // TOOLWINDOW so the widget is not an Alt-Tab entry an agent will try
                // to close. Deliberately no NOACTIVATE: Sign in must be clickable.
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                CLASS_NAME,
                w!("MagickVoice Sentinel"),
                WS_POPUP | WS_VISIBLE,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                None,
                None,
                None,
                None,
            )?;

            let widget = WebView2Widget {
                hwnd,
                shared: Arc::new(Mutex::new(Shared::default())),
                rect,
            };
            // The window procedure has no instance pointer to reach the widget
            // through, so its shared state has to be published before any message can
            // arrive. Doing it here rather than leaving it to the caller means a
            // caller who forgets cannot silently lose drag handling.
            let _ = SHARED.set(widget.shared.clone());
            Ok(widget)
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Current bounds, read back from the window rather than from our own record: a
    /// drag moves the window without telling us.
    pub fn current_rect(&self) -> Rect {
        let mut r = RECT::default();
        if unsafe { GetWindowRect(self.hwnd, &mut r) }.is_err() {
            return self.rect;
        }
        Rect {
            x: r.left,
            y: r.top,
            width: r.right - r.left,
            height: r.bottom - r.top,
        }
    }

    /// The work area of the monitor the widget is on — the desktop minus the taskbar.
    pub fn work_area(&self) -> Rect {
        unsafe {
            let monitor = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut mi).as_bool() {
                let w = mi.rcWork;
                Rect {
                    x: w.left,
                    y: w.top,
                    width: w.right - w.left,
                    height: w.bottom - w.top,
                }
            } else {
                // Fall back to the widget's own bounds, which at least keeps it where
                // it is rather than snapping it to a guessed origin.
                self.current_rect()
            }
        }
    }

    /// If the agent finished dragging, snap and return the new position to persist.
    pub fn take_drag_result(&mut self) -> Option<Rect> {
        let moved = {
            let mut s = self.shared.lock().unwrap();
            std::mem::replace(&mut s.moved, false)
        };
        if !moved {
            return None;
        }
        let snapped = super::snap_to_edge(self.current_rect(), self.work_area(), SNAP_THRESHOLD);
        let _ = self.set_position(snapped);
        Some(snapped)
    }

    /// Begin a caption drag, in response to `sentinel.beginDrag` from the bundle.
    ///
    /// A frameless window has no caption for Windows to drag by, and once the WebView2
    /// child HWND covers the client area, `WM_NCHITTEST` on the parent never fires for
    /// those pixels. Posting `WM_NCLBUTTONDOWN` with `HTCAPTION` hands the drag to the
    /// window manager exactly as a real title bar would.
    pub fn begin_drag(&self) {
        unsafe {
            let _ = PostMessageW(
                self.hwnd,
                WM_NCLBUTTONDOWN,
                WPARAM(HTCAPTION as usize),
                LPARAM(0),
            );
        }
    }

    /// Deliver a host call from the WebView's `web_message_received` handler.
    pub fn push_host_call(&self, call: HostCall) {
        self.shared.lock().unwrap().inbox.push(call);
    }

    // TODO(OPEN-5 adjacent, not the same decision): create the WebView2 controller
    // and navigate it to the agent-only routes of the shared React bundle. Blocked
    // here on the WebView2 SDK: the `ICoreWebView2*` interfaces are not in
    // `windows-rs`, they come from the `Microsoft.Web.WebView2` NuGet package via the
    // `webview2-com` crate, and neither NuGet nor that crate's generated bindings are
    // available in this build environment. What is needed, once they are:
    //
    //   1. `CreateCoreWebView2EnvironmentWithOptions` with a user-data folder under
    //      %LOCALAPPDATA% — per user, like the widget position, or two shifts share a
    //      cookie jar.
    //   2. `ICoreWebView2Controller` parented to `self.hwnd`, resized on WM_SIZE.
    //   3. `AddScriptToExecuteOnDocumentCreated` installing the `sentinel.*` shim that
    //      marshals to `chrome.webview.postMessage`, so the bundle sees the surface
    //      documented in spec 6.7 rather than raw message passing.
    //   4. `add_WebMessageReceived` → `parse_host_call` → `push_host_call`.
    //   5. `AddHostObjectToScript` is deliberately NOT used: it exposes a live COM
    //      object to the renderer, which is a far wider surface than a JSON message
    //      channel for no benefit here.
    //   6. `SetVirtualHostNameToFolderMapping` for the bundle, so it loads from disk
    //      under an https-like origin rather than `file:`, which browsers treat as
    //      opaque and which would break the fetch calls to the API.
    pub fn attach_webview(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("WebView2 host not implemented: WebView2 SDK bindings unavailable")
    }
}

impl Drop for WebView2Widget {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

impl WidgetShell for WebView2Widget {
    fn post_state(&mut self, state: &WidgetState) -> anyhow::Result<()> {
        let _json = serde_json::to_string(state)?;
        // TODO: forward to `ICoreWebView2::PostWebMessageAsJson` once the controller
        // above exists. Until then the state is computed and validated but not
        // rendered; the agent's own logic does not depend on the WebView receiving it.
        Ok(())
    }

    fn drain_host_calls(&mut self) -> Vec<HostCall> {
        std::mem::take(&mut self.shared.lock().unwrap().inbox)
    }

    fn set_position(&mut self, rect: Rect) -> anyhow::Result<()> {
        unsafe {
            // HWND_TOPMOST on every move: another application calling
            // SetForegroundWindow can knock a topmost window out of the topmost band,
            // and a recording indicator that has slipped behind the dialer is not the
            // "always present" spec 12.4 requires.
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                SWP_NOACTIVATE,
            )?;
        }
        self.rect = rect;
        Ok(())
    }

    fn position(&self) -> Rect {
        self.rect
    }

    fn show(&mut self) -> anyhow::Result<()> {
        unsafe {
            // SW_SHOWNOACTIVATE, not SW_SHOW: the widget appearing mid-call must not
            // steal focus from the dialer the agent is typing into.
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            // Re-assert the Z-order and nothing else.
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )?;
        }
        Ok(())
    }

    fn hide(&mut self) -> anyhow::Result<()> {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        Ok(())
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            // Report the client area as caption so the frameless window can be
            // dragged from anywhere the WebView2 child does not cover. Where it does,
            // the hit test never reaches this procedure and the bundle's own drag
            // region calls `begin_drag` instead.
            let hit = DefWindowProcW(hwnd, msg, wparam, lparam);
            if hit.0 == HTCLIENT as isize {
                LRESULT(HTCAPTION as isize)
            } else {
                hit
            }
        }
        WM_EXITSIZEMOVE => {
            // The drag finished. The agent thread picks this up and snaps.
            if let Some(shared) = SHARED.get() {
                shared.lock().unwrap().moved = true;
            }
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// The window procedure is a bare `extern "system" fn` with no instance pointer, so
/// the drag flag it sets has to reach the widget through process state. There is
/// exactly one widget per agent process — one instance per interactive session,
/// guarded by the named mutex in `main` — so a single slot is sufficient and a
/// per-HWND map would be ceremony.
static SHARED: std::sync::OnceLock<Arc<Mutex<Shared>>> = std::sync::OnceLock::new();

/// Point in a rect, for hit-testing the bundle's declared drag region.
pub fn point_in(rect: Rect, p: POINT) -> bool {
    p.x >= rect.x && p.x < rect.right() && p.y >= rect.y && p.y < rect.bottom()
}
