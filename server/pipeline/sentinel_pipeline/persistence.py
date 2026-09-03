"""Postgres implementations of everything the pipeline reads and writes.

All of it goes through :mod:`sentinel_pipeline.db`, which means every statement in
this file runs inside a transaction carrying the row-level-security context, as the
``sentinel_pipeline`` role, which is ``NOBYPASSRLS``. There is no query here that is
allowed to see two tenants at once, and the one lookup that legitimately precedes a
tenant context — enumerating the tenants a nightly job has to visit — goes through a
narrow ``SECURITY DEFINER`` function (``db/migrations/0008``) rather than through a
loosened policy, the same trade ``db/migrations/0005`` made for the gateway's three
bootstrap lookups.

Three things about the writes are worth stating before reading them.

**Every write is idempotent on the call.** ``consumer.py`` promises at-least-once
delivery, so a redelivered ``call.finalize`` re-runs the whole pipeline and must
overwrite rather than duplicate. Transcripts are keyed ``(call_id, channel)``,
analyses and PTPs by ``call_id``, flags by ``(call_id, rule_id, tier)``, and each
upsert lists the columns it owns.

**A re-run must never overwrite a human's decision.** That is the one thing that is
not idempotent, because it is not the pipeline's to write: a reviewer's verdict on a
flag, a reviewer's note, an agent's response, an agent's correction to an extracted
promise-to-pay. Those columns are absent from every ``DO UPDATE SET`` below, and the
stale-flag delete is restricted to flags no human has touched. Losing a reviewer's
work to a redelivered NATS message is the kind of bug that ends a pilot.

**Money is integer paise** — ``cost_paise`` is a ``bigint``, and nothing here turns
it into a float on the way past.
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass
from datetime import date, datetime, timedelta, timezone
from typing import Iterable, Sequence

from .compliance.engine import RuleEngine, load_default_rule_set, load_rule_set
from .cost import TenantBudget
from .coverage import CapturedCall, Coverage
from .db import Database, call_uuid
from .models import Analysis, CallContext, Channel, Finding, Transcript
from .segments import SegmentRow

log = logging.getLogger(__name__)


# --------------------------------------------------------------------- reading audio


_SEGMENTS_SQL = """
SELECT seq, s3_key, foreign_audio
  FROM media_segments
 WHERE call_id = %s AND channel = %s
   AND NOT foreign_audio
 ORDER BY seq
"""


@dataclass
class PostgresSegmentIndex:
    """Which objects hold one channel of one call.

    ``NOT foreign_audio`` in the SQL is the first of the two places the tier B
    foreign-audio rule is enforced; :func:`sentinel_pipeline.segments.transcribable`
    is the second, and re-checks the flag on whatever came back. Two checks for one
    rule looks redundant until you consider what a single missed one produces: RBI
    conduct findings quoted from an agent's music, attached to a call, shown to a
    bank. See ``contracts/wire.md`` §4.2.
    """

    db: Database
    tenant_id: str

    def segments_for(self, call_id: str, channel: Channel) -> Sequence[SegmentRow]:
        with self.db.as_system(self.tenant_id) as conn:
            rows = conn.execute(_SEGMENTS_SQL, (call_uuid(call_id), int(channel))).fetchall()
        return [SegmentRow(seq=int(r[0]), s3_key=str(r[1]), foreign_audio=bool(r[2]))
                for r in rows]


# ------------------------------------------------------------------------ the sink


_UPSERT_TRANSCRIPT = """
INSERT INTO transcripts (tenant_id, call_id, channel, asr_provider, asr_version,
                         language, text, word_timings, confidence)
VALUES (%s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s)
ON CONFLICT (call_id, channel) DO UPDATE
   SET asr_provider = excluded.asr_provider,
       asr_version  = excluded.asr_version,
       language     = excluded.language,
       text         = excluded.text,
       word_timings = excluded.word_timings,
       confidence   = excluded.confidence,
       created_at   = now()
"""

_UPSERT_ANALYSIS = """
INSERT INTO analyses (call_id, tenant_id, prompt_version, model, summary, disposition,
                      next_action, sentiment, talk_ratio, interruptions,
                      input_tokens, output_tokens, cost_paise, truncated)
VALUES (%s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s, %s, %s, %s, %s, %s)
ON CONFLICT (call_id) DO UPDATE
   SET prompt_version = excluded.prompt_version,
       model          = excluded.model,
       summary        = excluded.summary,
       disposition    = excluded.disposition,
       next_action    = excluded.next_action,
       sentiment      = excluded.sentiment,
       talk_ratio     = excluded.talk_ratio,
       interruptions  = excluded.interruptions,
       input_tokens   = excluded.input_tokens,
       output_tokens  = excluded.output_tokens,
       cost_paise     = excluded.cost_paise,
       truncated      = excluded.truncated,
       created_at     = now()
"""

# The WHERE on the conflict action is the whole point: once an agent has corrected an
# extracted promise-to-pay, the extraction has lost the argument permanently. A
# re-run overwriting `agent_amount_paise` would erase a correction the agent made in
# the widget, and the PTP is the figure the floor is managed on.
_UPSERT_PTP = """
INSERT INTO ptps (tenant_id, call_id, amount_paise, due_date, confidence, extracted_span)
VALUES (%s, %s, %s, %s, %s, %s::int4range)
ON CONFLICT (call_id) DO UPDATE
   SET amount_paise   = excluded.amount_paise,
       due_date       = excluded.due_date,
       confidence     = excluded.confidence,
       extracted_span = excluded.extracted_span
 WHERE ptps.corrected_at IS NULL
"""

_DELETE_UNCONFIRMED_PTP = """
DELETE FROM ptps WHERE call_id = %s AND corrected_at IS NULL
"""

_UPSERT_FLAG = """
INSERT INTO flags (tenant_id, call_id, rule_id, rule_set_version, severity, tier,
                   span_start_ms, span_end_ms, evidence_text, judge_rationale,
                   judge_confidence)
VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
ON CONFLICT (call_id, rule_id, tier) DO UPDATE
   SET rule_set_version = excluded.rule_set_version,
       severity         = excluded.severity,
       span_start_ms    = excluded.span_start_ms,
       span_end_ms      = excluded.span_end_ms,
       evidence_text    = excluded.evidence_text,
       judge_rationale  = excluded.judge_rationale,
       judge_confidence = excluded.judge_confidence
"""

# Only flags nobody has looked at. `status = 'open'` alone is not enough — a flag can
# be assigned to a reviewer and still be open — so the reviewer and agent columns are
# checked too. A flag that has been reviewed, dismissed or answered is a record of a
# human decision and survives every re-run, even one under a newer rule set.
_DELETE_STALE_FLAGS = """
DELETE FROM flags
 WHERE call_id = %s
   AND status = 'open'
   AND reviewer_uid IS NULL
   AND reviewer_note IS NULL
   AND agent_response IS NULL
   AND (rule_id || ':' || tier::text) <> ALL (%s::text[])
"""

_SET_STATUS = """
UPDATE calls SET status = %s, updated_at = now() WHERE id = %s
"""


@dataclass
class PostgresSink:
    """The production :class:`sentinel_pipeline.worker.Sink`.

    One transaction per method rather than one per call. That is deliberate: the
    worker writes the transcript, then the analysis, then the findings, and each is
    useful on its own. Holding a single transaction open across the whole finalize
    would mean an analysis provider timing out after ninety seconds also threw away
    the transcript that had already succeeded, and the retry would pay for the ASR
    again — while holding a connection and its locks for the duration.
    """

    db: Database
    tenant_id: str

    def save_transcript(self, call_id: str, transcript: Transcript) -> None:
        uuid = call_uuid(call_id)
        with self.db.as_system(self.tenant_id) as conn:
            for channel, ct in sorted(transcript.channels.items()):
                # Word timings in the shape the gateway already reads back
                # (`transcriptTurns` in store/queries.go): start_ms, end_ms, text.
                # Changing these key names silently empties the portal's transcript
                # view, because that reader falls back to whole-channel text when the
                # JSON does not parse into what it expects.
                timings = json.dumps([
                    {"text": w.text, "start_ms": w.start_ms, "end_ms": w.end_ms,
                     "confidence": w.confidence}
                    for w in ct.words
                ])
                # `embedding` is left NULL. Nothing in this pipeline computes one yet,
                # and writing a zero vector would make semantic search silently
                # return every call rather than none.
                conn.execute(_UPSERT_TRANSCRIPT, (
                    self.tenant_id, uuid, int(channel), ct.provider, ct.provider_version,
                    ct.language, ct.text, timings, ct.confidence,
                ))

    def save_analysis(self, call_id: str, analysis: Analysis, cost_paise: int) -> None:
        uuid = call_uuid(call_id)
        with self.db.as_system(self.tenant_id) as conn:
            conn.execute(_UPSERT_ANALYSIS, (
                uuid, self.tenant_id, analysis.prompt_version, analysis.model,
                analysis.summary, analysis.disposition.value, analysis.next_action,
                json.dumps(analysis.sentiment), analysis.talk_ratio,
                analysis.interruptions, analysis.input_tokens, analysis.output_tokens,
                int(cost_paise), analysis.truncated,
            ))
            ptp = analysis.ptp
            if ptp.present:
                conn.execute(_UPSERT_PTP, (
                    self.tenant_id, uuid, ptp.amount_paise, ptp.due_date,
                    ptp.confidence, _int4range(ptp.evidence_span_ms),
                ))
            else:
                # A re-run that no longer sees a promise retracts the extracted one,
                # so the two directions are equally idempotent — but only while the
                # agent has not confirmed or corrected it. Their correction is the
                # record; ours is a guess about it.
                conn.execute(_DELETE_UNCONFIRMED_PTP, (uuid,))

    def save_findings(self, call_id: str, rule_set_version: int,
                      findings: list[Finding]) -> None:
        uuid = call_uuid(call_id)
        keys = [f"{f.rule_id}:{f.tier}" for f in findings]
        with self.db.as_system(self.tenant_id) as conn:
            for finding in findings:
                conn.execute(_UPSERT_FLAG, (
                    self.tenant_id, uuid, finding.rule_id, rule_set_version,
                    finding.severity.value, finding.tier, finding.span_start_ms,
                    finding.span_end_ms, finding.evidence_text, finding.rationale,
                    finding.confidence,
                ))
            # Insert before delete, in one transaction: a reader that lands between
            # the two sees the old flags and the new ones rather than a call that
            # briefly has no compliance findings at all.
            conn.execute(_DELETE_STALE_FLAGS, (uuid, keys))

    def set_status(self, call_id: str, status: str) -> None:
        with self.db.as_system(self.tenant_id) as conn:
            conn.execute(_SET_STATUS, (status, call_uuid(call_id)))


def _int4range(span: tuple[int, int] | None) -> str | None:
    """Render an evidence span as Postgres range literal text, or NULL.

    Built here rather than with ``int4range(%s, %s)`` in SQL because that function
    turns two NULLs into an unbounded range — which reads as "the evidence is the
    whole call" — where the honest answer is that there is no span.
    """
    if not span:
        return None
    start, end = span
    return f"[{int(start)},{int(end)})"


# ------------------------------------------------------------------- reading a call


_CALL_SQL = """
SELECT c.user_uid, c.started_at, COALESCE(c.duration_ms, 0), c.account_ref,
       c.direction, c.capture_tier, t.timezone, t.policy
  FROM calls c
  JOIN tenants t ON t.id = c.tenant_id
 WHERE c.id = %s
"""

_PRIOR_CONTACTS_SQL = """
SELECT count(*)
  FROM calls
 WHERE tenant_id = %s
   AND account_ref = %s
   AND started_at >= %s - interval '24 hours'
   AND started_at <  %s
   AND id <> %s
"""

_ACTIVE_RULE_SET_SQL = """
SELECT version, definition FROM rule_sets WHERE tenant_id = %s AND active
"""

_BUDGET_SQL = """
SELECT t.monthly_budget_paise,
       COALESCE(t.policy ->> 'model_kill_switch', 'false'),
       COALESCE((SELECT sum(a.cost_paise) FROM analyses a
                  WHERE a.tenant_id = t.id
                    AND a.created_at >= date_trunc('month', now())), 0)
  FROM tenants t
 WHERE t.id = %s
"""


class CallNotFound(LookupError):
    """The finalize message names a call this tenant does not have.

    Not a transient failure. Either the message is for another tenant — which the RLS
    context turns into "no rows" rather than into another tenant's call, exactly as
    intended — or the call was discarded. Retrying cannot fix either.
    """


@dataclass
class PostgresCallRepository:
    """Everything the finalize path has to look up, given only the bus message.

    The message carries a call id, a tenant id, an attempt number and a timestamp
    and nothing else — no transcript, no audio, no borrower data on the bus — so
    every input to the pipeline is read here, under that tenant's RLS context.
    """

    db: Database

    def call_context(self, tenant_id: str, call_id: str) -> CallContext:
        uuid = call_uuid(call_id)
        with self.db.as_system(tenant_id) as conn:
            row = conn.execute(_CALL_SQL, (uuid,)).fetchone()
            if row is None:
                raise CallNotFound(f"call {call_id} is not present for this tenant")
            (user_uid, started_at, duration_ms, account_ref, direction,
             capture_tier, tz, policy) = row
            policy = _as_dict(policy)

            prior = 0
            if account_ref:
                prior = int(conn.execute(_PRIOR_CONTACTS_SQL, (
                    tenant_id, account_ref, started_at, started_at, uuid,
                )).fetchone()[0])

        return CallContext(
            call_id=call_id,
            tenant_id=tenant_id,
            user_uid=user_uid,
            started_at=started_at,
            duration_ms=int(duration_ms),
            tenant_timezone=tz or "Asia/Kolkata",
            # The floor's language, from tenant policy. This is what selects the
            # transcriber on a routed floor (see providers/registry.py), so a wrong
            # value here is not a hint being ignored — it is Tamil audio going to a
            # model that has no Tamil and coming back as confident nonsense. Absent
            # means "let the provider detect", which is the right default for a
            # code-mixed floor and the wrong one for Tamil.
            language=policy.get("language") or None,
            account_ref=account_ref,
            direction=direction or "outbound",
            capture_tier=capture_tier or "A",
            prior_contacts_24h=prior,
        )

    def rule_engine(self, tenant_id: str) -> RuleEngine:
        with self.db.as_system(tenant_id) as conn:
            row = conn.execute(_ACTIVE_RULE_SET_SQL, (tenant_id,)).fetchone()
        if row is None:
            # Loud, and then the shipped defaults. Running no rules at all would
            # produce a call that looks reviewed and carries no findings, which is
            # worse than running a rule set the tenant has not customised.
            log.warning("tenant has no active rule set; using the shipped defaults",
                        extra={"tenant_id": tenant_id})
            return RuleEngine(load_default_rule_set())
        version, definition = int(row[0]), _as_dict(row[1])
        return RuleEngine(load_rule_set(definition, version))

    def budget(self, tenant_id: str) -> TenantBudget:
        """This month's spend against the tenant's cap.

        Recomputed per call from ``analyses.cost_paise`` rather than kept in a
        counter: a counter drifts when a call is re-run, and a budget that drifts
        upward stops analysis for a tenant that has not actually spent the money.

        The month boundary is the database's (UTC), not the tenant's. For a floor in
        IST that moves the reset by five and a half hours once a month, which is not
        worth a per-tenant ``AT TIME ZONE`` on a query that runs on every call.

        **Known understatement: tier-2 judge spend is not in this figure.** The
        schema has one cost column, ``analyses.cost_paise``, and ``worker.py``
        writes the analysis row before the judge runs, so a judged call's judge
        tokens are counted against the in-memory budget for that call and then
        forgotten. The effect is that the monthly total the budget compares against
        the cap is low by the judge's share — roughly the tier-1 hit rate plus the
        judge sample percentage, so single-digit percent, but low rather than high,
        which is the wrong direction for a spend control. Judge spend *is* visible
        per tenant and per model in the ``sentinel.model.spend`` metric
        (:mod:`sentinel_pipeline.telemetry`), which is the honest workaround until
        there is somewhere to persist it: either a cost column on ``flags`` or a
        per-call spend ledger. Both are schema changes and neither is this change.
        """
        with self.db.as_system(tenant_id) as conn:
            row = conn.execute(_BUDGET_SQL, (tenant_id,)).fetchone()
        if row is None:
            raise CallNotFound(f"tenant {tenant_id} is not present")
        cap, kill_switch, spent = row
        return TenantBudget(
            tenant_id=tenant_id,
            monthly_budget_paise=int(cap) if cap is not None else None,
            spent_paise=int(spent or 0),
            # An operator turns this on in tenant policy when spend spikes. Capture
            # and tier-1 compliance keep working, which is the part the customer is
            # paying for; the model calls stop.
            kill_switch=str(kill_switch).lower() in {"true", "1", "yes"},
        )


def _as_dict(value: object) -> dict:
    """``jsonb`` comes back as a dict from psycopg and as text from a plain cursor."""
    if isinstance(value, dict):
        return value
    if isinstance(value, (str, bytes)):
        try:
            parsed = json.loads(value)
        except ValueError:
            return {}
        return parsed if isinstance(parsed, dict) else {}
    return {}


# ------------------------------------------------------------------ tenant listing


_TENANTS_SQL = """
SELECT tenant_id::text, audio_retention_days, transcript_retention_days, timezone
  FROM sentinel_pipeline_tenants()
"""


@dataclass(frozen=True)
class TenantRow:
    tenant_id: str
    audio_retention_days: int
    transcript_retention_days: int
    timezone: str = "Asia/Kolkata"


def list_tenants(db: Database) -> list[TenantRow]:
    """Every tenant the nightly jobs have to visit.

    This is the one query in the pipeline with no tenant context, and it cannot have
    one: it is the query that produces the list of contexts. Rather than loosen the
    ``tenants`` policy — which would let the gateway's role enumerate customers too —
    ``db/migrations/0008`` adds a ``SECURITY DEFINER`` function that returns tenant
    ids and retention periods and nothing else, executable by ``sentinel_pipeline``
    alone. Same shape as the three bootstrap functions in ``db/migrations/0005``, for
    the same reason.

    Retention periods are read here per tenant and never hard-coded: they are
    **OPEN-6**, the schema defaults of 30 and 365 days are explicitly placeholders,
    and a job with a constant in it would quietly outlive the decision.
    """
    with db.without_tenant("listing tenants for a scheduled job") as conn:
        rows = conn.execute(_TENANTS_SQL).fetchall()
    return [TenantRow(str(r[0]), int(r[1]), int(r[2]), str(r[3] or "Asia/Kolkata"))
            for r in rows]


# ---------------------------------------------------------------------- retention


_AUDIO_KEYS_SQL = """
SELECT call_id::text, channel, seq, s3_key
  FROM media_segments
 WHERE tenant_id = %s AND received_at < %s
 ORDER BY received_at
 LIMIT %s
"""

_COUNT_AUDIO_SQL = """
SELECT count(*) FROM media_segments WHERE tenant_id = %s AND received_at < %s
"""

_DELETE_MEDIA_ROWS_SQL = """
DELETE FROM media_segments m
 USING unnest(%s::uuid[], %s::smallint[], %s::int[]) AS k(call_id, channel, seq)
 WHERE m.tenant_id = %s
   AND m.call_id = k.call_id AND m.channel = k.channel AND m.seq = k.seq
"""

# ctid rather than a key: `transcripts` is keyed (call_id, channel) and a two-column
# IN-list of a thousand pairs plans far worse than a bounded self-join on the
# physical row id.
_DELETE_TRANSCRIPTS_SQL = """
DELETE FROM transcripts
 WHERE ctid IN (SELECT ctid FROM transcripts
                 WHERE tenant_id = %s AND created_at < %s
                 LIMIT %s)
"""

_COUNT_TRANSCRIPTS_SQL = """
SELECT count(*) FROM transcripts WHERE tenant_id = %s AND created_at < %s
"""

_AUDIT_SQL = """
INSERT INTO audit_log (tenant_id, actor_uid, action, entity, entity_id, detail)
VALUES (%s, 'system', %s, 'tenant', %s, %s::jsonb)
"""


@dataclass
class PostgresRetentionStore:
    """The database half of the nightly purge.

    The delete order across the two stores is fixed by
    :class:`sentinel_pipeline.retention.RetentionJob` and is the object first, then
    the row. The reverse is tempting — the row is the cheap delete — and it is wrong:
    a row is the only record that an object exists, so losing it while the object
    survives leaves audio past its retention period that no later sweep can find and
    no DPDP request can answer for. An object with no row is the recoverable
    direction: the day-prefix sweep collects it.
    """

    db: Database

    def tenants(self) -> list[tuple[str, int, int]]:
        return [(t.tenant_id, t.audio_retention_days, t.transcript_retention_days)
                for t in list_tenants(self.db)]

    def audio_keys_before(self, tenant_id: str, cutoff: datetime,
                          limit: int) -> list[tuple[str, int, int, str]]:
        with self.db.as_system(tenant_id) as conn:
            rows = conn.execute(_AUDIO_KEYS_SQL, (tenant_id, cutoff, limit)).fetchall()
        return [(str(r[0]), int(r[1]), int(r[2]), str(r[3])) for r in rows]

    def count_audio_before(self, tenant_id: str, cutoff: datetime) -> int:
        with self.db.as_system(tenant_id) as conn:
            return int(conn.execute(_COUNT_AUDIO_SQL, (tenant_id, cutoff)).fetchone()[0])

    def delete_media_rows(self, tenant_id: str,
                          keys: list[tuple[str, int, int]]) -> int:
        if not keys:
            return 0
        call_ids = [k[0] for k in keys]
        channels = [int(k[1]) for k in keys]
        seqs = [int(k[2]) for k in keys]
        with self.db.as_system(tenant_id) as conn:
            cur = conn.execute(_DELETE_MEDIA_ROWS_SQL,
                               (call_ids, channels, seqs, tenant_id))
            return int(cur.rowcount)

    def delete_transcripts_before(self, tenant_id: str, cutoff: datetime,
                                  limit: int) -> int:
        with self.db.as_system(tenant_id) as conn:
            cur = conn.execute(_DELETE_TRANSCRIPTS_SQL, (tenant_id, cutoff, limit))
            return int(cur.rowcount)

    def count_transcripts_before(self, tenant_id: str, cutoff: datetime) -> int:
        with self.db.as_system(tenant_id) as conn:
            return int(conn.execute(_COUNT_TRANSCRIPTS_SQL,
                                    (tenant_id, cutoff)).fetchone()[0])

    def audit(self, tenant_id: str, action: str, detail: dict) -> None:
        with self.db.as_system(tenant_id) as conn:
            conn.execute(_AUDIT_SQL, (tenant_id, action, tenant_id, json.dumps(detail)))


# ----------------------------------------------------------------------- coverage


_CAPTURED_SQL = """
SELECT user_uid, started_at, COALESCE(duration_ms, 0), account_ref, dialer_call_id
  FROM calls
 WHERE tenant_id = %s
   AND started_at >= %s AND started_at < %s
   AND status <> 'discarded'
 ORDER BY started_at
"""

_UPSERT_COVERAGE = """
INSERT INTO coverage_daily (tenant_id, user_uid, date, dialer_calls, captured_calls,
                            dialer_minutes, captured_minutes, gap_reason)
VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
ON CONFLICT (tenant_id, user_uid, date) DO UPDATE
   SET dialer_calls     = excluded.dialer_calls,
       captured_calls   = excluded.captured_calls,
       dialer_minutes   = excluded.dialer_minutes,
       captured_minutes = excluded.captured_minutes,
       gap_reason       = excluded.gap_reason
"""

_POLICY_SQL = "SELECT policy FROM tenants WHERE id = %s"


@dataclass
class PostgresCoverageStore:
    """Our side of the reconciliation, and where the result lands.

    Only our side. The dialer's side is **OPEN-7** and lives behind
    :class:`sentinel_pipeline.coverage.CdrSource`; see :mod:`sentinel_pipeline.cdr`.
    """

    db: Database

    def captured_calls(self, tenant_id: str, day: date,
                       tz: str = "Asia/Kolkata") -> list[CapturedCall]:
        """Calls we captured on one local day.

        Bounded in the tenant's own timezone, not UTC. A collections floor works
        08:00–19:00 IST; a UTC day boundary would cut the evening shift in half and
        report a coverage gap that is really a timezone.
        """
        start, end = _local_day_bounds(day, tz)
        with self.db.as_system(tenant_id) as conn:
            rows = conn.execute(_CAPTURED_SQL, (tenant_id, start, end)).fetchall()
        return [
            CapturedCall(user_uid=str(r[0]), started_at=r[1], duration_ms=int(r[2]),
                         account_ref=r[3], dialer_call_id=r[4])
            for r in rows
        ]

    def agent_map(self, tenant_id: str) -> dict[str, str]:
        """Dialer agent id → our Firebase uid, from tenant policy.

        The two identifier spaces are unrelated — the bank's dialer knows nothing
        about our IdP — and without the mapping every call reconciles as uncaptured,
        which is the most alarming possible way to get a configuration error wrong
        (``coverage.reconcile`` says so too). Held in ``tenants.policy`` because
        OPEN-7 has not settled what the export even looks like, let alone where the
        mapping should live.
        """
        with self.db.as_system(tenant_id) as conn:
            row = conn.execute(_POLICY_SQL, (tenant_id,)).fetchone()
        policy = _as_dict(row[0]) if row else {}
        mapping = policy.get("cdr_agent_map") or {}
        return {str(k): str(v) for k, v in mapping.items()} if isinstance(mapping, dict) else {}

    def write_coverage(self, rows: Iterable[Coverage]) -> int:
        written = 0
        rows = list(rows)
        if not rows:
            return 0
        tenant_id = rows[0].tenant_id
        with self.db.as_system(tenant_id) as conn:
            for row in rows:
                if row.tenant_id != tenant_id:
                    # One transaction carries one tenant's context, so a mixed batch
                    # would silently write nothing for the others: the WITH CHECK
                    # predicate on coverage_daily refuses the row rather than
                    # crossing the boundary. Refuse it here instead, loudly.
                    raise ValueError("coverage rows for two tenants in one batch")
                conn.execute(_UPSERT_COVERAGE, (
                    row.tenant_id, row.user_uid, row.day, row.dialer_calls,
                    row.captured_calls, row.dialer_minutes, row.captured_minutes,
                    row.gap_reason,
                ))
                written += 1
        return written


def _local_day_bounds(day: date, tz: str) -> tuple[datetime, datetime]:
    from zoneinfo import ZoneInfo  # noqa: PLC0415 - stdlib, but only this path needs it

    try:
        zone = ZoneInfo(tz)
    except Exception:  # noqa: BLE001 - a bad tz in tenant config must not stop the job
        log.warning("unknown tenant timezone; falling back to UTC", extra={"timezone": tz})
        zone = timezone.utc
    start = datetime(day.year, day.month, day.day, tzinfo=zone)
    return start, start + timedelta(days=1)
