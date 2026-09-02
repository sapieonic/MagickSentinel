//! Crash-dump collection (spec 15: "crash dumps from the service, symbolicated,
//! retained 30 days").
//!
//! Windows Error Reporting is configured by the installer to drop dumps for our two
//! binaries into `%PROGRAMDATA%\MagickVoice\Sentinel\dumps`. This module decides
//! which of them to ship and when to delete them; the upload itself is an ordinary
//! authenticated POST.
//!
//! A minidump of the agent can contain heap, and the agent's heap holds decoded
//! audio and — if the UIA scrape succeeded — an account reference. That makes a dump
//! PII-bearing evidence, not a log line: it is uploaded over the same authenticated
//! channel as call audio, never written to a shared location, and deleted locally
//! once shipped.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Dumps older than this are deleted unshipped. Matches the server-side retention so
/// the endpoint never holds evidence longer than the service that receives it.
pub const MAX_DUMP_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// Refuse to accumulate more than this many dumps. A crash loop must not fill the
/// same disk the spool lives on — losing call audio to a debugging aid would be a
/// poor trade.
pub const MAX_DUMPS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dump {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub bytes: u64,
}

/// Split a set of dumps into those to upload (newest first) and those to delete
/// unshipped.
///
/// Newest first because a crash loop produces near-identical dumps and the most
/// recent one is the one that matches the currently deployed build.
pub fn triage(mut dumps: Vec<Dump>, now: SystemTime) -> (Vec<Dump>, Vec<Dump>) {
    dumps.sort_by(|a, b| b.modified.cmp(&a.modified));
    let mut ship = Vec::new();
    let mut drop = Vec::new();
    for d in dumps {
        let too_old = now
            .duration_since(d.modified)
            .map(|age| age > MAX_DUMP_AGE)
            .unwrap_or(false);
        if too_old || ship.len() >= MAX_DUMPS {
            drop.push(d);
        } else {
            ship.push(d);
        }
    }
    (ship, drop)
}

/// Enumerate `.dmp` files in the dump directory.
pub fn scan(dir: &Path) -> std::io::Result<Vec<Dump>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // No directory yet simply means nothing has crashed.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("dmp") {
            continue;
        }
        let meta = entry.metadata()?;
        out.push(Dump {
            path,
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            bytes: meta.len(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump(name: &str, age: Duration, now: SystemTime) -> Dump {
        Dump { path: PathBuf::from(name), modified: now - age, bytes: 1024 }
    }

    #[test]
    fn dumps_older_than_the_retention_window_are_dropped_unshipped() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(90 * 24 * 3600);
        let fresh = dump("new.dmp", Duration::from_secs(3600), now);
        let stale = dump("old.dmp", MAX_DUMP_AGE + Duration::from_secs(1), now);
        let (ship, drop) = triage(vec![stale.clone(), fresh.clone()], now);
        assert_eq!(ship, vec![fresh]);
        assert_eq!(drop, vec![stale]);
    }

    #[test]
    fn the_newest_dumps_are_shipped_first_and_the_backlog_is_capped() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(90 * 24 * 3600);
        let dumps: Vec<Dump> = (0..MAX_DUMPS + 5)
            .map(|i| dump(&format!("{i}.dmp"), Duration::from_secs(i as u64 * 60), now))
            .collect();
        let (ship, drop) = triage(dumps, now);
        assert_eq!(ship.len(), MAX_DUMPS);
        assert_eq!(drop.len(), 5);
        assert_eq!(ship[0].path, PathBuf::from("0.dmp"), "newest first");
        // A crash loop must not fill the disk the spool shares.
        assert!(drop.iter().all(|d| ship.iter().all(|s| s.path != d.path)));
    }

    #[test]
    fn a_missing_dump_directory_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan(&dir.path().join("never-created")).unwrap().is_empty());
    }

    #[test]
    fn only_dmp_files_are_collected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.dmp"), b"x").unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"x").unwrap();
        let found = scan(dir.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path.file_name().unwrap(), "a.dmp");
    }
}
