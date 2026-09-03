"""Retention: the dry run, the day-prefix sweep, and the entrypoint's safety catch.

``tests/test_retention.py`` pins the deleting loop and its boundaries. This file
covers the parts added when the job was wired to a real database and a real object
store, all three of which are about not destroying evidence:

* the dry run, which is what the scheduler gets unless someone opts in explicitly,
* the day-prefix sweep, which is the only thing that can ever find an object whose
  row is already gone — and the margin that stops it deleting a day whose rows are
  still live,
* and ``run_retention``, which will not delete anything without
  ``SENTINEL_RETENTION_COMMIT=1``.
"""

from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone

from fakedb import FakeDatabase

from sentinel_pipeline.blobstore import MemoryBlobStore, segment_key
from sentinel_pipeline.retention import RetentionJob
from sentinel_pipeline.service import run_retention

NOW = datetime(2026, 9, 1, 2, 0, tzinfo=timezone.utc)
TENANT = "t1"


@dataclass
class CountingStore:
    """A store that can count as well as delete, as Postgres can."""

    segments: list = field(default_factory=list)   # (call, channel, seq, key, created)
    transcripts: list = field(default_factory=list)
    audits: list = field(default_factory=list)
    tenant_rows: list = field(default_factory=lambda: [(TENANT, 30, 365)])
    calls: list = field(default_factory=list)

    def tenants(self):
        return self.tenant_rows

    def audio_keys_before(self, tenant_id, cutoff, limit):
        self.calls.append("audio_keys_before")
        return [(c, ch, sq, k) for (c, ch, sq, k, created) in self.segments
                if created < cutoff][:limit]

    def count_audio_before(self, tenant_id, cutoff):
        self.calls.append("count_audio_before")
        return len([s for s in self.segments if s[4] < cutoff])

    def delete_media_rows(self, tenant_id, keys):
        self.calls.append("delete_media_rows")
        wanted = set(keys)
        before = len(self.segments)
        self.segments = [s for s in self.segments if (s[0], s[1], s[2]) not in wanted]
        return before - len(self.segments)

    def delete_transcripts_before(self, tenant_id, cutoff, limit):
        self.calls.append("delete_transcripts_before")
        stale = [t for t in self.transcripts if t < cutoff][:limit]
        for t in stale:
            self.transcripts.remove(t)
        return len(stale)

    def count_transcripts_before(self, tenant_id, cutoff):
        self.calls.append("count_transcripts_before")
        return len([t for t in self.transcripts if t < cutoff])

    def audit(self, tenant_id, action, detail):
        self.calls.append("audit")
        self.audits.append((tenant_id, action, detail))


@dataclass
class PrefixBlob(MemoryBlobStore):
    """An object store that supports the prefix operations, and records the order."""

    calls: list = field(default_factory=list)
    fail_prefixes: bool = False

    def delete(self, key):
        self.calls.append(("delete", key))
        super().delete(key)

    def day_prefixes(self, prefix):
        self.calls.append(("day_prefixes", prefix))
        if self.fail_prefixes:
            raise OSError("object store unavailable")
        return super().day_prefixes(prefix)

    def delete_prefix(self, prefix):
        self.calls.append(("delete_prefix", prefix))
        if self.fail_prefixes:
            raise OSError("object store unavailable")
        return super().delete_prefix(prefix)


def seg(n, age_days, key=None):
    return (f"call-{n}", 0, n, key or f"audio/{TENANT}/key-{n}",
            NOW - timedelta(days=age_days))


# ------------------------------------------------------------------- the dry run


def test_a_dry_run_deletes_nothing_and_counts_everything():
    store = CountingStore(segments=[seg(1, 40), seg(2, 31), seg(3, 1)],
                          transcripts=[NOW - timedelta(days=400)])
    blob = PrefixBlob()
    result = RetentionJob(store=store, blob=blob, now=NOW,
                          dry_run=True).purge_tenant(TENANT, 30, 365)

    assert result.dry_run is True
    assert result.audio_segments == 2 and result.transcripts == 1
    assert blob.calls == [], "a dry run must not touch object storage"
    assert len(store.segments) == 3 and len(store.transcripts) == 1
    assert "delete_media_rows" not in store.calls


def test_a_dry_run_still_records_that_it_ran_and_marks_itself():
    store = CountingStore(segments=[seg(1, 40)])
    RetentionJob(store=store, blob=PrefixBlob(), now=NOW,
                 dry_run=True).purge_tenant(TENANT, 30, 365)

    tenant, action, detail = store.audits[0]
    # A preview entry that could be mistaken for a purge would be worse than none:
    # "retention removed 40,000 segments last night" has to mean it happened.
    assert action == "retention.preview" and detail["dry_run"] is True
    assert tenant == TENANT


def test_a_dry_run_counts_rather_than_looping_over_batches():
    # The deleting loop terminates because deleted rows stop coming back. With
    # nothing deleted it would fetch the same batch forever, so the preview must not
    # use it — this test would hang rather than fail if it did.
    store = CountingStore(segments=[seg(i, 40) for i in range(50)])
    result = RetentionJob(store=store, blob=PrefixBlob(), now=NOW, batch_size=10,
                          dry_run=True).purge_tenant(TENANT, 30, 365)
    assert result.audio_segments == 50
    assert store.calls.count("audio_keys_before") == 0


def test_run_covers_every_tenant_in_dry_run_too():
    store = CountingStore(segments=[seg(1, 40)],
                          tenant_rows=[("t1", 30, 365), ("t2", 7, 30)])
    results = RetentionJob(store=store, blob=PrefixBlob(), now=NOW, dry_run=True).run()
    assert [r.tenant_id for r in results] == ["t1", "t2"]
    assert all(r.dry_run for r in results)


# -------------------------------------------------------------- the prefix sweep


def blob_with_days(*days):
    store = PrefixBlob()
    for day in days:
        store.objects[segment_key(TENANT, day, "orphan", 0, 0)] = b"x"
    return store


def test_a_day_entirely_past_the_cutoff_is_deleted_by_prefix():
    # This is what blob.SegmentKey's date partitioning exists for, and the only way
    # an object whose row has already gone is ever found again: the gateway writes
    # the object before the row, so a crash between the two leaves exactly this.
    store = CountingStore()
    blob = blob_with_days("2026-07-01", "2026-07-02")
    result = RetentionJob(store=store, blob=blob, now=NOW).purge_tenant(TENANT, 30, 365)

    assert result.swept_days == 2 and result.swept_objects == 2
    assert blob.objects == {}


def test_the_sweep_stays_a_day_behind_the_row_driven_purge():
    # A key's day comes from the gateway's clock at upload; the row's received_at from
    # the database's. They agree to within a skew, not exactly, and a prefix delete is
    # not recoverable — so the day the cutoff falls in, and the one before it, are
    # left to the row-driven purge.
    cutoff_day = (NOW - timedelta(days=30)).date().isoformat()      # 2026-08-02
    margin_day = (NOW - timedelta(days=31)).date().isoformat()      # 2026-08-01
    older_day = (NOW - timedelta(days=32)).date().isoformat()       # 2026-07-31
    blob = blob_with_days(cutoff_day, margin_day, older_day)

    result = RetentionJob(store=CountingStore(), blob=blob,
                          now=NOW).purge_tenant(TENANT, 30, 365)
    assert result.swept_days == 1
    remaining = sorted(blob.day_prefixes(f"audio/{TENANT}/"))
    assert remaining == [f"audio/{TENANT}/{margin_day}/", f"audio/{TENANT}/{cutoff_day}/"]


def test_a_day_inside_the_retention_window_is_never_swept():
    recent = (NOW - timedelta(days=2)).date().isoformat()
    blob = blob_with_days(recent)
    result = RetentionJob(store=CountingStore(), blob=blob,
                          now=NOW).purge_tenant(TENANT, 30, 365)
    assert result.swept_days == 0 and len(blob.objects) == 1


def test_the_sweep_runs_after_the_rows_have_had_their_objects_deleted():
    # The other order would delete objects whose rows are still present, turning a
    # purge into a set of calls that point at audio that is not there.
    key = segment_key(TENANT, "2026-07-01", "call-1", 0, 0)
    store = CountingStore(segments=[("call-1", 0, 0, key, NOW - timedelta(days=40))])
    blob = PrefixBlob({key: b"x"})
    RetentionJob(store=store, blob=blob, now=NOW).purge_tenant(TENANT, 30, 365)

    kinds = [c[0] for c in blob.calls]
    assert kinds.index("delete") < kinds.index("day_prefixes")


def test_a_store_that_cannot_delete_by_prefix_still_purges_from_the_rows():
    # The row-driven purge is the correctness baseline; the sweep is on top of it.
    class KeyOnlyBlob:
        def __init__(self):
            self.deleted = []

        def delete(self, key):
            self.deleted.append(key)

    store = CountingStore(segments=[seg(1, 40)])
    blob = KeyOnlyBlob()
    result = RetentionJob(store=store, blob=blob, now=NOW).purge_tenant(TENANT, 30, 365)
    assert result.audio_segments == 1 and result.swept_days == 0
    assert blob.deleted == [f"audio/{TENANT}/key-1"]


def test_an_unexpected_prefix_is_left_alone_rather_than_deleted():
    blob = PrefixBlob({f"audio/{TENANT}/not-a-day/thing": b"x"})
    result = RetentionJob(store=CountingStore(), blob=blob,
                          now=NOW).purge_tenant(TENANT, 30, 365)
    assert result.swept_days == 0
    assert blob.objects, "something unrecognised under a tenant's audio is not ours to delete"


def test_a_sweep_failure_is_recorded_and_does_not_fail_the_purge():
    store = CountingStore(transcripts=[NOW - timedelta(days=400)])
    blob = blob_with_days("2026-07-01")
    blob.fail_prefixes = True
    result = RetentionJob(store=store, blob=blob, now=NOW).purge_tenant(TENANT, 30, 365)
    assert result.errors and result.transcripts == 1


def test_the_sweep_does_not_run_in_a_dry_run():
    blob = blob_with_days("2026-07-01")
    result = RetentionJob(store=CountingStore(), blob=blob, now=NOW,
                          dry_run=True).purge_tenant(TENANT, 30, 365)
    assert result.swept_days == 0 and blob.calls == []


def test_the_audit_entry_still_carries_counts_only_after_the_sweep():
    store = CountingStore(segments=[seg(1, 40)])
    blob = blob_with_days("2026-07-01")
    RetentionJob(store=store, blob=blob, now=NOW).purge_tenant(TENANT, 30, 365)
    detail = store.audits[0][2]
    assert detail["swept_days"] == 1
    rendered = " ".join(str(v) for v in detail.values())
    assert "audio/" not in rendered and "call-" not in rendered


# ------------------------------------------------------------------ entrypoint


def scripted_db(audio: int, transcripts: int) -> FakeDatabase:
    db = FakeDatabase()
    db.on("sentinel_pipeline_tenants", [(TENANT, 30, 365, "Asia/Kolkata")])
    db.on("count(*) FROM media_segments", [(audio,)])
    db.on("count(*) FROM transcripts", [(transcripts,)])
    return db


def test_the_entrypoint_is_a_dry_run_unless_commit_is_asked_for():
    # The job has never deleted anything in this repository's history, so the first
    # real run is against a customer's evidence. It should have to be requested.
    db = scripted_db(audio=5, transcripts=2)
    blob = PrefixBlob()
    assert run_retention({}, db=db, blob=blob) == 0

    assert blob.calls == []
    assert not db.sql_for("DELETE FROM media_segments")
    preview = db.sql_for("INSERT INTO audit_log")[0]
    assert preview.params[1] == "retention.preview"


def test_the_entrypoint_deletes_when_told_to():
    db = FakeDatabase()
    db.on("sentinel_pipeline_tenants", [(TENANT, 30, 365, "Asia/Kolkata")])
    key = segment_key(TENANT, "2026-07-01", "call-1", 0, 0)
    db.on("FROM media_segments WHERE tenant_id", [("0192f3e0-1234-4567-89ab-cdef01234567",
                                                   0, 0, key)])
    db.on("DELETE FROM media_segments", rowcount=1)
    db.on("DELETE FROM transcripts", rowcount=0)
    blob = PrefixBlob({key: b"x"})

    assert run_retention({"SENTINEL_RETENTION_COMMIT": "1"}, db=db, blob=blob) == 0
    assert ("delete", key) in blob.calls
    assert db.sql_for("INSERT INTO audit_log")[0].params[1] == "retention.purge"


def test_the_entrypoint_reports_a_non_zero_exit_when_a_tenant_had_errors():
    # The exit code is the contract with the scheduler: a cron that ignores stdout
    # still notices.
    db = FakeDatabase()
    db.on("sentinel_pipeline_tenants", [(TENANT, 30, 365, "Asia/Kolkata")])
    db.on("FROM media_segments WHERE tenant_id", [("0192f3e0-1234-4567-89ab-cdef01234567",
                                                   0, 0, "audio/t1/2026-07-01/c/0.opus")])
    blob = PrefixBlob()
    blob.fail_prefixes = True

    class Angry(PrefixBlob):
        def delete(self, key):
            raise OSError("object store unavailable")

    assert run_retention({"SENTINEL_RETENTION_COMMIT": "1"}, db=db, blob=Angry()) == 1
