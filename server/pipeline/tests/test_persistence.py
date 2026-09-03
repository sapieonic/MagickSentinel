"""The Postgres implementations, against a recording fake connection.

Two classes of assertion here, and the second is the reason this file exists rather
than being left to an integration test:

* **Row mapping and parameters** — that a transcript's word timings go in with the
  key names the gateway reads back, that money stays an integer, that a call id
  becomes the uuid the schema is keyed by.
* **Discipline** — that every statement ran inside a tenant context, that the one
  statement allowed to run without one is the SECURITY DEFINER tenant listing, and
  that no upsert touches a column a human owns. A live-database test passes happily
  while a re-run silently erases a reviewer's verdict; these do not.
"""

from datetime import date, datetime, timedelta, timezone

import pytest
from conftest import COMPLIANT_OPENING, call, channel
from fakedb import FakeDatabase

from sentinel_pipeline.models import (
    Analysis,
    Channel,
    Disposition,
    Finding,
    PromiseToPay,
    Severity,
)
from sentinel_pipeline.persistence import (
    CallNotFound,
    PostgresCallRepository,
    PostgresCoverageStore,
    PostgresRetentionStore,
    PostgresSegmentIndex,
    PostgresSink,
    list_tenants,
)

TENANT = "11111111-1111-1111-1111-111111111111"
CALL = "01J8ZQ8H2Q7X9K3M4N5P6R7S8T"
CALL_UUID = "01923f74-4457-3f53-31d0-952d8d83e51a"
NOW = datetime(2026, 9, 1, 6, 0, tzinfo=timezone.utc)


def analysis(present: bool = True) -> Analysis:
    return Analysis(
        summary="The agent discussed an overdue amount and the borrower responded.",
        disposition=Disposition.PTP if present else Disposition.NO_CONTACT,
        ptp=PromiseToPay(present=present, amount_paise=1_500_000,
                         due_date=date(2026, 9, 15), confidence=0.8,
                         evidence_span_ms=(1_000, 2_000)),
        sentiment={"far": [], "near": [], "delta": 0.1},
        talk_ratio=0.55,
        interruptions=2,
        next_action="Call back on the fifteenth.",
        model="claude-sonnet-5",
        prompt_version="analysis-v1",
        input_tokens=1_200,
        output_tokens=300,
    )


# ----------------------------------------------------------------- segment index


def test_the_segment_query_excludes_foreign_audio_in_sql_as_well_as_in_python():
    # The Python filter in segments.transcribable is the backstop; this is the first
    # line. Both exist because one missed check transcribes an agent's music.
    db = FakeDatabase().on("media_segments", [(0, "audio/t/d/c/00000000.opus", False)])
    index = PostgresSegmentIndex(db, TENANT)
    rows = index.segments_for(CALL, Channel.FAR)

    assert [r.s3_key for r in rows] == ["audio/t/d/c/00000000.opus"]
    statement = db.sql_for("media_segments")[0]
    assert "NOT foreign_audio" in statement.squashed
    assert "ORDER BY seq" in statement.squashed
    # Keyed by the uuid form of the ULID, and scoped to one tenant's context.
    assert statement.params == (CALL_UUID, 0)
    assert statement.context == (TENANT, "system", "admin")


def test_the_segment_index_asks_for_the_channel_it_was_given():
    db = FakeDatabase().on("media_segments", [])
    PostgresSegmentIndex(db, TENANT).segments_for(CALL, Channel.NEAR)
    assert db.sql_for("media_segments")[0].params[1] == 1


# ------------------------------------------------------------------------- sink


def test_a_transcript_is_written_per_channel_with_the_key_names_the_portal_reads():
    transcript = call(near=channel(Channel.NEAR, (0, COMPLIANT_OPENING)),
                      far=channel(Channel.FAR, (0, "haan theek hai")))
    db = FakeDatabase()
    PostgresSink(db, TENANT).save_transcript(CALL, transcript)

    writes = db.sql_for("INSERT INTO transcripts")
    assert len(writes) == 2
    assert "ON CONFLICT (call_id, channel) DO UPDATE" in writes[0].squashed
    # store/queries.go's transcriptTurns reads start_ms, end_ms and text and falls
    # back to whole-channel text when the JSON is not what it expects — so renaming
    # one of these empties the portal's transcript view rather than failing.
    timings = writes[0].params[7]
    assert '"start_ms"' in timings and '"end_ms"' in timings and '"text"' in timings
    # No embedding column: nothing computes one, and a zero vector would make
    # semantic search return every call rather than none.
    assert "embedding" not in writes[0].squashed


def test_an_analysis_and_its_promise_to_pay_are_written_together():
    db = FakeDatabase()
    PostgresSink(db, TENANT).save_analysis(CALL, analysis(), cost_paise=417)

    written = db.sql_for("INSERT INTO analyses")[0]
    assert written.params[0] == CALL_UUID and written.params[1] == TENANT
    # Money is an integer number of paise, end to end.
    assert written.params[-2] == 417 and isinstance(written.params[-2], int)

    ptp = db.sql_for("INSERT INTO ptps")[0]
    assert ptp.params[2] == 1_500_000
    assert ptp.params[5] == "[1000,2000)"


def test_a_re_run_never_overwrites_an_agents_corrected_promise_to_pay():
    db = FakeDatabase()
    PostgresSink(db, TENANT).save_analysis(CALL, analysis(), cost_paise=1)
    ptp = db.sql_for("INSERT INTO ptps")[0].squashed
    # The conflict action is conditional: once corrected_at is set, the extraction
    # has lost the argument permanently.
    assert "WHERE ptps.corrected_at IS NULL" in ptp
    assert "agent_amount_paise" not in ptp and "agent_confirmed" not in ptp


def test_a_re_run_that_no_longer_sees_a_promise_retracts_the_uncorrected_one():
    db = FakeDatabase()
    PostgresSink(db, TENANT).save_analysis(CALL, analysis(present=False), cost_paise=1)
    assert not db.sql_for("INSERT INTO ptps")
    deleted = db.sql_for("DELETE FROM ptps")[0].squashed
    assert "corrected_at IS NULL" in deleted


def test_no_evidence_span_is_written_as_null_not_as_an_unbounded_range():
    # int4range(NULL, NULL) reads as "the evidence is the whole call", which is not
    # what "we could not point at the words" means.
    data = analysis()
    data.ptp = PromiseToPay(present=True, amount_paise=100, confidence=0.5)
    db = FakeDatabase()
    PostgresSink(db, TENANT).save_analysis(CALL, data, cost_paise=1)
    assert db.sql_for("INSERT INTO ptps")[0].params[5] is None


def findings() -> list[Finding]:
    return [
        Finding(rule_id="false_legal_threat", severity=Severity.CRITICAL, tier=1,
                span_start_ms=30_000, span_end_ms=34_000,
                evidence_text="we will file a police case"),
        Finding(rule_id="abusive_language", severity=Severity.HIGH, tier=2,
                span_start_ms=1_000, span_end_ms=2_000, evidence_text="…",
                rationale="upheld", confidence=0.9),
    ]


def test_findings_are_upserted_and_a_reviewers_work_is_untouchable():
    db = FakeDatabase()
    PostgresSink(db, TENANT).save_findings(CALL, 3, findings())

    writes = db.sql_for("INSERT INTO flags")
    assert len(writes) == 2
    updated = writes[0].squashed.split("DO UPDATE", 1)[1]
    # These columns belong to a human. A redelivered NATS message must not be able
    # to reopen a dismissed flag or erase a reviewer's note.
    for human_column in ("status", "reviewer_uid", "reviewer_note", "agent_response",
                         "resolved_at"):
        assert human_column not in updated, f"a re-run would overwrite {human_column}"
    assert writes[0].params[3] == 3, "the rule set version travels with the flag"


def test_stale_flags_are_removed_only_while_nobody_has_touched_them():
    db = FakeDatabase()
    PostgresSink(db, TENANT).save_findings(CALL, 3, findings())

    delete = db.sql_for("DELETE FROM flags")[0]
    assert delete.params[1] == ["false_legal_threat:1", "abusive_language:2"]
    squashed = delete.squashed
    assert "status = 'open'" in squashed
    assert "reviewer_uid IS NULL" in squashed
    assert "agent_response IS NULL" in squashed


def test_a_call_that_now_produces_no_findings_clears_its_untouched_flags():
    db = FakeDatabase()
    PostgresSink(db, TENANT).save_findings(CALL, 4, [])
    assert not db.sql_for("INSERT INTO flags")
    assert db.sql_for("DELETE FROM flags")[0].params[1] == []


def test_the_insert_happens_before_the_delete_in_one_transaction():
    # A reader landing between the two sees the old flags and the new ones, rather
    # than a call that briefly has no compliance findings at all.
    db = FakeDatabase()
    PostgresSink(db, TENANT).save_findings(CALL, 1, findings())
    kinds = [s.squashed.split()[0] for s in db.statements if "flags" in s.squashed]
    assert kinds == ["INSERT", "INSERT", "DELETE"]


def test_status_updates_are_scoped_to_the_call_and_the_tenant_context():
    db = FakeDatabase()
    PostgresSink(db, TENANT).set_status(CALL, "complete")
    statement = db.sql_for("UPDATE calls")[0]
    assert statement.params == ("complete", CALL_UUID)
    assert statement.context == (TENANT, "system", "admin")


def test_every_sink_statement_runs_inside_a_tenant_context():
    db = FakeDatabase()
    sink = PostgresSink(db, TENANT)
    sink.save_transcript(CALL, call(near=channel(Channel.NEAR, (0, "hello"))))
    sink.save_analysis(CALL, analysis(), 10)
    sink.save_findings(CALL, 1, findings())
    sink.set_status(CALL, "complete")

    assert all(s.context == (TENANT, "system", "admin") for s in db.statements)
    assert db.tenants_seen() == {TENANT}


# ------------------------------------------------------------------- repository


def call_row(policy="{}"):
    return (
        "agent-a", NOW, 300_000, "LN-88213", "outbound", "A", "Asia/Kolkata", policy,
    )


def test_the_call_context_comes_entirely_from_the_database():
    # The bus message carries four identifiers; everything the pipeline reasons about
    # is read here, under this tenant's RLS context.
    db = FakeDatabase()
    db.on("FROM calls c", [call_row('{"language": "ta-IN"}')])
    db.on("SELECT count(*)", [(2,)])

    ctx = PostgresCallRepository(db).call_context(TENANT, CALL)
    assert ctx.call_id == CALL and ctx.tenant_id == TENANT
    assert ctx.user_uid == "agent-a" and ctx.duration_ms == 300_000
    assert ctx.account_ref == "LN-88213" and ctx.prior_contacts_24h == 2
    # The floor's language selects the transcriber (providers/registry.py). Wrong
    # here means Tamil audio going to a model that has no Tamil.
    assert ctx.language == "ta-IN"


def test_a_missing_language_leaves_detection_to_the_provider():
    db = FakeDatabase().on("FROM calls c", [call_row()]).on("SELECT count(*)", [(0,)])
    assert PostgresCallRepository(db).call_context(TENANT, CALL).language is None


def test_prior_contacts_are_not_counted_when_there_is_no_account_reference():
    # The repeat-contact rule needs an account reference to count against; without
    # one there is nothing to group by and the query is skipped rather than counting
    # every call on the floor.
    row = list(call_row())
    row[3] = None
    db = FakeDatabase().on("FROM calls c", [tuple(row)])
    ctx = PostgresCallRepository(db).call_context(TENANT, CALL)
    assert ctx.prior_contacts_24h == 0
    assert not db.sql_for("SELECT count(*)")


def test_a_call_another_tenant_owns_is_indistinguishable_from_one_that_is_absent():
    # That is the RLS design working: the query returns no rows either way, and the
    # pipeline must treat both as permanent rather than retrying.
    db = FakeDatabase().on("FROM calls c", [])
    with pytest.raises(CallNotFound):
        PostgresCallRepository(db).call_context(TENANT, CALL)


def test_the_active_rule_set_is_used_when_the_tenant_has_one():
    definition = {"rules": [{"rule_id": "abusive_language", "severity": "high"}],
                  "judge_sample_pct": 2.5}
    db = FakeDatabase().on("FROM rule_sets", [(7, definition)])
    engine = PostgresCallRepository(db).rule_engine(TENANT)
    assert engine.rule_set.version == 7
    assert engine.rule_set.judge_sample_pct == 2.5


def test_a_tenant_with_no_active_rule_set_falls_back_to_the_shipped_defaults():
    # Loud, and then the defaults: running no rules would produce a call that looks
    # reviewed and carries no findings.
    db = FakeDatabase().on("FROM rule_sets", [])
    engine = PostgresCallRepository(db).rule_engine(TENANT)
    assert len(engine.rule_set.rules) == 10


def test_the_budget_is_this_months_spend_against_the_tenants_cap():
    db = FakeDatabase().on("FROM tenants t", [(10_000_000, "false", 7_000_000)])
    budget = PostgresCallRepository(db).budget(TENANT)
    assert budget.monthly_budget_paise == 10_000_000
    assert budget.spent_paise == 7_000_000
    assert budget.kill_switch is False
    assert budget.state.value == "warn_70"


def test_the_kill_switch_is_read_from_tenant_policy():
    db = FakeDatabase().on("FROM tenants t", [(None, "true", 0)])
    budget = PostgresCallRepository(db).budget(TENANT)
    assert budget.kill_switch is True
    assert budget.monthly_budget_paise is None


# ---------------------------------------------------------------- tenant listing


def test_listing_tenants_is_the_one_query_with_no_tenant_context():
    db = FakeDatabase().on("sentinel_pipeline_tenants", [(TENANT, 30, 365, "Asia/Kolkata")])
    tenants = list_tenants(db)
    assert [t.tenant_id for t in tenants] == [TENANT]
    assert tenants[0].audio_retention_days == 30
    # No context, because it is the query that produces the contexts — and it goes
    # through the SECURITY DEFINER function rather than through a loosened policy.
    assert db.statements[0].context is None
    assert "sentinel_pipeline_tenants()" in db.statements[0].squashed


# -------------------------------------------------------------------- retention


def test_expired_audio_is_selected_by_arrival_and_scoped_to_the_tenant():
    cutoff = NOW - timedelta(days=30)
    db = FakeDatabase().on("FROM media_segments",
                           [(CALL_UUID, 0, 5, "audio/t/2026-07-01/c/00000005.opus")])
    rows = PostgresRetentionStore(db).audio_keys_before(TENANT, cutoff, 100)
    assert rows == [(CALL_UUID, 0, 5, "audio/t/2026-07-01/c/00000005.opus")]
    statement = db.sql_for("FROM media_segments")[0]
    assert statement.params == (TENANT, cutoff, 100)
    assert statement.context[0] == TENANT


def test_media_rows_are_deleted_by_their_full_primary_key():
    db = FakeDatabase()
    PostgresRetentionStore(db).delete_media_rows(
        TENANT, [(CALL_UUID, 0, 1), (CALL_UUID, 1, 2)])
    statement = db.sql_for("DELETE FROM media_segments")[0]
    assert statement.params == ([CALL_UUID, CALL_UUID], [0, 1], [1, 2], TENANT)
    assert "m.channel = k.channel" in statement.squashed


def test_deleting_no_rows_does_not_issue_a_statement():
    db = FakeDatabase()
    assert PostgresRetentionStore(db).delete_media_rows(TENANT, []) == 0
    assert db.statements == []


def test_transcript_deletion_is_bounded_by_the_batch_size():
    db = FakeDatabase()
    PostgresRetentionStore(db).delete_transcripts_before(TENANT, NOW, 500)
    assert db.sql_for("DELETE FROM transcripts")[0].params == (TENANT, NOW, 500)


def test_the_purge_audit_entry_carries_counts_and_no_identifiers():
    db = FakeDatabase()
    PostgresRetentionStore(db).audit(TENANT, "retention.purge",
                                    {"audio_segments": 12, "transcripts": 3})
    statement = db.sql_for("INSERT INTO audit_log")[0]
    assert statement.params[1] == "retention.purge"
    assert statement.params[3] == '{"audio_segments": 12, "transcripts": 3}'
    assert "'system'" in statement.squashed


# --------------------------------------------------------------------- coverage


def test_captured_calls_are_bounded_by_the_tenants_own_local_day():
    # A collections floor works 08:00-19:00 IST. A UTC day boundary would cut the
    # evening shift in half and report a coverage gap that is really a timezone.
    db = FakeDatabase().on("FROM calls", [("agent-a", NOW, 120_000, "LN-1", None)])
    rows = PostgresCoverageStore(db).captured_calls(TENANT, date(2026, 9, 1),
                                                    "Asia/Kolkata")
    assert [r.user_uid for r in rows] == ["agent-a"]
    start, end = db.sql_for("FROM calls")[0].params[1:3]
    assert start.isoformat() == "2026-09-01T00:00:00+05:30"
    assert end.isoformat() == "2026-09-02T00:00:00+05:30"


def test_an_unknown_tenant_timezone_falls_back_to_utc_rather_than_failing_the_job():
    db = FakeDatabase().on("FROM calls", [])
    PostgresCoverageStore(db).captured_calls(TENANT, date(2026, 9, 1), "Mars/Olympus")
    assert db.sql_for("FROM calls")[0].params[1].utcoffset().total_seconds() == 0


def test_discarded_calls_are_not_counted_as_captured():
    db = FakeDatabase().on("FROM calls", [])
    PostgresCoverageStore(db).captured_calls(TENANT, date(2026, 9, 1))
    assert "status <> 'discarded'" in db.sql_for("FROM calls")[0].squashed


def test_the_dialer_agent_map_comes_from_tenant_policy():
    db = FakeDatabase().on("SELECT policy", [({"cdr_agent_map": {"AG-1": "uid-1"}},)])
    assert PostgresCoverageStore(db).agent_map(TENANT) == {"AG-1": "uid-1"}


def test_a_missing_agent_map_is_empty_rather_than_an_error():
    db = FakeDatabase().on("SELECT policy", [({},)])
    assert PostgresCoverageStore(db).agent_map(TENANT) == {}


def test_coverage_rows_are_upserted_so_a_re_run_corrects_rather_than_duplicates():
    from sentinel_pipeline.coverage import Coverage

    rows = [Coverage(TENANT, "uid-1", date(2026, 9, 1), 10, 9, 40, 36, None)]
    db = FakeDatabase()
    assert PostgresCoverageStore(db).write_coverage(rows) == 1
    statement = db.sql_for("INSERT INTO coverage_daily")[0]
    assert "ON CONFLICT (tenant_id, user_uid, date) DO UPDATE" in statement.squashed
    assert statement.params[:5] == (TENANT, "uid-1", date(2026, 9, 1), 10, 9)


def test_a_batch_mixing_two_tenants_is_refused_rather_than_silently_dropped():
    # One transaction carries one tenant's context, so the WITH CHECK predicate on
    # coverage_daily would refuse the other tenant's rows — writing nothing, quietly.
    from sentinel_pipeline.coverage import Coverage

    rows = [Coverage(TENANT, "uid-1", date(2026, 9, 1), 1, 1, 1, 1, None),
            Coverage("other-tenant", "uid-2", date(2026, 9, 1), 1, 1, 1, 1, None)]
    with pytest.raises(ValueError):
        PostgresCoverageStore(FakeDatabase()).write_coverage(rows)
