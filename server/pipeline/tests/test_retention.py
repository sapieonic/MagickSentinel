"""Retention purge.

The failure mode here is deleting data that should have been kept, or keeping data
that should have gone — either of which is a reportable incident in a product sold on
compliance. These tests pin the ordering and the boundaries.
"""

from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone

import pytest

from sentinel_pipeline.retention import RetentionJob

NOW = datetime(2026, 9, 1, 2, 0, tzinfo=timezone.utc)


@dataclass
class FakeStore:
    """Holds segments and transcripts with their ages, and records what was deleted."""

    segments: list = field(default_factory=list)   # (call_id, channel, seq, key, created)
    transcripts: list = field(default_factory=list)  # created datetimes
    audits: list = field(default_factory=list)
    tenant_rows: list = field(default_factory=lambda: [("t1", 30, 365)])

    def tenants(self):
        return self.tenant_rows

    def audio_keys_before(self, tenant_id, cutoff, limit):
        return [(c, ch, sq, k) for (c, ch, sq, k, created) in self.segments
                if created < cutoff][:limit]

    def delete_media_rows(self, tenant_id, keys):
        wanted = set(keys)
        before = len(self.segments)
        self.segments = [s for s in self.segments if (s[0], s[1], s[2]) not in wanted]
        return before - len(self.segments)

    def delete_transcripts_before(self, tenant_id, cutoff, limit):
        stale = [t for t in self.transcripts if t < cutoff][:limit]
        for t in stale:
            self.transcripts.remove(t)
        return len(stale)

    def audit(self, tenant_id, action, detail):
        self.audits.append((tenant_id, action, detail))


@dataclass
class FakeBlob:
    deleted: list = field(default_factory=list)
    fail_on: set = field(default_factory=set)

    def delete(self, key):
        if key in self.fail_on:
            raise OSError("object store unavailable")
        self.deleted.append(key)


def seg(n: int, age_days: int, tenant_created=NOW):
    return (f"call-{n}", 0, n, f"audio/t1/key-{n}", tenant_created - timedelta(days=age_days))


def build(store: FakeStore, blob: FakeBlob, batch_size: int = 1_000) -> RetentionJob:
    return RetentionJob(store=store, blob=blob, batch_size=batch_size, now=NOW)


def test_audio_older_than_the_window_is_purged_and_newer_audio_is_kept():
    store = FakeStore(segments=[seg(1, 40), seg(2, 31), seg(3, 29), seg(4, 1)])
    blob = FakeBlob()
    result = build(store, blob).purge_tenant("t1", audio_days=30, transcript_days=365)

    assert result.audio_segments == 2
    assert sorted(blob.deleted) == ["audio/t1/key-1", "audio/t1/key-2"]
    assert [s[0] for s in store.segments] == ["call-3", "call-4"]


def test_transcripts_outlive_audio():
    # The asymmetry is the point: the recording is the sensitive artifact, the
    # transcript is what the compliance record needs a year later.
    store = FakeStore(
        segments=[seg(1, 40)],
        transcripts=[NOW - timedelta(days=40), NOW - timedelta(days=400)],
    )
    result = build(store, FakeBlob()).purge_tenant("t1", audio_days=30, transcript_days=365)
    assert result.audio_segments == 1
    assert result.transcripts == 1, "the 40-day-old transcript must survive an audio purge"
    assert len(store.transcripts) == 1


def test_a_blob_delete_failure_leaves_the_row_for_the_next_run():
    # Deleting the row while the object survives orphans audio that no later sweep
    # can find — the worst outcome available here, because it is invisible.
    store = FakeStore(segments=[seg(1, 40), seg(2, 40)])
    blob = FakeBlob(fail_on={"audio/t1/key-1"})
    result = build(store, blob).purge_tenant("t1", audio_days=30, transcript_days=365)

    assert result.audio_segments == 1
    assert len(result.errors) == 1
    assert [s[0] for s in store.segments] == ["call-1"], \
        "the row whose object survived must remain, so the next run retries it"


def test_purging_is_batched_and_terminates():
    store = FakeStore(segments=[seg(i, 40) for i in range(25)])
    blob = FakeBlob()
    result = build(store, blob, batch_size=10).purge_tenant("t1", 30, 365)
    assert result.audio_segments == 25
    assert store.segments == []


def test_a_persistent_blob_failure_does_not_loop_forever():
    # Every object in the batch fails, so no row is deleted and the next fetch
    # returns the same batch. Without the short-batch break this spins.
    store = FakeStore(segments=[seg(i, 40) for i in range(5)])
    blob = FakeBlob(fail_on={f"audio/t1/key-{i}" for i in range(5)})
    result = build(store, blob, batch_size=10).purge_tenant("t1", 30, 365)
    assert result.audio_segments == 0
    assert len(result.errors) == 5
    assert len(store.segments) == 5


def test_every_run_writes_one_audit_entry_carrying_counts_only():
    store = FakeStore(segments=[seg(1, 40)], transcripts=[NOW - timedelta(days=400)])
    build(store, FakeBlob()).purge_tenant("t1", 30, 365)

    assert len(store.audits) == 1
    tenant, action, detail = store.audits[0]
    assert tenant == "t1" and action == "retention.purge"
    assert detail["audio_segments"] == 1 and detail["transcripts"] == 1
    assert detail["audio_cutoff"] == "2026-08-02"
    # Counts and cutoffs, never identifiers. The audit log is not a place to leak the
    # data the purge just removed.
    blob = " ".join(str(v) for v in detail.values())
    assert "call-" not in blob and "audio/" not in blob


def test_a_run_that_purges_nothing_still_records_that_it_ran():
    # An operator asking "did retention run last night" needs an answer even on a
    # quiet night; silence is indistinguishable from a broken cron.
    store = FakeStore(segments=[seg(1, 1)])
    result = build(store, FakeBlob()).purge_tenant("t1", 30, 365)
    assert result.audio_segments == 0
    assert len(store.audits) == 1


def test_retention_periods_are_per_tenant():
    store = FakeStore(
        segments=[seg(1, 10)],
        tenant_rows=[("t1", 7, 30), ("t2", 90, 365)],
    )
    job = build(store, FakeBlob())
    strict = job.purge_tenant("t1", 7, 30)
    assert strict.audio_segments == 1, "a 7-day tenant purges 10-day-old audio"

    store.segments = [seg(2, 10)]
    lenient = job.purge_tenant("t2", 90, 365)
    assert lenient.audio_segments == 0, "a 90-day tenant keeps it"


def test_run_covers_every_tenant():
    store = FakeStore(
        segments=[("c1", 0, 0, "k1", NOW - timedelta(days=40))],
        tenant_rows=[("t1", 30, 365), ("t2", 30, 365)],
    )
    results = build(store, FakeBlob()).run()
    assert [r.tenant_id for r in results] == ["t1", "t2"]
    assert len(store.audits) == 2


def test_the_cutoff_boundary_keeps_data_exactly_at_the_limit():
    # Off-by-one here deletes a day early, every day, for every tenant.
    store = FakeStore(segments=[
        ("c-at", 0, 0, "k-at", NOW - timedelta(days=30)),
        ("c-past", 0, 1, "k-past", NOW - timedelta(days=30, seconds=1)),
    ])
    blob = FakeBlob()
    build(store, blob).purge_tenant("t1", 30, 365)
    assert blob.deleted == ["k-past"]
    assert [s[0] for s in store.segments] == ["c-at"]
