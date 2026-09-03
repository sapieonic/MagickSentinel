"""Nightly retention purge.

Audio and transcripts have separate retention periods and audio purges much sooner —
30 days against 365 by default. That asymmetry is the point: the audio is the
sensitive artifact and the transcript is what the compliance record actually needs,
so a bank's reviewer can still trace a flag to the words a year later without the
recording still existing.

Every purge batch writes an audit entry. A retention job that quietly deletes a
borrower's data leaves nothing to answer a DPDP request with.

The defaults in the schema are placeholders — retention periods are **OPEN-6** and
have to come from the customer in writing. This job reads them per tenant rather than
hard-coding anything.

Two properties this module is built around, both because a retention bug in a
compliance product destroys evidence rather than merely losing data:

**Dry run is the safe default at the entrypoint.** ``RetentionJob(dry_run=True)``
counts what it would delete and touches nothing.
:func:`sentinel_pipeline.service.run_retention` requires an explicit
``SENTINEL_RETENTION_COMMIT=1`` before anything is deleted, so a mis-scheduled cron,
a wrong DSN or an OPEN-6 answer that has not landed yet costs a log line rather than
a customer's evidence.

**Deletes go object first, then row, and the day-prefix sweep goes last.** The
ordering is the subject of :meth:`RetentionJob.purge_tenant` below; the sweep exists
because ``blob.SegmentKey`` partitions keys by day precisely so a day can be removed
by prefix, which is also the only way to collect objects whose row is already gone.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta, timezone
from typing import Protocol

log = logging.getLogger(__name__)


class RetentionStore(Protocol):
    """The narrow database surface the purge needs."""

    def tenants(self) -> list[tuple[str, int, int]]:
        """``(tenant_id, audio_retention_days, transcript_retention_days)``."""

    def audio_keys_before(self, tenant_id: str, cutoff: datetime, limit: int) -> list[tuple[str, int, int, str]]:
        """``(call_id, channel, seq, s3_key)`` for segments older than the cutoff."""

    def delete_media_rows(self, tenant_id: str, keys: list[tuple[str, int, int]]) -> int: ...

    def delete_transcripts_before(self, tenant_id: str, cutoff: datetime, limit: int) -> int: ...

    def audit(self, tenant_id: str, action: str, detail: dict) -> None: ...

    # The two counting methods exist for the dry run and nowhere else. A preview
    # cannot use the deleting loop below: that loop terminates because deleted rows
    # stop coming back, so a pass that deletes nothing would fetch the same batch
    # forever.
    def count_audio_before(self, tenant_id: str, cutoff: datetime) -> int: ...

    def count_transcripts_before(self, tenant_id: str, cutoff: datetime) -> int: ...


class BlobStore(Protocol):
    """Object deletion. ``delete`` is required; the prefix methods are optional.

    A store that can enumerate and delete by prefix (S3, and the local directory
    backend) also gets the day-prefix sweep. One that can only delete a key it is
    given still purges correctly from the rows — it just cannot collect objects whose
    row has already gone. The capability is probed rather than required so a minimal
    store, and the fakes in the tests, stay valid implementations.
    """

    def delete(self, key: str) -> None: ...

    def day_prefixes(self, prefix: str) -> list[str]: ...

    def delete_prefix(self, prefix: str) -> int: ...


@dataclass
class PurgeResult:
    tenant_id: str
    audio_segments: int = 0
    audio_bytes_freed: int = 0
    transcripts: int = 0
    #: Objects removed by the expired-day sweep — those whose row was already gone.
    swept_objects: int = 0
    swept_days: int = 0
    #: True when nothing was deleted and the counts are a preview.
    dry_run: bool = False
    errors: list[str] = field(default_factory=list)


@dataclass
class RetentionJob:
    store: RetentionStore
    blob: BlobStore
    batch_size: int = 1_000
    now: datetime | None = None
    #: When set, nothing is deleted: the counts are what *would* go. The entrypoint
    #: defaults to this and requires an explicit opt-in to delete, because the first
    #: run of a purge job against a real customer's data is the single most dangerous
    #: thing in this repository.
    dry_run: bool = False

    def _now(self) -> datetime:
        return self.now or datetime.now(timezone.utc)

    def run(self) -> list[PurgeResult]:
        return [self.purge_tenant(*t) for t in self.store.tenants()]

    def purge_tenant(self, tenant_id: str, audio_days: int, transcript_days: int) -> PurgeResult:
        if self.dry_run:
            return self.preview_tenant(tenant_id, audio_days, transcript_days)
        result = PurgeResult(tenant_id=tenant_id)
        now = self._now()

        audio_cutoff = now - timedelta(days=audio_days)
        while True:
            batch = self.store.audio_keys_before(tenant_id, audio_cutoff, self.batch_size)
            if not batch:
                break
            deleted_keys: list[tuple[str, int, int]] = []
            for call_id, channel, seq, s3_key in batch:
                try:
                    self.blob.delete(s3_key)
                except Exception as exc:  # noqa: BLE001 - storage errors are opaque
                    # The row stays so the next run retries. Deleting the row while
                    # the object survives would orphan audio that no retention sweep
                    # can ever find again — the worst outcome available here.
                    log.error("audio purge failed", extra={"tenant_id": tenant_id,
                                                           "error_type": type(exc).__name__})
                    result.errors.append(f"blob delete failed for {call_id}/{channel}/{seq}")
                    continue
                deleted_keys.append((call_id, channel, seq))
            if deleted_keys:
                result.audio_segments += self.store.delete_media_rows(tenant_id, deleted_keys)
            if len(batch) < self.batch_size:
                break

        # Only now, with every expired row's object already deleted, is it safe to
        # remove whole day prefixes: anything still under an expired day has no live
        # row pointing at it. Doing this first would delete objects whose rows are
        # still present and turn a purge into a set of broken calls.
        self._sweep_expired_days(tenant_id, audio_cutoff, result)

        transcript_cutoff = now - timedelta(days=transcript_days)
        while True:
            n = self.store.delete_transcripts_before(tenant_id, transcript_cutoff, self.batch_size)
            result.transcripts += n
            if n < self.batch_size:
                break

        # One audit entry per tenant per run, carrying counts only. Never a call id,
        # never an account reference: the audit log is not a place to leak the data
        # the purge just removed.
        self.store.audit(tenant_id, "retention.purge", {
            "audio_segments": result.audio_segments,
            "transcripts": result.transcripts,
            "swept_objects": result.swept_objects,
            "swept_days": result.swept_days,
            "audio_cutoff": audio_cutoff.date().isoformat(),
            "transcript_cutoff": transcript_cutoff.date().isoformat(),
            "errors": len(result.errors),
        })
        return result

    # ------------------------------------------------------------- day-prefix sweep

    #: How far the prefix sweep stays behind the row-driven purge, in days.
    #:
    #: A key's day comes from the gateway's clock at upload; the row's ``received_at``
    #: comes from the database's. They agree to within a clock skew, not exactly, and
    #: a prefix delete is not recoverable — so the sweep never touches the day the
    #: cutoff itself falls in, nor the one before it. The cost is one extra day of
    #: retained orphans; the alternative cost is deleting the day the cutoff is
    #: passing through, whose rows are still live.
    SWEEP_MARGIN_DAYS = 1

    def _sweep_expired_days(self, tenant_id: str, audio_cutoff: datetime,
                            result: PurgeResult) -> None:
        """Delete whole day prefixes that are entirely past the audio cutoff.

        This is what ``blob.SegmentKey``'s date partitioning is *for* (the comment on
        it says so, and OPEN-6 repeats it). It catches the one class of expired audio
        the row-driven purge structurally cannot: the gateway writes the object before
        the row (``ingest/sink.go``, deliberately, so a row never points at nothing),
        so a crash between the two leaves an object no row mentions. Nothing else will
        ever find it, and "audio we cannot see is still audio past its retention
        period" is not an answer to a DPDP request.

        Skipped, with no error, on a store that cannot enumerate prefixes: the
        row-driven purge above is the correctness baseline and this is the sweep on
        top of it.
        """
        day_prefixes = getattr(self.blob, "day_prefixes", None)
        delete_prefix = getattr(self.blob, "delete_prefix", None)
        if day_prefixes is None or delete_prefix is None:
            return

        from .blobstore import day_prefix, tenant_prefix  # noqa: PLC0415 - avoids a cycle

        horizon = (audio_cutoff - timedelta(days=self.SWEEP_MARGIN_DAYS)).date()
        try:
            present = day_prefixes(tenant_prefix(tenant_id))
        except Exception as exc:  # noqa: BLE001 - storage errors are opaque
            log.error("could not list audio day prefixes",
                      extra={"tenant_id": tenant_id, "error_type": type(exc).__name__})
            result.errors.append("listing day prefixes failed")
            return

        for prefix in present:
            day = prefix.rstrip("/").rsplit("/", 1)[-1]
            try:
                parsed = date.fromisoformat(day)
            except ValueError:
                # Not a day partition. Left alone rather than guessed at: an
                # unexpected prefix under a tenant's audio is something to look at,
                # not something to delete.
                log.warning("unexpected prefix under a tenant's audio",
                            extra={"tenant_id": tenant_id})
                continue
            if parsed >= horizon:
                continue
            try:
                removed = delete_prefix(day_prefix(tenant_id, day))
            except Exception as exc:  # noqa: BLE001
                log.error("day prefix purge failed",
                          extra={"tenant_id": tenant_id,
                                 "error_type": type(exc).__name__})
                result.errors.append("day prefix delete failed")
                continue
            result.swept_objects += removed
            result.swept_days += 1

    # -------------------------------------------------------------------- dry run

    def preview_tenant(self, tenant_id: str, audio_days: int,
                       transcript_days: int) -> PurgeResult:
        """Count what a real run would delete, without deleting anything.

        Counts rather than the deleting loop with the deletes removed, because that
        loop terminates by consuming its own input: with nothing deleted, the same
        batch comes back forever. The audit entry is still written — an operator
        asking "what would retention have removed last night" deserves a durable
        answer, and one marked ``dry_run`` cannot be mistaken for a purge that ran.
        """
        now = self._now()
        audio_cutoff = now - timedelta(days=audio_days)
        transcript_cutoff = now - timedelta(days=transcript_days)
        result = PurgeResult(tenant_id=tenant_id, dry_run=True)
        result.audio_segments = self.store.count_audio_before(tenant_id, audio_cutoff)
        result.transcripts = self.store.count_transcripts_before(tenant_id,
                                                                 transcript_cutoff)
        self.store.audit(tenant_id, "retention.preview", {
            "dry_run": True,
            "audio_segments": result.audio_segments,
            "transcripts": result.transcripts,
            "audio_cutoff": audio_cutoff.date().isoformat(),
            "transcript_cutoff": transcript_cutoff.date().isoformat(),
        })
        log.info("retention dry run", extra={"tenant_id": tenant_id,
                                             "audio_segments": result.audio_segments,
                                             "transcripts": result.transcripts})
        return result


@dataclass
class SubjectRequest:
    """A DPDP data-subject request, keyed by account reference.

    The BPO is the data fiduciary and MagickVoice is the data processor, so we act on
    the BPO's instruction rather than on the borrower's directly — but the path has to
    exist and has to be per tenant.
    """

    tenant_id: str
    account_ref: str
    kind: str  # "export" | "delete"
    requested_by: str
    requested_at: date
