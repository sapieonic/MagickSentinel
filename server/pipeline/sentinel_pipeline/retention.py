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


class BlobStore(Protocol):
    def delete(self, key: str) -> None: ...


@dataclass
class PurgeResult:
    tenant_id: str
    audio_segments: int = 0
    audio_bytes_freed: int = 0
    transcripts: int = 0
    errors: list[str] = field(default_factory=list)


@dataclass
class RetentionJob:
    store: RetentionStore
    blob: BlobStore
    batch_size: int = 1_000
    now: datetime | None = None

    def _now(self) -> datetime:
        return self.now or datetime.now(timezone.utc)

    def run(self) -> list[PurgeResult]:
        return [self.purge_tenant(*t) for t in self.store.tenants()]

    def purge_tenant(self, tenant_id: str, audio_days: int, transcript_days: int) -> PurgeResult:
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
            "audio_cutoff": audio_cutoff.date().isoformat(),
            "transcript_cutoff": transcript_cutoff.date().isoformat(),
            "errors": len(result.errors),
        })
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
