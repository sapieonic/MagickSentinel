//! The widget shell (spec 6.7, 13.1).
//!
//! WebView2 hosting the shared React bundle, in an always-on-top, frameless,
//! draggable window that snaps to a screen edge and remembers where the user put it.
//!
//! The native/JS boundary is deliberately narrow. `sentinel.*` exposes state the
//! native layer already holds and actions only the native layer can perform; anything
//! the API can answer — history, summaries — the WebView fetches itself with a token
//! the native layer injects. Widening this surface means widening what a compromised
//! renderer can do.
//!
//! Window management and the host-object surface sit behind [`WidgetShell`] so the
//! agent's state machine can be driven headlessly in CI. The WebView2 half is in
//! `webview2.rs` and compiles only on Windows.

pub mod position;

use serde::{Deserialize, Serialize};

/// What `sentinel.getState()` returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WidgetState {
    /// `"signed_out"` | `"signed_in"`.
    pub auth_state: String,
    /// `CallState::as_str()`.
    pub capture_state: String,
    /// `"A"` | `"B"` | null. Tier B shows a distinct indicator (spec 3, mitigation 3).
    pub tier: Option<String>,
    /// Today's capture coverage, 0–100, or null before the first reconciliation.
    pub coverage: Option<f32>,
    /// The call in progress, if any.
    pub call_id: Option<String>,
    /// Non-null whenever capture cannot run; the widget shows it verbatim so the
    /// agent is told what to fix rather than that something went wrong.
    pub message: Option<String>,
    /// Drives the non-dismissible recording indicator required by spec 12.4.
    pub recording: bool,
    /// Unacked segments on disk, shown when the floor has been offline.
    pub spool_depth: u64,
}

impl WidgetState {
    pub fn signed_out(message: &str) -> Self {
        WidgetState {
            auth_state: "signed_out".into(),
            capture_state: "BLOCKED".into(),
            tier: None,
            coverage: None,
            call_id: None,
            message: Some(message.into()),
            recording: false,
            spool_depth: 0,
        }
    }
}

/// A call from JavaScript into the native layer.
///
/// The complete `sentinel.*` surface from spec 6.7. `onStateChange` is not here: it
/// is a subscription the WebView registers, served by pushing [`WidgetState`] the
/// other way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum HostCall {
    GetState,
    SignIn,
    SignOut,
    #[serde(rename_all = "camelCase")]
    ConfirmCall {
        call_id: String,
        /// Disposition and PTP corrections, passed through to
        /// `POST /v1/me/calls/{id}/confirm` unexamined. The native layer has no
        /// business parsing a borrower's payment details.
        payload: serde_json::Value,
    },
    #[serde(rename_all = "camelCase")]
    OpenPortal {
        path: String,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HostCallError {
    #[error("host call is not valid JSON: {0}")]
    Malformed(String),
    #[error("unknown host method")]
    UnknownMethod,
    #[error("portal path must be a site-relative path, got {0:?}")]
    UnsafePortalPath(String),
}

/// Parse a message posted from the WebView.
pub fn parse_host_call(json: &str) -> Result<HostCall, HostCallError> {
    let call: HostCall = serde_json::from_str(json).map_err(|e| {
        if e.is_data() {
            HostCallError::UnknownMethod
        } else {
            HostCallError::Malformed(e.to_string())
        }
    })?;
    if let HostCall::OpenPortal { path } = &call {
        validate_portal_path(path)?;
    }
    Ok(call)
}

/// `openPortal` takes a path, never a URL.
///
/// The native layer resolves it against the configured portal origin. Accepting a
/// full URL would turn a compromised or merely buggy WebView into a way to make the
/// agent's default browser open anything at all — including a `file:` path — under
/// the user's session.
pub fn validate_portal_path(path: &str) -> Result<(), HostCallError> {
    let bad = !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("://")
        || path.contains('\\')
        || path.contains("..");
    if bad {
        return Err(HostCallError::UnsafePortalPath(path.to_string()));
    }
    Ok(())
}

/// Where the widget sits, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.width
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.height
    }
}

/// Distance, in pixels, within which the widget snaps to an edge.
pub const SNAP_THRESHOLD: i32 = 24;

/// Snap a dragged window to the nearest work-area edge, and keep it on screen.
///
/// `work_area` is the monitor's work area — the desktop minus the taskbar — not its
/// full bounds. Snapping to the full bounds puts the widget behind the taskbar, where
/// an always-on-top window is still clickable but the agent cannot see the recording
/// indicator, which is the one thing spec 12.4 requires to be visible.
pub fn snap_to_edge(window: Rect, work_area: Rect, threshold: i32) -> Rect {
    let mut x = window.x;
    let mut y = window.y;

    if (window.x - work_area.x).abs() <= threshold {
        x = work_area.x;
    } else if (work_area.right() - window.right()).abs() <= threshold {
        x = work_area.right() - window.width;
    }
    if (window.y - work_area.y).abs() <= threshold {
        y = work_area.y;
    } else if (work_area.bottom() - window.bottom()).abs() <= threshold {
        y = work_area.bottom() - window.height;
    }

    // Clamp last: a monitor that was unplugged, or a resolution change between shifts,
    // can leave a remembered position entirely off the desktop. A widget nobody can
    // see is a widget whose recording indicator nobody can see.
    x = x.clamp(work_area.x, (work_area.right() - window.width).max(work_area.x));
    y = y.clamp(work_area.y, (work_area.bottom() - window.height).max(work_area.y));

    Rect { x, y, ..window }
}

/// The window the agent drives. Implemented by the WebView2 shell on Windows and by
/// [`HeadlessWidget`] everywhere else.
pub trait WidgetShell: Send {
    /// Push new state to the WebView, which fans it out to `onStateChange`.
    fn post_state(&mut self, state: &WidgetState) -> anyhow::Result<()>;
    /// Take any host calls that arrived since the last poll.
    fn drain_host_calls(&mut self) -> Vec<HostCall>;
    fn set_position(&mut self, rect: Rect) -> anyhow::Result<()>;
    fn position(&self) -> Rect;
    fn show(&mut self) -> anyhow::Result<()>;
    fn hide(&mut self) -> anyhow::Result<()>;
}

/// A shell with no window. Used off Windows and by tests; it holds the same state the
/// real one does so the agent's logic is exercised identically.
#[derive(Debug)]
pub struct HeadlessWidget {
    pub last_state: Option<WidgetState>,
    pub pending_calls: Vec<HostCall>,
    pub rect: Rect,
    pub visible: bool,
}

impl Default for HeadlessWidget {
    fn default() -> Self {
        HeadlessWidget {
            last_state: None,
            pending_calls: Vec::new(),
            rect: position::DEFAULT_RECT,
            visible: false,
        }
    }
}

impl WidgetShell for HeadlessWidget {
    fn post_state(&mut self, state: &WidgetState) -> anyhow::Result<()> {
        self.last_state = Some(state.clone());
        Ok(())
    }
    fn drain_host_calls(&mut self) -> Vec<HostCall> {
        std::mem::take(&mut self.pending_calls)
    }
    fn set_position(&mut self, rect: Rect) -> anyhow::Result<()> {
        self.rect = rect;
        Ok(())
    }
    fn position(&self) -> Rect {
        self.rect
    }
    fn show(&mut self) -> anyhow::Result<()> {
        self.visible = true;
        Ok(())
    }
    fn hide(&mut self) -> anyhow::Result<()> {
        self.visible = false;
        Ok(())
    }
}

#[cfg(windows)]
pub mod webview2;

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: Rect = Rect { x: 0, y: 0, width: 1920, height: 1040 };

    #[test]
    fn every_documented_host_method_parses() {
        assert_eq!(parse_host_call(r#"{"method":"getState"}"#).unwrap(), HostCall::GetState);
        assert_eq!(parse_host_call(r#"{"method":"signIn"}"#).unwrap(), HostCall::SignIn);
        assert_eq!(parse_host_call(r#"{"method":"signOut"}"#).unwrap(), HostCall::SignOut);
        assert_eq!(
            parse_host_call(r#"{"method":"openPortal","path":"/calls/01J8"}"#).unwrap(),
            HostCall::OpenPortal { path: "/calls/01J8".into() }
        );
        let confirm = parse_host_call(
            r#"{"method":"confirmCall","callId":"01J8","payload":{"disposition":"ptp"}}"#,
        )
        .unwrap();
        assert_eq!(
            confirm,
            HostCall::ConfirmCall {
                call_id: "01J8".into(),
                payload: serde_json::json!({"disposition":"ptp"}),
            }
        );
    }

    #[test]
    fn an_unknown_method_is_rejected_rather_than_ignored() {
        assert_eq!(
            parse_host_call(r#"{"method":"readSpool"}"#),
            Err(HostCallError::UnknownMethod)
        );
        assert!(matches!(parse_host_call("not json"), Err(HostCallError::Malformed(_))));
    }

    #[test]
    fn open_portal_refuses_anything_that_is_not_a_site_relative_path() {
        // A full URL here would let the renderer make the agent's browser open
        // anything at all under the user's session.
        for bad in [
            "https://evil.example.com/",
            "//evil.example.com/",
            "file:///C:/Windows/System32/calc.exe",
            "calls/01J8",
            "/../../etc/passwd",
            "/calls\\..\\x",
            "",
        ] {
            assert!(
                validate_portal_path(bad).is_err(),
                "{bad:?} should be refused as a portal path"
            );
        }
        for good in ["/", "/calls", "/calls/01J8ZQ8H2Q7X9K3M4N5P6R7S8T", "/me/stats?tab=ptp"] {
            assert!(validate_portal_path(good).is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn a_window_near_an_edge_snaps_flush_to_it() {
        let w = Rect { x: 12, y: 900, width: 320, height: 120 };
        let snapped = snap_to_edge(w, WORK, SNAP_THRESHOLD);
        assert_eq!(snapped.x, 0, "the left edge is within the threshold");
        assert_eq!(snapped.y, WORK.height - 120, "so is the bottom");
        assert_eq!((snapped.width, snapped.height), (320, 120), "snapping never resizes");
    }

    #[test]
    fn a_window_in_open_space_is_left_where_the_agent_put_it() {
        let w = Rect { x: 800, y: 400, width: 320, height: 120 };
        assert_eq!(snap_to_edge(w, WORK, SNAP_THRESHOLD), w);
    }

    #[test]
    fn snapping_uses_the_work_area_so_the_widget_does_not_hide_behind_the_taskbar() {
        // Spec 12.4: the recording indicator must be visible. Snapping to the
        // monitor's full bounds instead of the work area puts it under the taskbar.
        let work = Rect { x: 0, y: 0, width: 1920, height: 1040 }; // 1080 minus taskbar
        let w = Rect { x: 1600, y: 1030, width: 320, height: 120 };
        let snapped = snap_to_edge(w, work, SNAP_THRESHOLD);
        assert_eq!(snapped.bottom(), 1040);
        assert!(snapped.bottom() <= work.bottom());
    }

    #[test]
    fn a_position_remembered_from_a_monitor_that_is_gone_is_pulled_back_on_screen() {
        // The agent docked their laptop yesterday and did not today.
        let w = Rect { x: 3000, y: -400, width: 320, height: 120 };
        let snapped = snap_to_edge(w, WORK, SNAP_THRESHOLD);
        assert!(snapped.x >= WORK.x && snapped.right() <= WORK.right());
        assert!(snapped.y >= WORK.y && snapped.bottom() <= WORK.bottom());
    }

    #[test]
    fn a_widget_wider_than_the_work_area_is_pinned_rather_than_placed_negatively() {
        let narrow = Rect { x: 0, y: 0, width: 200, height: 200 };
        let w = Rect { x: 50, y: 50, width: 400, height: 400 };
        let snapped = snap_to_edge(w, narrow, SNAP_THRESHOLD);
        assert_eq!((snapped.x, snapped.y), (0, 0));
    }

    #[test]
    fn the_headless_shell_records_what_the_agent_pushed() {
        let mut w = HeadlessWidget::default();
        assert!(!w.visible);
        w.show().unwrap();
        w.post_state(&WidgetState::signed_out("Sign in to start")).unwrap();
        assert!(w.visible);
        assert_eq!(w.last_state.as_ref().unwrap().auth_state, "signed_out");
        assert!(!w.last_state.as_ref().unwrap().recording);

        w.pending_calls.push(HostCall::SignIn);
        assert_eq!(w.drain_host_calls(), vec![HostCall::SignIn]);
        assert!(w.drain_host_calls().is_empty(), "draining twice yields nothing");
    }

    #[test]
    fn the_state_object_serialises_with_the_field_names_the_bundle_expects() {
        let s = WidgetState {
            auth_state: "signed_in".into(),
            capture_state: "IN_CALL".into(),
            tier: Some("B".into()),
            coverage: Some(98.5),
            call_id: Some("01J8".into()),
            message: None,
            recording: true,
            spool_depth: 3,
        };
        let v = serde_json::to_value(&s).unwrap();
        for k in ["authState", "captureState"] {
            assert!(v.get(k).is_none(), "the bundle reads snake_case, not {k}");
        }
        assert_eq!(v["auth_state"], "signed_in");
        assert_eq!(v["tier"], "B");
        assert_eq!(v["recording"], true);
    }
}
