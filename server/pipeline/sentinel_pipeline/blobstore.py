"""Object storage, from the reading end.

The gateway writes call audio; this module reads it back and deletes it when it
expires. Both halves have to agree on exactly one thing — the key layout — and the
authoritative statement of it is Go:

    // server/gateway/internal/blob/blob.go
    func SegmentKey(tenantID, day, callID string, channel uint8, seq uint32) string {
        return fmt.Sprintf("audio/%s/%s/%s/%d/%08d.opus", tenantID, day, callID, channel, seq)
    }

A mismatch here does not fail loudly. It produces a pipeline that finds no audio for
any call, marks every call ``failed``, and looks like an ASR outage. So the format
string is kept next to a copy of the Go one and ``tests/test_blobstore.py`` reads
``blob.go`` and asserts the two are still character-for-character identical, in the
same spirit as ``contracts/fixtures/wire_vectors.json``: two implementations of one
format that cannot drift without a test failing.

Two backends, the same split the gateway has: S3 for production and a local
directory for development. Nothing here is the source of truth for *which* objects
exist — that is ``media_segments.s3_key``, written by the gateway in the same
transaction shape as the audio itself (see :mod:`sentinel_pipeline.persistence`).
The key builder below exists for the two jobs that work by *prefix* rather than by
row: the day-partitioned retention sweep, and finding objects whose row is already
gone.
"""

from __future__ import annotations

import logging
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, Protocol

log = logging.getLogger(__name__)

#: Verbatim copy of the Go format string, for the drift test to compare against.
#: Do not "tidy" this: it is a fixture, not a template this module formats with.
GO_SEGMENT_KEY_FORMAT = "audio/%s/%s/%s/%d/%08d.opus"


def segment_key(tenant_id: str, day: str, call_id: str, channel: int, seq: int) -> str:
    """The canonical object key for one 1-second segment.

    ``day`` is the UTC upload day as ``YYYY-MM-DD``, stamped by the gateway when the
    segment arrived rather than derived from the call's start: a call that runs over
    midnight has segments under two prefixes, and both are correct. That is why the
    row's ``s3_key`` is authoritative for reads and this function is only used for
    prefix work.
    """
    return f"audio/{tenant_id}/{day}/{call_id}/{channel}/{seq:08d}.opus"


def tenant_prefix(tenant_id: str) -> str:
    return f"audio/{tenant_id}/"


def day_prefix(tenant_id: str, day: str) -> str:
    """The prefix holding one tenant's audio for one UTC day.

    The entire reason the key is date-partitioned (OPEN-6, and the comment on
    ``blob.SegmentKey``): a retention sweep can delete a day in one operation instead
    of issuing one delete per second of every call.
    """
    return f"audio/{tenant_id}/{day}/"


class BlobStore(Protocol):
    """The object-storage surface the pipeline needs.

    Narrower than the gateway's ``blob.Store``: the pipeline never writes audio. It
    reads it once per call and deletes it once, forever, at the end of retention.
    """

    def get(self, key: str) -> bytes | None:
        """The object, or ``None`` if it is not there.

        Absence is a normal outcome rather than an error: the gateway writes the
        object before the row, so a crash between the two leaves a row-less object,
        and a retention sweep that has already run leaves an object-less row for as
        long as the delete of the row is still pending.
        """

    def delete(self, key: str) -> None: ...

    def delete_prefix(self, prefix: str) -> int:
        """Delete everything under ``prefix``; returns the number of objects removed."""

    def day_prefixes(self, prefix: str) -> list[str]:
        """The immediate child prefixes of ``prefix`` — i.e. the days present."""


@dataclass
class MemoryBlobStore:
    """In-memory store, for tests. Mirrors ``blob.Memory`` in the gateway."""

    objects: dict[str, bytes] = field(default_factory=dict)

    def get(self, key: str) -> bytes | None:
        return self.objects.get(key)

    def delete(self, key: str) -> None:
        self.objects.pop(key, None)

    def delete_prefix(self, prefix: str) -> int:
        doomed = [k for k in self.objects if k.startswith(prefix)]
        for key in doomed:
            del self.objects[key]
        return len(doomed)

    def day_prefixes(self, prefix: str) -> list[str]:
        days = {k[len(prefix):].split("/", 1)[0] for k in self.objects if k.startswith(prefix)}
        return sorted(f"{prefix}{day}/" for day in days if day)


@dataclass
class DirBlobStore:
    """Filesystem-backed store, for development without MinIO.

    The gateway's ``blob.Dir`` is the writer; this is the reader. Keys are joined onto
    the root with the same ``filepath.FromSlash`` semantics, so the two see the same
    files against the same ``SENTINEL_BLOB_DIR``.
    """

    root: str

    def _path(self, key: str) -> Path:
        # Keys are built by this package and by the gateway, never by a request, so
        # they cannot contain traversal — but a corrupt row could, and the cost of
        # refusing is one comparison against a delete that would leave the tree.
        path = (Path(self.root) / key).resolve()
        root = Path(self.root).resolve()
        if root not in path.parents and path != root:
            raise ValueError(f"blobstore: key escapes the root: {key!r}")
        return path

    def get(self, key: str) -> bytes | None:
        try:
            return self._path(key).read_bytes()
        except FileNotFoundError:
            return None

    def delete(self, key: str) -> None:
        try:
            self._path(key).unlink()
        except FileNotFoundError:
            # Idempotent, like the gateway's Dir.Delete: retention retries, and the
            # second attempt must not be an error.
            return

    def delete_prefix(self, prefix: str) -> int:
        base = self._path(prefix.rstrip("/"))
        if not base.is_dir():
            return 0
        removed = 0
        for path in sorted(base.rglob("*"), reverse=True):
            if path.is_file():
                path.unlink()
                removed += 1
            elif path.is_dir():
                path.rmdir()
        base.rmdir()
        return removed

    def day_prefixes(self, prefix: str) -> list[str]:
        base = self._path(prefix.rstrip("/"))
        if not base.is_dir():
            return []
        return sorted(f"{prefix}{child.name}/" for child in base.iterdir() if child.is_dir())


@dataclass
class S3BlobStore:
    """S3 (or MinIO) backed store.

    ``boto3`` is imported inside the constructor, the same convention the provider
    adapters follow: a development deployment on ``SENTINEL_BLOB_DIR`` must not need
    the AWS SDK installed, and the unit tests must not need it either.

    ``client`` injects a pre-built client so tests exercise this class without
    credentials or a network.

    There is deliberately no bucket prefix. The gateway's writer
    (``internal/blob/s3.go``) puts objects at the key ``SegmentKey`` returns and
    nothing else, and the comment there spells out why: a layer that rewrote,
    prefixed or flattened keys would break the day-prefix retention delete. A prefix
    configured on only one of the two services is also the exact silent mismatch the
    key-format test at the top of this module exists to prevent.
    """

    bucket: str
    region: str | None = None
    endpoint_url: str | None = None
    client: object = None
    #: S3 accepts at most 1000 keys per DeleteObjects call. A day of one agent's
    #: audio is ~18k objects at one per second, so the cap is reached constantly.
    delete_batch: int = 1_000

    def __post_init__(self) -> None:
        if self.client is None:
            import boto3  # noqa: PLC0415 - lazy, see providers/__init__

            self.client = boto3.client(
                "s3", region_name=self.region, endpoint_url=self.endpoint_url
            )

    def get(self, key: str) -> bytes | None:
        try:
            resp = self.client.get_object(Bucket=self.bucket, Key=key)
        except Exception as exc:  # noqa: BLE001 - botocore's exceptions are dynamic
            # Only a genuine absence is None. Anything else — a permissions error, a
            # throttle — must propagate, because treating it as "no audio" would mark
            # the call failed and lose the retry.
            if _is_not_found(exc):
                return None
            raise
        return resp["Body"].read()

    def delete(self, key: str) -> None:
        self.client.delete_object(Bucket=self.bucket, Key=key)

    def delete_prefix(self, prefix: str) -> int:
        removed = 0
        batch: list[dict] = []
        for key in self._list_keys(prefix):
            batch.append({"Key": key})
            if len(batch) >= self.delete_batch:
                removed += self._delete_batch(batch)
                batch = []
        if batch:
            removed += self._delete_batch(batch)
        return removed

    def _delete_batch(self, batch: list[dict]) -> int:
        resp = self.client.delete_objects(
            Bucket=self.bucket, Delete={"Objects": batch, "Quiet": True}
        )
        errors = resp.get("Errors") or []
        for err in errors:
            # No key in the log line: the key contains the tenant and the call id.
            log.error("object delete refused", extra={"code": err.get("Code")})
        return len(batch) - len(errors)

    def _list_keys(self, prefix: str) -> Iterable[str]:
        paginator = self.client.get_paginator("list_objects_v2")
        for page in paginator.paginate(Bucket=self.bucket, Prefix=prefix):
            for obj in page.get("Contents") or []:
                yield obj["Key"]

    def day_prefixes(self, prefix: str) -> list[str]:
        # Delimiter listing so a tenant with three years of audio costs one page per
        # thousand days rather than one per object.
        paginator = self.client.get_paginator("list_objects_v2")
        out: list[str] = []
        for page in paginator.paginate(Bucket=self.bucket, Prefix=prefix,
                                       Delimiter="/"):
            for common in page.get("CommonPrefixes") or []:
                out.append(common["Prefix"])
        return sorted(out)


def _is_not_found(exc: Exception) -> bool:
    response = getattr(exc, "response", None) or {}
    code = str((response.get("Error") or {}).get("Code", ""))
    return code in {"NoSuchKey", "404", "NotFound"} or type(exc).__name__ == "NoSuchKey"


#: Where the ASR default already put the region question. OPEN-4 is not settled, and
#: the OpenAPI document annotates production as ap-south-1, so that is the default
#: here rather than boto3's (which is "whatever the environment happens to say").
#: Same value and same reasoning as ``blob.DefaultRegion`` on the gateway side.
DEFAULT_REGION = "ap-south-1"


def blob_store_from_env(env: dict[str, str] | None = None) -> BlobStore:
    """Build the object store the way the gateway chooses one, from the same names.

    ``SENTINEL_S3_BUCKET``    S3 bucket; selects the S3 backend.
    ``SENTINEL_S3_REGION``    AWS region, ``ap-south-1`` by default (OPEN-4).
    ``SENTINEL_S3_ENDPOINT``  MinIO or another S3-compatible endpoint.
    ``SENTINEL_BLOB_DIR``     local directory; selects the development backend.

    The variable names are the gateway's, deliberately: the writer and the reader
    have to be pointed at the same bucket, and two services with two spellings of
    the same setting is how they end up pointed at different ones — which does not
    fail, it just finds no audio.

    Refuses to guess when neither is set. ``main.go`` takes the same line for the
    same reason: a pipeline pointed at nothing transcribes nothing and reports it as
    an ASR problem.
    """
    env = dict(os.environ if env is None else env)
    bucket = env.get("SENTINEL_S3_BUCKET")
    if bucket:
        return S3BlobStore(
            bucket=bucket,
            region=env.get("SENTINEL_S3_REGION") or DEFAULT_REGION,
            endpoint_url=env.get("SENTINEL_S3_ENDPOINT") or None,
        )
    directory = env.get("SENTINEL_BLOB_DIR")
    if directory:
        return DirBlobStore(root=directory)
    raise RuntimeError(
        "no object storage configured: set SENTINEL_S3_BUCKET for S3 or "
        "SENTINEL_BLOB_DIR for a local directory"
    )
