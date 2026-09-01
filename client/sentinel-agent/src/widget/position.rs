//! Widget position, persisted per user (spec 6.7).
//!
//! Per user, not per machine: collections floors run two or three shifts on the same
//! desktop, and the morning agent moving the widget out of their dialer's way must
//! not move it for the evening agent. `%LOCALAPPDATA%` is per-profile, so the path
//! alone gives that — as long as the agent is launched with the user's own
//! environment block, which is why `windows::launcher` goes to the trouble.
//!
//! Not roaming (`%APPDATA%`): a pixel position is meaningless on a different desktop
//! with a different monitor layout.

use super::Rect;
use std::path::PathBuf;

/// Where the widget appears before the agent has ever moved it: bottom-right, above
/// the taskbar, out of the way of a maximised dialer window.
pub const DEFAULT_RECT: Rect = Rect { x: 1560, y: 880, width: 340, height: 140 };

/// Per-user state directory.
pub fn state_dir() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var("LOCALAPPDATA")
            .or_else(|_| std::env::var("APPDATA"))
            .unwrap_or_else(|_| ".".into());
        PathBuf::from(base).join("MagickVoice").join("Sentinel")
    } else {
        std::env::var("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join("magickvoice-sentinel")
    }
}

fn path_for(dir: &std::path::Path, uid: &str) -> PathBuf {
    // The UID is interpolated into a filename, so it is reduced to characters that
    // cannot traverse or collide. Firebase UIDs are already alphanumeric; a hostile
    // one is not this component's problem to detect, only to contain.
    let safe: String = uid
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    dir.join(format!("widget-{safe}.json"))
}

/// Load the remembered position, or the default.
///
/// Never fails: a widget that will not appear because its position file is corrupt is
/// a widget the agent cannot use to sign in.
pub fn load(dir: &std::path::Path, uid: &str) -> Rect {
    std::fs::read_to_string(path_for(dir, uid))
        .ok()
        .and_then(|s| serde_json::from_str::<Rect>(&s).ok())
        .unwrap_or(DEFAULT_RECT)
}

pub fn save(dir: &std::path::Path, uid: &str, rect: Rect) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(path_for(dir, uid), serde_json::to_vec(&rect)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_round_trips_for_one_user() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), "uid-a"), DEFAULT_RECT);
        let moved = Rect { x: 40, y: 40, width: 340, height: 140 };
        save(dir.path(), "uid-a", moved).unwrap();
        assert_eq!(load(dir.path(), "uid-a"), moved);
    }

    #[test]
    fn two_shifts_on_one_desktop_keep_separate_positions() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), "uid-morning", Rect { x: 0, y: 0, width: 340, height: 140 }).unwrap();
        save(dir.path(), "uid-evening", Rect { x: 900, y: 500, width: 340, height: 140 }).unwrap();
        assert_eq!(load(dir.path(), "uid-morning").x, 0);
        assert_eq!(load(dir.path(), "uid-evening").x, 900);
    }

    #[test]
    fn a_corrupt_position_file_falls_back_to_the_default() {
        // A widget that will not appear is a widget the agent cannot sign in from.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("widget-uid-a.json"), b"{{{").unwrap();
        assert_eq!(load(dir.path(), "uid-a"), DEFAULT_RECT);
    }

    #[test]
    fn a_uid_cannot_escape_the_state_directory() {
        let dir = std::path::Path::new("/state");
        let p = path_for(dir, "../../windows/system32");
        assert_eq!(p.parent().unwrap(), dir);
        assert!(!p.to_string_lossy().contains(".."));
    }
}
