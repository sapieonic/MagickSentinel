"""Object storage: the key layout, and the two backends.

The first test here is the important one. The gateway writes segment objects and the
pipeline reads them, and the only thing the two halves must agree on is the key. A
mismatch is silent — every call finds no audio and is marked failed, which looks
exactly like an ASR outage — so the format string is compared against the Go source
that defines it, in the same spirit as the shared wire fixture the Rust and Go codecs
both read.
"""

import re
from pathlib import Path

import pytest

from sentinel_pipeline.blobstore import (
    GO_SEGMENT_KEY_FORMAT,
    DirBlobStore,
    MemoryBlobStore,
    S3BlobStore,
    blob_store_from_env,
    day_prefix,
    segment_key,
    tenant_prefix,
)

BLOB_GO = (Path(__file__).resolve().parents[2]
           / "gateway" / "internal" / "blob" / "blob.go")


def test_the_key_format_still_matches_the_gateways_go_source():
    # Read the authoritative definition rather than trusting a copied comment. If
    # SegmentKey is reshaped in Go, this fails here rather than in production as a
    # floor whose calls all fail transcription with no audio found.
    source = BLOB_GO.read_text(encoding="utf-8")
    match = re.search(r'fmt\.Sprintf\("([^"]+)",\s*tenantID,\s*day,\s*callID,\s*channel,\s*seq\)',
                      source)
    assert match, f"could not find SegmentKey's format string in {BLOB_GO}"
    assert match.group(1) == GO_SEGMENT_KEY_FORMAT
    # And the Python builder produces what that format string would.
    assert segment_key("t", "2026-09-01", "01J8", 1, 42) == \
        GO_SEGMENT_KEY_FORMAT % ("t", "2026-09-01", "01J8", 1, 42)


def test_the_sequence_number_is_zero_padded_to_eight_digits():
    # %08d, so keys sort lexically in sequence order. A reader that lists a prefix
    # and concatenates in listing order depends on that.
    key = segment_key("t", "2026-09-01", "call", 0, 7)
    assert key.endswith("/00000007.opus")
    assert sorted([segment_key("t", "d", "c", 0, 9), segment_key("t", "d", "c", 0, 10)]) == \
        [segment_key("t", "d", "c", 0, 9), segment_key("t", "d", "c", 0, 10)]


def test_prefixes_are_the_partitions_the_retention_sweep_deletes_by():
    assert tenant_prefix("t1") == "audio/t1/"
    assert day_prefix("t1", "2026-09-01") == "audio/t1/2026-09-01/"
    assert segment_key("t1", "2026-09-01", "c", 0, 0).startswith(day_prefix("t1", "2026-09-01"))


# ---------------------------------------------------------------------- backends


@pytest.fixture(params=["memory", "dir"])
def store(request, tmp_path):
    return MemoryBlobStore() if request.param == "memory" else DirBlobStore(str(tmp_path))


def _put(store, key, body=b"x"):
    if isinstance(store, MemoryBlobStore):
        store.objects[key] = body
    else:
        path = Path(store.root) / key
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(body)


def test_a_missing_object_reads_as_none_rather_than_raising(store):
    # Absence is normal: the gateway writes the object before the row, and retention
    # deletes the object before the row, so both orders leave a window.
    assert store.get("audio/t/2026-09-01/c/00000000.opus") is None


def test_round_trip_and_delete(store):
    key = segment_key("t", "2026-09-01", "c", 0, 1)
    _put(store, key, b"opus-bytes")
    assert store.get(key) == b"opus-bytes"
    store.delete(key)
    assert store.get(key) is None


def test_deleting_a_missing_object_is_not_an_error(store):
    # Retention retries; the second attempt must not fail the run.
    store.delete(segment_key("t", "2026-09-01", "c", 0, 1))


def test_day_prefixes_and_prefix_delete(store):
    for day in ("2026-08-01", "2026-08-02"):
        _put(store, segment_key("t", day, "c", 0, 0))
        _put(store, segment_key("t", day, "c", 1, 0))
    _put(store, segment_key("other", "2026-08-01", "c", 0, 0))

    assert store.day_prefixes(tenant_prefix("t")) == [
        "audio/t/2026-08-01/", "audio/t/2026-08-02/"]

    assert store.delete_prefix(day_prefix("t", "2026-08-01")) == 2
    assert store.day_prefixes(tenant_prefix("t")) == ["audio/t/2026-08-02/"]
    # Another tenant's audio is untouched by a prefix delete scoped to this one.
    assert store.get(segment_key("other", "2026-08-01", "c", 0, 0)) is not None


def test_a_directory_store_refuses_a_key_that_escapes_its_root(tmp_path):
    store = DirBlobStore(str(tmp_path / "blobs"))
    with pytest.raises(ValueError):
        store.get("../../etc/passwd")


# ------------------------------------------------------------------------- S3


class FakeS3:
    """The three S3 calls the store makes, and nothing else."""

    def __init__(self, objects=None):
        self.objects = dict(objects or {})
        self.deleted = []

    def get_object(self, Bucket, Key):  # noqa: N803 - boto3's own casing
        if Key not in self.objects:
            raise _NoSuchKey()
        return {"Body": _Body(self.objects[Key])}

    def delete_object(self, Bucket, Key):  # noqa: N803
        self.objects.pop(Key, None)
        self.deleted.append(Key)

    def delete_objects(self, Bucket, Delete):  # noqa: N803
        for entry in Delete["Objects"]:
            self.objects.pop(entry["Key"], None)
            self.deleted.append(entry["Key"])
        return {}

    def get_paginator(self, name):
        return _Paginator(self)


class _NoSuchKey(Exception):
    response = {"Error": {"Code": "NoSuchKey"}}


class _Body:
    def __init__(self, data):
        self._data = data

    def read(self):
        return self._data


class _Paginator:
    def __init__(self, client):
        self._client = client

    def paginate(self, Bucket, Prefix, Delimiter=None):  # noqa: N803
        keys = sorted(k for k in self._client.objects if k.startswith(Prefix))
        if Delimiter is None:
            return [{"Contents": [{"Key": k} for k in keys]}]
        commons = sorted({Prefix + k[len(Prefix):].split(Delimiter, 1)[0] + Delimiter
                          for k in keys if Delimiter in k[len(Prefix):]})
        return [{"CommonPrefixes": [{"Prefix": p} for p in commons]}]


def test_s3_reads_writes_and_lists_under_an_optional_bucket_prefix():
    key = segment_key("t", "2026-08-01", "c", 0, 3)
    client = FakeS3({f"sentinel/{key}": b"audio"})
    store = S3BlobStore(bucket="b", prefix="sentinel/", client=client)

    assert store.get(key) == b"audio"
    assert store.get(segment_key("t", "2026-08-01", "c", 0, 4)) is None
    # The prefix is stripped back off, so callers only ever see canonical keys.
    assert store.day_prefixes(tenant_prefix("t")) == ["audio/t/2026-08-01/"]
    assert store.delete_prefix(day_prefix("t", "2026-08-01")) == 1
    assert store.get(key) is None


def test_s3_errors_other_than_absence_propagate():
    class Angry(FakeS3):
        def get_object(self, Bucket, Key):  # noqa: N803
            raise _Denied()

    class _Denied(Exception):
        response = {"Error": {"Code": "AccessDenied"}}

    store = S3BlobStore(bucket="b", client=Angry())
    # Treating a permissions error as "no audio" would mark the call failed and throw
    # away the retry, so it has to escape.
    with pytest.raises(Exception):
        store.get("audio/t/d/c/00000000.opus")


def test_s3_deletes_are_batched_to_the_api_limit():
    keys = {segment_key("t", "2026-08-01", "c", 0, i): b"x" for i in range(2_500)}
    client = FakeS3(keys)
    store = S3BlobStore(bucket="b", client=client, delete_batch=1_000)
    assert store.delete_prefix(day_prefix("t", "2026-08-01")) == 2_500


# ---------------------------------------------------------------- env selection


def test_the_bucket_selects_s3_and_the_directory_selects_dev(tmp_path, monkeypatch):
    monkeypatch.setattr("sentinel_pipeline.blobstore.S3BlobStore.__post_init__",
                        lambda self: None)
    s3 = blob_store_from_env({"SENTINEL_BLOB_BUCKET": "b"})
    assert isinstance(s3, S3BlobStore) and s3.region == "ap-south-1"
    assert isinstance(blob_store_from_env({"SENTINEL_BLOB_DIR": str(tmp_path)}),
                      DirBlobStore)


def test_no_storage_configured_refuses_to_guess():
    # main.go takes the same line: a pipeline pointed at nothing transcribes nothing
    # and reports it as an ASR problem.
    with pytest.raises(RuntimeError, match="SENTINEL_BLOB_DIR"):
        blob_store_from_env({})
