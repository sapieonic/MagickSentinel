"""The persistence layer against a real, migrated PostgreSQL.

Skipped unless ``SENTINEL_PIPELINE_TEST_DATABASE_URL`` and
``SENTINEL_PIPELINE_TEST_ADMIN_DATABASE_URL`` are set, which keeps
``python -m pytest`` runnable with no database — the same pattern the gateway's Go
integration tests follow, and for the same reason: the default suite has to stay
something anyone can run.

``tests/pg_integration.sh`` boots a throwaway cluster, applies every migration and
sets both variables.

What this covers that the fake-connection tests cannot:

* that every statement in :mod:`sentinel_pipeline.persistence` actually parses and
  runs against the schema as it is today,
* that the ``sentinel_pipeline`` role can do what the pipeline needs and **nothing
  wider** — the isolation assertions here are the pipeline's half of
  ``db/test/rls_test.sh``, which covers ``sentinel_app``,
* that the upserts really are idempotent in Postgres, including the two cases where a
  re-run must lose to a human: a reviewed flag and a corrected promise-to-pay,
* and that ``db/migrations/0008``'s tenant-listing function is executable by this
  role and returns the retention periods the purge reads (OPEN-6).

The application DSN connects as ``sentinel_pipeline``; the admin DSN is the schema
owner and is used only to seed fixtures and to read back rows the pipeline role must
not be able to see.
"""

import os
from datetime import date, datetime, timedelta, timezone

import pytest

from sentinel_pipeline.db import Database, DatabaseConfig, call_uuid
from sentinel_pipeline.models import (
    Analysis,
    CallContext,
    Channel,
    ChannelTranscript,
    Disposition,
    Finding,
    PromiseToPay,
    Severity,
    Transcript,
    Word,
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

APP_DSN = os.environ.get("SENTINEL_PIPELINE_TEST_DATABASE_URL")
ADMIN_DSN = os.environ.get("SENTINEL_PIPELINE_TEST_ADMIN_DATABASE_URL")

pytestmark = pytest.mark.skipif(
    not (APP_DSN and ADMIN_DSN),
    reason="set SENTINEL_PIPELINE_TEST_DATABASE_URL and "
           "SENTINEL_PIPELINE_TEST_ADMIN_DATABASE_URL (see tests/pg_integration.sh)",
)

T1 = "11111111-1111-1111-1111-111111111111"
T2 = "22222222-2222-2222-2222-222222222222"
CALL = "01J8ZQ8H2Q7X9K3M4N5P6R7S8T"
DEVICE = "dddddddd-0000-0000-0000-000000000001"
STARTED = datetime(2026, 9, 1, 5, 30, tzinfo=timezone.utc)

# Statements are applied one at a time: psycopg refuses multiple commands in a
# statement that carries parameters, and the parameters are what keep the fixture
# readable next to the constants above.
CLEAN = """
DELETE FROM coverage_daily; DELETE FROM audit_log; DELETE FROM flags;
DELETE FROM ptps; DELETE FROM analyses; DELETE FROM transcripts;
DELETE FROM media_segments; DELETE FROM calls; DELETE FROM devices;
DELETE FROM rule_sets; DELETE FROM users; DELETE FROM tenants;
"""

SEED = [
    """INSERT INTO tenants (id, name, idp_tenant_id, policy) VALUES
         (%(t1)s, 'Acme BPO', 'acme',
          '{"language":"hi-IN","cdr_agent_map":{"AG-1":"agent-a"}}'),
         (%(t2)s, 'Rival BPO', 'rival', '{}')""",
    """INSERT INTO users (firebase_uid, tenant_id, role, display_name) VALUES
         ('agent-a', %(t1)s, 'agent', 'Agent A'),
         ('rival-a', %(t2)s, 'agent', 'Rival A')""",
    """INSERT INTO devices (id, tenant_id, machine_guid, hw_fingerprint,
                            cert_fingerprint, os_build, capture_tier, agent_version)
       VALUES (%(device)s, %(t1)s, 'mg-1', 'hw-1', 'cf-1', '10.0.22631', 'B', '1.0.0')""",
    """INSERT INTO calls (id, tenant_id, device_id, user_uid, started_at, duration_ms,
                          account_ref, capture_tier, status)
       VALUES (%(call)s, %(t1)s, %(device)s, 'agent-a', %(started)s, 300000,
               'LN-88213', 'B', 'transcribing')""",
    # One segment per case the reader has to distinguish: a current non-foreign
    # segment, a current foreign one that must never be transcribed, and an expired
    # one for the purge.
    """INSERT INTO media_segments (tenant_id, call_id, channel, seq, s3_key, bytes,
                                   duration_ms, timestamp_ms, foreign_audio, received_at)
       VALUES
         (%(t1)s, %(call)s, 1, 0, 'audio/t1/2026-09-01/c/1/00000000.opus', 100, 1000,
          0, false, now()),
         (%(t1)s, %(call)s, 0, 0, 'audio/t1/2026-09-01/c/0/00000000.opus', 100, 1000,
          0, true, now()),
         (%(t1)s, %(call)s, 0, 1, 'audio/t1/2026-06-01/c/0/00000001.opus', 100, 1000,
          1000, false, now() - interval '90 days')""",
    """INSERT INTO rule_sets (tenant_id, version, definition, active, created_by)
       SELECT t.id, 1, d.definition, true, 'system'
         FROM tenants t CROSS JOIN default_rule_set d""",
]


@pytest.fixture()
def admin():
    import psycopg

    with psycopg.connect(ADMIN_DSN, autocommit=True) as conn:
        yield conn


@pytest.fixture()
def db(admin):
    params = {"t1": T1, "t2": T2, "call": call_uuid(CALL), "device": DEVICE,
              "started": STARTED}
    admin.execute(CLEAN)
    for statement in SEED:
        admin.execute(statement, params)
    database = Database(DatabaseConfig(dsn=APP_DSN)).open()
    try:
        yield database
    finally:
        database.close()


@pytest.fixture()
def context(db) -> CallContext:
    return PostgresCallRepository(db).call_context(T1, CALL)


def transcript_for(ctx: CallContext) -> Transcript:
    return Transcript(context=ctx, channels={
        Channel.NEAR: ChannelTranscript(
            channel=Channel.NEAR, text="hello there",
            words=[Word("hello", 0, 400, 0.9), Word("there", 400, 800, 0.8)],
            language="hi", provider="fake-asr", provider_version="1", confidence=0.9),
    })


def analysis_for() -> Analysis:
    return Analysis(
        summary="The agent discussed an overdue amount.",
        disposition=Disposition.PTP,
        ptp=PromiseToPay(True, 1_500_000, date(2026, 9, 15), 0.8, (1_000, 2_000)),
        sentiment={"far": [], "near": []}, talk_ratio=0.5, interruptions=1,
        model="fake-analysis", prompt_version="analysis-v1",
        input_tokens=10, output_tokens=5,
    )


FINDING = Finding("false_legal_threat", Severity.CRITICAL, 1, 100, 200,
                  "we will file a police case")


# ------------------------------------------------------------------- the role


def test_the_pipeline_connects_as_a_role_that_cannot_bypass_rls(db):
    # If this ever passes while connected as the owner, every other assertion in this
    # file is meaningless.
    db.assert_rls_enforced()


def test_a_query_with_no_tenant_context_returns_zero_rows_not_everything(db):
    # The property db/test/rls_test.sh asserts for sentinel_app, asserted here for
    # sentinel_pipeline. This is the failure mode the whole design is built around.
    with db.without_tenant("proving the failure mode") as conn:
        assert conn.execute("SELECT count(*) FROM calls").fetchone()[0] == 0
        assert conn.execute("SELECT count(*) FROM media_segments").fetchone()[0] == 0
        assert conn.execute("SELECT count(*) FROM transcripts").fetchone()[0] == 0


def test_another_tenants_context_sees_none_of_this_tenants_rows(db):
    with db.as_system(T2) as conn:
        assert conn.execute("SELECT count(*) FROM calls").fetchone()[0] == 0
    assert PostgresSegmentIndex(db, T2).segments_for(CALL, Channel.NEAR) == []
    with pytest.raises(CallNotFound):
        PostgresCallRepository(db).call_context(T2, CALL)


# ------------------------------------------------------------ migration 0008


def test_the_tenant_listing_function_is_executable_and_carries_the_periods(db):
    tenants = {t.tenant_id: t for t in list_tenants(db)}
    assert set(tenants) == {T1, T2}
    # OPEN-6: read per tenant, never hard-coded. These are the schema placeholders.
    assert tenants[T1].audio_retention_days == 30
    assert tenants[T1].transcript_retention_days == 365
    assert tenants[T1].timezone == "Asia/Kolkata"


def test_the_gateways_role_cannot_enumerate_tenants_through_that_function(db, admin):
    # EXECUTE on a new function defaults to PUBLIC, which would have handed the
    # customer list to sentinel_app and to anything else with a login. 0008 revokes
    # it and grants the pipeline alone — the gateway never needs it, because every
    # gateway request already knows its tenant from a verified token.
    import psycopg

    admin.execute("SET ROLE sentinel_app")
    try:
        with pytest.raises(psycopg.errors.InsufficientPrivilege):
            admin.execute("SELECT * FROM sentinel_pipeline_tenants()").fetchall()
    finally:
        admin.execute("RESET ROLE")


# ----------------------------------------------------------------- reading


def test_foreign_audio_is_excluded_by_the_query_itself(db):
    index = PostgresSegmentIndex(db, T1)
    assert [r.seq for r in index.segments_for(CALL, Channel.FAR)] == [1]
    assert [r.seq for r in index.segments_for(CALL, Channel.NEAR)] == [0]


def test_the_call_context_is_read_whole(context):
    assert context.user_uid == "agent-a"
    assert context.duration_ms == 300_000
    assert context.account_ref == "LN-88213"
    assert context.capture_tier == "B"
    assert context.language == "hi-IN"


def test_the_active_rule_set_and_the_budget_come_back(db):
    repo = PostgresCallRepository(db)
    assert repo.rule_engine(T1).rule_set.version == 1
    budget = repo.budget(T1)
    assert budget.spent_paise == 0 and budget.kill_switch is False


# ----------------------------------------------------------------- writing


def test_every_write_is_idempotent_under_redelivery(db, context, admin):
    sink = PostgresSink(db, T1)
    for _ in range(2):
        sink.save_transcript(CALL, transcript_for(context))
        sink.save_analysis(CALL, analysis_for(), 417)
        sink.save_findings(CALL, 1, [FINDING])
        sink.set_status(CALL, "complete")

    counts = admin.execute(
        "SELECT (SELECT count(*) FROM transcripts), (SELECT count(*) FROM analyses),"
        "       (SELECT count(*) FROM ptps), (SELECT count(*) FROM flags)").fetchone()
    assert counts == (1, 1, 1, 1)
    assert admin.execute("SELECT status, has_flags FROM calls").fetchone() == \
        ("complete", True)


def test_word_timings_are_stored_in_the_shape_the_portal_reads(db, context, admin):
    PostgresSink(db, T1).save_transcript(CALL, transcript_for(context))
    row = admin.execute("SELECT word_timings->0 FROM transcripts").fetchone()[0]
    # store/queries.go's transcriptTurns falls back to whole-channel text when these
    # keys are not what it expects, so a rename empties the transcript view silently.
    assert set(row) >= {"text", "start_ms", "end_ms"}


def test_money_stays_an_integer_number_of_paise(db, context, admin):
    PostgresSink(db, T1).save_analysis(CALL, analysis_for(), 417)
    cost = admin.execute("SELECT cost_paise FROM analyses").fetchone()[0]
    assert cost == 417 and isinstance(cost, int)


def test_a_reviewers_decision_survives_a_re_run_that_no_longer_finds_it(db, context, admin):
    sink = PostgresSink(db, T1)
    sink.save_findings(CALL, 1, [FINDING])
    admin.execute("UPDATE flags SET status='dismissed', reviewer_uid='qa-1', "
                  "reviewer_note='not a threat in context'")

    sink.save_findings(CALL, 2, [])   # the rule set changed; the rule no longer fires

    assert admin.execute("SELECT status, reviewer_uid FROM flags").fetchone() == \
        ("dismissed", "qa-1")


def test_an_untouched_flag_is_cleared_by_a_re_run_that_no_longer_produces_it(db, admin):
    sink = PostgresSink(db, T1)
    sink.save_findings(CALL, 1, [FINDING])
    sink.save_findings(CALL, 2, [])
    assert admin.execute("SELECT count(*) FROM flags").fetchone()[0] == 0


def test_an_agents_corrected_promise_to_pay_is_never_overwritten(db, admin):
    sink = PostgresSink(db, T1)
    sink.save_analysis(CALL, analysis_for(), 1)
    admin.execute("UPDATE ptps SET agent_confirmed = true, agent_amount_paise = 999, "
                  "corrected_at = now()")

    sink.save_analysis(CALL, analysis_for(), 2)

    # The agent's figure is the record; ours is a guess about it.
    assert admin.execute("SELECT agent_amount_paise FROM ptps").fetchone()[0] == 999


def test_a_re_run_with_no_promise_retracts_only_the_uncorrected_one(db, admin):
    sink = PostgresSink(db, T1)
    sink.save_analysis(CALL, analysis_for(), 1)
    no_ptp = analysis_for()
    no_ptp.ptp = PromiseToPay(present=False)
    sink.save_analysis(CALL, no_ptp, 1)
    assert admin.execute("SELECT count(*) FROM ptps").fetchone()[0] == 0


# --------------------------------------------------------------- retention


def test_the_purge_deletes_expired_rows_and_keeps_the_rest(db, admin):
    store = PostgresRetentionStore(db)
    cutoff = datetime.now(timezone.utc) - timedelta(days=30)

    assert store.count_audio_before(T1, cutoff) == 1
    keys = store.audio_keys_before(T1, cutoff, 100)
    assert store.delete_media_rows(T1, [(k[0], k[1], k[2]) for k in keys]) == 1
    assert store.count_audio_before(T1, cutoff) == 0
    # The two recent segments, including the foreign one, are still there: retention
    # is about age, and a foreign segment is kept as the record of what was discarded.
    assert admin.execute("SELECT count(*) FROM media_segments").fetchone()[0] == 2


def test_the_purge_writes_one_audit_entry_carrying_counts_only(db, admin):
    PostgresRetentionStore(db).audit(T1, "retention.purge",
                                     {"audio_segments": 1, "transcripts": 0})
    row = admin.execute("SELECT action, actor_uid, detail FROM audit_log").fetchone()
    assert row[0] == "retention.purge" and row[1] == "system"
    assert row[2] == {"audio_segments": 1, "transcripts": 0}


def test_transcripts_are_deleted_on_their_own_much_longer_clock(db, context, admin):
    PostgresSink(db, T1).save_transcript(CALL, transcript_for(context))
    store = PostgresRetentionStore(db)
    # Nothing is 365 days old, so the transcript survives an audio purge.
    old = datetime.now(timezone.utc) - timedelta(days=365)
    assert store.count_transcripts_before(T1, old) == 0
    assert store.delete_transcripts_before(T1, old, 100) == 0
    assert admin.execute("SELECT count(*) FROM transcripts").fetchone()[0] == 1


# ---------------------------------------------------------------- coverage


def test_coverage_reads_our_side_and_writes_the_reconciled_row(db, admin):
    from sentinel_pipeline.coverage import CdrCall, reconcile

    store = PostgresCoverageStore(db)
    assert store.agent_map(T1) == {"AG-1": "agent-a"}

    captured = store.captured_calls(T1, date(2026, 9, 1), "Asia/Kolkata")
    assert [c.user_uid for c in captured] == ["agent-a"]

    rows = reconcile(T1, date(2026, 9, 1),
                     [CdrCall("AG-1", STARTED, 300_000),
                      CdrCall("AG-1", STARTED + timedelta(hours=1), 60_000)],
                     captured, agent_for=store.agent_map(T1))
    assert store.write_coverage(rows) == 1
    assert store.write_coverage(rows) == 1, "a re-run corrects rather than duplicating"

    stored = admin.execute("SELECT dialer_calls, captured_calls, gap_reason "
                           "FROM coverage_daily").fetchone()
    assert stored[0] == 2 and stored[1] == 1
    assert "device events" in stored[2]
