//! Disk spool for captured audio (spec section 6.5).
//!
//! A SQLCipher database at `%PROGRAMDATA%\MagickVoice\Sentinel\spool.db`. The key is
//! generated per machine at enrollment and wrapped with DPAPI at **machine** scope:
//! the service and the agent run as different principals, so user scope would leave
//! one of them unable to open the file.
//!
//! Two invariants this module exists to hold:
//!
//! * **A segment is deleted only after the server acks it.** Never before, never on
//!   `call.end`, never on shutdown.
//! * **Eviction is never silent.** Hitting a cap emits a `spool_eviction` event
//!   carrying the count of lost segments. A compliance product that quietly drops
//!   audio is worse than one that admits it did.

use crate::config::SpoolLimits;
use crate::events::{ClientEvent, EventKind};
use crate::protocol::{Channel, MediaFlags, MediaRecord};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::path::Path;

// A release build must not ship an unencrypted spool. The `sqlcipher` feature is off
// by default so CI on Linux can run the spool tests without the SQLCipher toolchain,
// and that convenience is exactly how an unencrypted build reaches a customer
// desktop holding borrower audio. Failing the compile is the only check that cannot
// be forgotten; `allow-unencrypted-spool` exists for the rare release-mode benchmark
// and has to be typed deliberately.
#[cfg(all(
    not(debug_assertions),
    not(feature = "sqlcipher"),
    not(feature = "allow-unencrypted-spool")
))]
compile_error!(
    "release builds must enable the `sqlcipher` feature: the spool holds borrower \
     audio at rest. Build with --features sqlcipher, or --features \
     allow-unencrypted-spool if you genuinely intend an unencrypted database."
);

#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    #[error("spool database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("spool is sealed after a fatal server error for call {0}")]
    Sealed(String),
    #[error("no spool encryption key was supplied")]
    MissingKey,
    #[error("the spool key did not open the database")]
    WrongKey,
}

/// A spooled segment, as handed back for upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRow {
    pub call_id: String,
    pub channel: Channel,
    pub seq: u32,
    pub timestamp_ms: u64,
    pub flags: MediaFlags,
    pub payload: Vec<u8>,
    /// Local monotonic-ish creation time, used for age-based eviction.
    pub created_ms: u64,
}

impl SegmentRow {
    pub fn to_record(&self, call_id_bin: [u8; 16]) -> MediaRecord {
        MediaRecord {
            channel: self.channel,
            flags: self.flags,
            seq: self.seq,
            timestamp_ms: self.timestamp_ms,
            call_id: call_id_bin,
            payload: self.payload.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpoolStats {
    pub segments: u64,
    pub bytes: u64,
    pub oldest_created_ms: Option<u64>,
}

pub struct Spool {
    conn: Connection,
    limits: SpoolLimits,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;   -- a torn write here is lost evidence

CREATE TABLE IF NOT EXISTS segments (
  call_id      TEXT    NOT NULL,
  channel      INTEGER NOT NULL,
  seq          INTEGER NOT NULL,
  timestamp_ms INTEGER NOT NULL,
  flags        INTEGER NOT NULL,
  payload      BLOB    NOT NULL,
  created_ms   INTEGER NOT NULL,
  PRIMARY KEY (call_id, channel, seq)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS segments_by_age ON segments (created_ms);

-- One row per call, holding the control frames verbatim so a reconnect can replay
-- call.start and call.end exactly as first sent.
CREATE TABLE IF NOT EXISTS calls (
  call_id    TEXT PRIMARY KEY,
  start_json TEXT NOT NULL,
  end_json   TEXT,
  sealed     INTEGER NOT NULL DEFAULT 0,
  created_ms INTEGER NOT NULL
);

-- Cumulative ack watermark, mirroring the server's.
CREATE TABLE IF NOT EXISTS acks (
  call_id     TEXT    NOT NULL,
  channel     INTEGER NOT NULL,
  through_seq INTEGER NOT NULL,
  PRIMARY KEY (call_id, channel)
);
"#;

impl Spool {
    /// Open (or create) the spool.
    ///
    /// `key` is the DPAPI-unwrapped SQLCipher key. Under the `sqlcipher` feature it
    /// is applied with `PRAGMA key`; without it — CI on Linux — the database is plain
    /// SQLite and the key is ignored, so the logic below is exercised without needing
    /// the SQLCipher toolchain. Production builds MUST enable the feature.
    pub fn open(path: &Path, key: &str, limits: SpoolLimits) -> Result<Self, SpoolError> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn, key, limits)
    }

    /// In-memory spool for tests.
    pub fn open_in_memory(limits: SpoolLimits) -> Result<Self, SpoolError> {
        Self::from_connection(Connection::open_in_memory()?, "test", limits)
    }

    fn from_connection(conn: Connection, key: &str, limits: SpoolLimits) -> Result<Self, SpoolError> {
        #[cfg(feature = "sqlcipher")]
        {
            if key.is_empty() {
                return Err(SpoolError::MissingKey);
            }
            conn.pragma_update(None, "key", key)?;
            // Prove the key worked before handing the caller a Spool. Without this a
            // wrong key surfaces as a corrupt-database error on the first write,
            // mid-call, rather than at open time where it can be reported.
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
                .map_err(|_| SpoolError::WrongKey)?;
        }
        #[cfg(not(feature = "sqlcipher"))]
        {
            let _ = key;
        }
        conn.execute_batch(SCHEMA)?;
        Ok(Spool { conn, limits })
    }

    // ------------------------------------------------------------------ calls

    /// Record the `call.start` frame for a call. Idempotent: a reconnect replays the
    /// original frame rather than minting a new one.
    pub fn begin_call(&self, call_id: &str, start_json: &str, now_ms: u64) -> Result<(), SpoolError> {
        self.conn.execute(
            "INSERT INTO calls (call_id, start_json, created_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(call_id) DO NOTHING",
            params![call_id, start_json, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn end_call(&self, call_id: &str, end_json: &str) -> Result<(), SpoolError> {
        self.conn.execute(
            "UPDATE calls SET end_json = ?2 WHERE call_id = ?1",
            params![call_id, end_json],
        )?;
        Ok(())
    }

    pub fn call_start_json(&self, call_id: &str) -> Result<Option<String>, SpoolError> {
        Ok(self
            .conn
            .query_row("SELECT start_json FROM calls WHERE call_id = ?1", params![call_id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn call_end_json(&self, call_id: &str) -> Result<Option<String>, SpoolError> {
        Ok(self
            .conn
            .query_row("SELECT end_json FROM calls WHERE call_id = ?1", params![call_id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten())
    }

    /// Calls that still hold unacked segments, oldest first. This is the reconnect
    /// work list.
    pub fn pending_calls(&self) -> Result<Vec<String>, SpoolError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.call_id FROM segments s
             JOIN calls c ON c.call_id = s.call_id
             WHERE c.sealed = 0
             ORDER BY (SELECT min(created_ms) FROM segments x WHERE x.call_id = s.call_id)",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // --------------------------------------------------------------- segments

    /// Append one segment. Returns any eviction event the write triggered.
    ///
    /// Eviction happens on write rather than on a timer so the caps are honoured
    /// exactly, and so the event is emitted at the moment audio is actually lost.
    pub fn push(&mut self, seg: &SegmentRow) -> Result<Option<ClientEvent>, SpoolError> {
        if self.is_sealed(&seg.call_id)? {
            return Err(SpoolError::Sealed(seg.call_id.clone()));
        }
        self.conn.execute(
            "INSERT INTO segments (call_id, channel, seq, timestamp_ms, flags, payload, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(call_id, channel, seq) DO NOTHING",
            params![
                seg.call_id,
                seg.channel.as_u8(),
                seg.seq,
                seg.timestamp_ms as i64,
                seg.flags.to_bits(),
                seg.payload,
                seg.created_ms as i64,
            ],
        )?;
        self.enforce_limits(seg.created_ms)
    }

    /// Unacked segments for a call, in `(channel, seq)` order, up to `limit`.
    pub fn take_pending(&self, call_id: &str, limit: usize) -> Result<Vec<SegmentRow>, SpoolError> {
        let mut stmt = self.conn.prepare(
            "SELECT s.call_id, s.channel, s.seq, s.timestamp_ms, s.flags, s.payload, s.created_ms
             FROM segments s
             LEFT JOIN acks a ON a.call_id = s.call_id AND a.channel = s.channel
             WHERE s.call_id = ?1 AND s.seq > COALESCE(a.through_seq, -1)
             ORDER BY s.channel, s.seq
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![call_id, limit as i64], Self::map_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SegmentRow> {
        Ok(SegmentRow {
            call_id: r.get(0)?,
            channel: Channel::from_u8(r.get::<_, u8>(1)?).unwrap_or(Channel::Far),
            seq: r.get(2)?,
            timestamp_ms: r.get::<_, i64>(3)? as u64,
            flags: MediaFlags::from_bits(r.get::<_, u8>(4)?).unwrap_or_default(),
            payload: r.get(5)?,
            created_ms: r.get::<_, i64>(6)? as u64,
        })
    }

    /// Apply a cumulative ack: every sequence at or below `through_seq` on this
    /// channel is durable server-side and may now be deleted locally.
    ///
    /// Acks never move backwards, so a stale ack arriving out of order after a
    /// reconnect cannot resurrect deleted rows or re-send acked audio.
    pub fn ack(&mut self, call_id: &str, channel: Channel, through_seq: u32) -> Result<usize, SpoolError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO acks (call_id, channel, through_seq) VALUES (?1, ?2, ?3)
             ON CONFLICT(call_id, channel)
             DO UPDATE SET through_seq = max(through_seq, excluded.through_seq)",
            params![call_id, channel.as_u8(), through_seq],
        )?;
        let effective: u32 = tx.query_row(
            "SELECT through_seq FROM acks WHERE call_id = ?1 AND channel = ?2",
            params![call_id, channel.as_u8()],
            |r| r.get(0),
        )?;
        let deleted = tx.execute(
            "DELETE FROM segments WHERE call_id = ?1 AND channel = ?2 AND seq <= ?3",
            params![call_id, channel.as_u8(), effective],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    pub fn acked_through(&self, call_id: &str) -> Result<BTreeMap<Channel, u32>, SpoolError> {
        let mut stmt = self
            .conn
            .prepare("SELECT channel, through_seq FROM acks WHERE call_id = ?1")?;
        let rows = stmt.query_map(params![call_id], |r| {
            Ok((Channel::from_u8(r.get::<_, u8>(0)?).unwrap_or(Channel::Far), r.get::<_, u32>(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Drop everything for a call the server has fatally rejected. The audio can
    /// never be accepted, so holding it just fills the disk — but the loss is
    /// reported, not swallowed.
    pub fn seal(&mut self, call_id: &str, reason: &str, now_ms: u64) -> Result<ClientEvent, SpoolError> {
        let tx = self.conn.transaction()?;
        let lost: u64 = tx.query_row(
            "SELECT count(*) FROM segments WHERE call_id = ?1",
            params![call_id],
            |r| r.get::<_, i64>(0).map(|v| v as u64),
        )?;
        tx.execute("DELETE FROM segments WHERE call_id = ?1", params![call_id])?;
        tx.execute("UPDATE calls SET sealed = 1 WHERE call_id = ?1", params![call_id])?;
        tx.commit()?;
        Ok(ClientEvent::new(EventKind::SpoolEviction, now_ms)
            .with_count(lost)
            .with_detail(format!("call rejected: {reason}")))
    }

    pub fn is_sealed(&self, call_id: &str) -> Result<bool, SpoolError> {
        Ok(self
            .conn
            .query_row("SELECT sealed FROM calls WHERE call_id = ?1", params![call_id], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .unwrap_or(0)
            != 0)
    }

    /// Remove call rows whose segments are all acked and whose `call.end` has been
    /// sent. Bookkeeping only — no audio is lost here.
    pub fn vacuum_finished_calls(&mut self) -> Result<usize, SpoolError> {
        Ok(self.conn.execute(
            "DELETE FROM calls WHERE end_json IS NOT NULL
             AND call_id NOT IN (SELECT call_id FROM segments)",
            [],
        )?)
    }

    // ------------------------------------------------------------------ stats

    pub fn stats(&self) -> Result<SpoolStats, SpoolError> {
        let (segments, bytes, oldest): (i64, i64, Option<i64>) = self.conn.query_row(
            "SELECT count(*), COALESCE(sum(length(payload)), 0), min(created_ms) FROM segments",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(SpoolStats {
            segments: segments as u64,
            bytes: bytes as u64,
            oldest_created_ms: oldest.map(|v| v as u64),
        })
    }

    /// Evict oldest-first until both caps are satisfied.
    fn enforce_limits(&mut self, now_ms: u64) -> Result<Option<ClientEvent>, SpoolError> {
        let mut evicted = 0u64;

        // Age cap first: old audio is both least useful and most likely already lost
        // to the server's own retention window.
        let age_cutoff = now_ms.saturating_sub(self.limits.max_age_ms);
        evicted += self
            .conn
            .execute("DELETE FROM segments WHERE created_ms < ?1", params![age_cutoff as i64])?
            as u64;

        // Size cap: delete oldest rows until we are back under the limit.
        loop {
            let stats = self.stats()?;
            if stats.bytes <= self.limits.max_bytes {
                break;
            }
            let over = stats.bytes - self.limits.max_bytes;
            // Delete in batches proportional to the overshoot so a large burst does
            // not turn into thousands of single-row deletes.
            let batch = (over / 3_000).clamp(1, 1_000) as i64;
            let n = self.conn.execute(
                "DELETE FROM segments WHERE (call_id, channel, seq) IN (
                   SELECT call_id, channel, seq FROM segments
                   ORDER BY created_ms, call_id, channel, seq LIMIT ?1)",
                params![batch],
            )?;
            if n == 0 {
                break;
            }
            evicted += n as u64;
        }

        if evicted == 0 {
            return Ok(None);
        }
        Ok(Some(
            ClientEvent::new(EventKind::SpoolEviction, now_ms)
                .with_count(evicted)
                .with_detail("spool cap reached".into()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(call: &str, ch: Channel, seq: u32, bytes: usize, created_ms: u64) -> SegmentRow {
        SegmentRow {
            call_id: call.into(),
            channel: ch,
            seq,
            timestamp_ms: seq as u64 * 1000,
            flags: MediaFlags::default(),
            payload: vec![0xAB; bytes],
            created_ms,
        }
    }

    fn spool(limits: SpoolLimits) -> Spool {
        let s = Spool::open_in_memory(limits).unwrap();
        s.begin_call("call-1", r#"{"t":"call.start","call_id":"call-1"}"#, 0).unwrap();
        s
    }

    #[test]
    fn segments_survive_until_acked_and_not_a_moment_sooner() {
        let mut s = spool(SpoolLimits::default());
        for i in 0..10 {
            s.push(&seg("call-1", Channel::Far, i, 100, i as u64 * 1000)).unwrap();
        }
        assert_eq!(s.stats().unwrap().segments, 10);

        // call.end must not delete anything.
        s.end_call("call-1", r#"{"t":"call.end"}"#).unwrap();
        assert_eq!(s.stats().unwrap().segments, 10);

        let deleted = s.ack("call-1", Channel::Far, 4).unwrap();
        assert_eq!(deleted, 5, "cumulative ack clears 0..=4");
        assert_eq!(s.stats().unwrap().segments, 5);
        assert_eq!(s.take_pending("call-1", 100).unwrap()[0].seq, 5);
    }

    #[test]
    fn acks_are_cumulative_and_never_move_backwards() {
        let mut s = spool(SpoolLimits::default());
        for i in 0..10 {
            s.push(&seg("call-1", Channel::Near, i, 50, 0)).unwrap();
        }
        s.ack("call-1", Channel::Near, 7).unwrap();
        assert_eq!(s.stats().unwrap().segments, 2);

        // A stale ack arriving after a reconnect must not resurrect or re-send.
        s.ack("call-1", Channel::Near, 3).unwrap();
        assert_eq!(s.acked_through("call-1").unwrap()[&Channel::Near], 7);
        assert_eq!(s.take_pending("call-1", 100).unwrap().len(), 2);
    }

    #[test]
    fn channels_ack_independently() {
        let mut s = spool(SpoolLimits::default());
        for i in 0..5 {
            s.push(&seg("call-1", Channel::Far, i, 50, 0)).unwrap();
            s.push(&seg("call-1", Channel::Near, i, 50, 0)).unwrap();
        }
        s.ack("call-1", Channel::Far, 4).unwrap();
        let pending = s.take_pending("call-1", 100).unwrap();
        assert_eq!(pending.len(), 5);
        assert!(pending.iter().all(|p| p.channel == Channel::Near));
    }

    #[test]
    fn duplicate_push_is_idempotent() {
        let mut s = spool(SpoolLimits::default());
        s.push(&seg("call-1", Channel::Far, 3, 100, 0)).unwrap();
        s.push(&seg("call-1", Channel::Far, 3, 100, 0)).unwrap();
        assert_eq!(s.stats().unwrap().segments, 1);
    }

    #[test]
    fn size_cap_evicts_oldest_first_and_reports_the_loss() {
        let limits = SpoolLimits { max_bytes: 1_000, max_age_ms: u64::MAX };
        let mut s = spool(limits);
        let mut last_event = None;
        for i in 0..30 {
            if let Some(ev) = s.push(&seg("call-1", Channel::Far, i, 100, i as u64)).unwrap() {
                last_event = Some(ev);
            }
        }
        let stats = s.stats().unwrap();
        assert!(stats.bytes <= 1_000, "cap not honoured: {stats:?}");

        let ev = last_event.expect("eviction must emit an event");
        assert_eq!(ev.kind, EventKind::SpoolEviction);
        assert!(ev.count.unwrap() > 0);

        // Oldest-first: the surviving sequences are the highest ones.
        let pending = s.take_pending("call-1", 100).unwrap();
        let seqs: Vec<u32> = pending.iter().map(|p| p.seq).collect();
        assert_eq!(seqs.last(), Some(&29));
        assert!(seqs[0] > 0, "the oldest segments should be the ones gone: {seqs:?}");
    }

    #[test]
    fn age_cap_evicts_and_reports() {
        let limits = SpoolLimits { max_bytes: u64::MAX, max_age_ms: 10_000 };
        let mut s = spool(limits);
        s.push(&seg("call-1", Channel::Far, 0, 10, 0)).unwrap();
        s.push(&seg("call-1", Channel::Far, 1, 10, 5_000)).unwrap();
        let ev = s.push(&seg("call-1", Channel::Far, 2, 10, 20_000)).unwrap();
        let ev = ev.expect("expired segments must emit an eviction event");
        assert_eq!(ev.count, Some(2));
        assert_eq!(s.stats().unwrap().segments, 1);
    }

    #[test]
    fn a_full_spool_still_accepts_the_newest_audio() {
        // The failure mode we must avoid is a full spool that silently stops
        // recording: eviction makes room for the call happening now.
        let limits = SpoolLimits { max_bytes: 500, max_age_ms: u64::MAX };
        let mut s = spool(limits);
        for i in 0..50 {
            s.push(&seg("call-1", Channel::Far, i, 100, i as u64)).unwrap();
        }
        let pending = s.take_pending("call-1", 100).unwrap();
        assert!(pending.iter().any(|p| p.seq == 49), "newest segment must be retained");
    }

    #[test]
    fn restart_recovers_pending_calls_in_age_order() {
        let mut s = Spool::open_in_memory(SpoolLimits::default()).unwrap();
        s.begin_call("call-old", "{}", 0).unwrap();
        s.begin_call("call-new", "{}", 100).unwrap();
        s.push(&seg("call-new", Channel::Far, 0, 10, 500)).unwrap();
        s.push(&seg("call-old", Channel::Far, 0, 10, 100)).unwrap();
        assert_eq!(s.pending_calls().unwrap(), vec!["call-old", "call-new"]);
    }

    #[test]
    fn sealing_a_rejected_call_reports_exactly_what_was_lost() {
        let mut s = spool(SpoolLimits::default());
        for i in 0..6 {
            s.push(&seg("call-1", Channel::Far, i, 10, 0)).unwrap();
        }
        let ev = s.seal("call-1", "tenant_mismatch", 12345).unwrap();
        assert_eq!(ev.count, Some(6));
        assert_eq!(s.stats().unwrap().segments, 0);
        assert!(s.is_sealed("call-1").unwrap());
        assert!(s.push(&seg("call-1", Channel::Far, 9, 10, 0)).is_err());
        assert!(s.pending_calls().unwrap().is_empty());
    }

    #[test]
    fn control_frames_replay_verbatim_after_a_reconnect() {
        let s = spool(SpoolLimits::default());
        let start = r#"{"t":"call.start","call_id":"call-1","started_at":"2026-09-01T10:14:02.113Z"}"#;
        s.begin_call("call-1", start, 0).unwrap();
        // A second begin_call (the reconnect path) must not overwrite the original.
        s.begin_call("call-1", r#"{"t":"call.start","started_at":"later"}"#, 999).unwrap();
        assert_eq!(s.call_start_json("call-1").unwrap().unwrap(), r#"{"t":"call.start","call_id":"call-1"}"#);
        let _ = start;
    }

    #[test]
    fn finished_calls_are_vacuumed_but_unacked_ones_are_kept() {
        let mut s = Spool::open_in_memory(SpoolLimits::default()).unwrap();
        s.begin_call("done", "{}", 0).unwrap();
        s.begin_call("pending", "{}", 0).unwrap();
        s.push(&seg("done", Channel::Far, 0, 10, 0)).unwrap();
        s.push(&seg("pending", Channel::Far, 0, 10, 0)).unwrap();
        s.end_call("done", "{}").unwrap();
        s.end_call("pending", "{}").unwrap();
        s.ack("done", Channel::Far, 0).unwrap();
        assert_eq!(s.vacuum_finished_calls().unwrap(), 1);
        assert!(s.call_start_json("pending").unwrap().is_some());
    }
}
