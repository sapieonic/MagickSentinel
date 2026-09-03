"""The composition root: the bus message, and one finalize end to end.

The interesting test is ``test_a_finalize_writes_the_whole_compliance_record``. It
runs the production path — the real ``Finalizer``, the real Postgres sink and segment
index, the real object-store reader — against a recording fake connection and an
in-memory bucket, with only the model providers faked. That is the wiring that did
not exist at all before: a worker that was a pure function, a consumer with no
producer, and no code anywhere that turned a message into a compliance record.
"""

import asyncio
import json
import logging
from datetime import date, datetime, timezone

import pytest
from conftest import COMPLIANT_OPENING
from fakedb import FakeDatabase

from sentinel_pipeline.analysis import CallAnalyzer
from sentinel_pipeline.blobstore import MemoryBlobStore, segment_key
from sentinel_pipeline.compliance.judge import ComplianceJudge
from sentinel_pipeline.consumer import Unprocessable
from sentinel_pipeline.cost import CostPolicy, ModelPricing
from sentinel_pipeline.providers import FakeAnalysisProvider, FakeASR, FakeJudgeProvider
from sentinel_pipeline.segments import SegmentCodec
from sentinel_pipeline.service import (
    FinalizeMessage,
    FinalizeService,
    configure_logging,
    pricing_from_env,
    run_coverage,
)

TENANT = "11111111-1111-1111-1111-111111111111"
CALL = "01J8ZQ8H2Q7X9K3M4N5P6R7S8T"
CALL_UUID = "01923f74-4457-3f53-31d0-952d8d83e51a"
STARTED = datetime(2026, 9, 1, 5, 30, tzinfo=timezone.utc)  # 11:00 IST, inside hours

THREAT = COMPLIANT_OPENING + " If you do not pay we will file a police case against you."

PRICING = {
    "fake-analysis": ModelPricing("fake-analysis", 25_000, 125_000),
    "fake-judge": ModelPricing("fake-judge", 25_000, 125_000),
}


# ------------------------------------------------------------------- the message


def test_the_four_fields_the_gateway_publishes_are_the_whole_message():
    message = FinalizeMessage.from_payload({
        "call_id": CALL, "tenant_id": TENANT, "attempt": 2,
        "finalized_at": "2026-09-01T10:19:44.802Z",
    })
    assert message.call_id == CALL and message.tenant_id == TENANT
    assert message.attempt == 2 and message.finalized_at.endswith("Z")


def test_an_unexpected_field_is_ignored_rather_than_stalling_the_stream():
    # Forward compatibility with a producer that grows a field. Nothing in the
    # pipeline reads anything but the four.
    message = FinalizeMessage.from_payload({"call_id": CALL, "tenant_id": TENANT,
                                            "shard": "mumbai-2"})
    assert message.call_id == CALL and message.attempt == 0


@pytest.mark.parametrize("payload", [
    {}, {"call_id": CALL}, {"tenant_id": TENANT}, {"call_id": "", "tenant_id": TENANT},
])
def test_a_message_with_nothing_to_look_up_is_permanently_unprocessable(payload):
    with pytest.raises(Unprocessable):
        FinalizeMessage.from_payload(payload)


def test_a_nonsense_attempt_counter_does_not_fail_the_call():
    # It is observability, not an input. Refusing the message over it would drop a
    # compliance record for a cosmetic reason.
    assert FinalizeMessage.from_payload(
        {"call_id": CALL, "tenant_id": TENANT, "attempt": "third"}).attempt == 0


# ------------------------------------------------------------------------ pricing


def test_model_prices_are_read_from_the_environment_in_paise():
    pricing = pricing_from_env({"SENTINEL_MODEL_PRICING":
                                "claude-sonnet-5=300/1500,gpt-4.1=250/1000"})
    assert pricing["claude-sonnet-5"].input_paise_per_mtok == 300
    assert pricing["gpt-4.1"].output_paise_per_mtok == 1_000


def test_an_unparseable_price_is_refused_rather_than_read_as_free():
    with pytest.raises(ValueError, match="paise per Mtok"):
        pricing_from_env({"SENTINEL_MODEL_PRICING": "claude-sonnet-5=300"})


def test_no_pricing_configured_is_empty_rather_than_invented():
    # cost.py raises on an unpriced model and worker.py turns that into a loud note.
    # A wrong price is worse than a missing one: it produces a budget that looks fine.
    assert pricing_from_env({}) == {}


# ------------------------------------------------------------------ one finalize


def scripted_db(*, segments_far=None, segments_near=None, policy="{}",
                budget=(None, "false", 0), call_present=True) -> FakeDatabase:
    db = FakeDatabase()
    db.on("FROM calls c JOIN tenants t",
          [("agent-a", STARTED, 300_000, "LN-88213", "outbound", "A",
            "Asia/Kolkata", policy)] if call_present else [])
    db.on("SELECT count(*) FROM calls", [(0,)])
    db.on("FROM tenants t WHERE t.id", [budget])
    db.on("FROM rule_sets", [])       # no tenant rule set: the shipped defaults
    db.on("FROM media_segments", segments_far or [])
    db.on("FROM media_segments", segments_near or [])
    return db


def build(db: FakeDatabase, blob: MemoryBlobStore, *, asr_text: str = THREAT,
          analyzer: bool = True, judge: bool = True) -> FinalizeService:
    return FinalizeService(
        db=db,
        blob=blob,
        asr=FakeASR(text=asr_text),
        analyzer=CallAnalyzer(FakeAnalysisProvider()) if analyzer else None,
        judge=ComplianceJudge(FakeJudgeProvider()) if judge else None,
        cost_policy=CostPolicy(pricing=PRICING),
        codec=SegmentCodec(raw_pcm=True),
    )


def audio_for(channel: int, foreign: bool = False):
    key = segment_key(TENANT, "2026-09-01", CALL, channel, 0)
    return [(0, key, foreign)], {key: b"\x00\x01" * 160}


def test_a_finalize_writes_the_whole_compliance_record():
    far_rows, far_objects = audio_for(0)
    near_rows, near_objects = audio_for(1)
    db = scripted_db(segments_far=far_rows, segments_near=near_rows)
    blob = MemoryBlobStore({**far_objects, **near_objects})

    outcome = build(db, blob).finalize(FinalizeMessage(CALL, TENANT, attempt=1))

    assert outcome.status == "complete"
    # Two transcripts, one analysis, and the tier-1 finding the threat earns.
    assert len(db.sql_for("INSERT INTO transcripts")) == 2
    assert len(db.sql_for("INSERT INTO analyses")) == 1
    flags = db.sql_for("INSERT INTO flags")
    assert [f.params[2] for f in flags] == ["false_legal_threat"]
    # The status walk the portal renders: transcribing → analyzing → complete.
    assert [s.params[0] for s in db.sql_for("UPDATE calls")] == ["analyzing", "complete"]
    # Everything, without exception, under this tenant's context.
    assert db.tenants_seen() == {TENANT}


def test_the_finding_carries_the_rule_set_version_it_was_produced_by():
    near_rows, near_objects = audio_for(1)
    db = scripted_db(segments_near=near_rows)
    outcome = build(db, MemoryBlobStore(near_objects)).finalize(
        FinalizeMessage(CALL, TENANT))
    assert outcome.status == "complete"
    # Version 1 is the shipped default rule set, which is what a tenant with no rule
    # set of its own is evaluated against.
    assert db.sql_for("INSERT INTO flags")[0].params[3] == 1


def test_the_same_words_on_the_borrower_channel_are_not_a_finding():
    # Conduct rules apply to the agent. Flagging a borrower's own threat would flood
    # the compliance queue with noise and destroy trust in the tool within a week —
    # and the engine can only tell them apart because the channels were never mixed.
    far_rows, far_objects = audio_for(0)
    db = scripted_db(segments_far=far_rows)
    outcome = build(db, MemoryBlobStore(far_objects)).finalize(
        FinalizeMessage(CALL, TENANT))
    assert outcome.status == "complete" and outcome.findings == []


def test_a_foreign_segment_is_never_fetched_or_transcribed():
    # The end-to-end version of the rule: a tier B call whose loopback was captured
    # while the softphone was idle has stored audio and must produce no transcript
    # from it. Transcribing it would file RBI findings quoted from an agent's music.
    far_rows, far_objects = audio_for(0, foreign=True)
    near_rows, near_objects = audio_for(1)
    db = scripted_db(segments_far=far_rows, segments_near=near_rows)
    blob = MemoryBlobStore({**far_objects, **near_objects})

    outcome = build(db, blob).finalize(FinalizeMessage(CALL, TENANT))
    assert outcome.status == "complete"
    assert len(db.sql_for("INSERT INTO transcripts")) == 1
    assert "no audio on the borrower channel" in outcome.notes


def test_a_call_with_no_audio_at_all_raises_so_the_message_is_not_acked():
    # A finalize can arrive before the last segments have landed; a redelivery a
    # minute later usually succeeds, and the DLQ catches the ones that never do.
    db = scripted_db()
    with pytest.raises(RuntimeError, match="no transcript"):
        build(db, MemoryBlobStore()).finalize(FinalizeMessage(CALL, TENANT))
    assert db.sql_for("UPDATE calls")[0].params[0] == "failed"


def test_a_call_this_tenant_does_not_have_is_unprocessable_rather_than_retried():
    db = scripted_db(call_present=False)
    with pytest.raises(Unprocessable):
        build(db, MemoryBlobStore()).finalize(FinalizeMessage(CALL, TENANT))


def test_a_call_id_that_is_not_a_ulid_is_unprocessable():
    with pytest.raises(Unprocessable):
        build(scripted_db(), MemoryBlobStore()).finalize(
            FinalizeMessage("not-a-ulid", TENANT))


def test_the_kill_switch_stops_the_model_calls_and_not_the_compliance_rules():
    # What the customer is paying for keeps working. cost.py decides; this asserts
    # the wiring reaches it from tenant policy.
    near_rows, near_objects = audio_for(1)
    db = scripted_db(segments_near=near_rows, budget=(None, "true", 0))
    outcome = build(db, MemoryBlobStore(near_objects)).finalize(
        FinalizeMessage(CALL, TENANT))

    assert outcome.status == "complete"
    assert not db.sql_for("INSERT INTO analyses")
    assert db.sql_for("INSERT INTO flags"), "tier-1 findings must still be written"


def test_an_exhausted_budget_still_produces_a_compliance_record():
    near_rows, near_objects = audio_for(1)
    db = scripted_db(segments_near=near_rows, budget=(1_000, "false", 5_000))
    outcome = build(db, MemoryBlobStore(near_objects)).finalize(
        FinalizeMessage(CALL, TENANT))
    assert outcome.status == "complete"
    assert not db.sql_for("INSERT INTO analyses")
    assert db.sql_for("INSERT INTO flags")


def test_the_floors_language_reaches_the_transcriber():
    # Tenant policy selects the transcriber on a routed floor. A wrong value here is
    # Tamil audio going to a model that has no Tamil.
    far_rows, far_objects = audio_for(0)
    db = scripted_db(segments_far=far_rows, policy='{"language": "ta-IN"}')
    service = build(db, MemoryBlobStore(far_objects))
    service.finalize(FinalizeMessage(CALL, TENANT))
    assert db.sql_for("INSERT INTO transcripts")[0].params[5] == "ta-IN"


def test_the_async_handler_runs_the_finalize_off_the_event_loop():
    # A blocking finalize on the event loop stalls every other message's ack until
    # AckWait expires, at which point JetStream redelivers calls that were being
    # processed perfectly well.
    far_rows, far_objects = audio_for(0)
    db = scripted_db(segments_far=far_rows)
    service = build(db, MemoryBlobStore(far_objects))

    asyncio.run(service.handle({"call_id": CALL, "tenant_id": TENANT, "attempt": 1,
                                "finalized_at": "2026-09-01T10:19:44Z"}))
    assert db.sql_for("INSERT INTO transcripts")


# ------------------------------------------------------------------ coverage job


CSV = """agent_id,started_at,duration_s,account_ref,dialer_call_id
AG-1,2026-09-01T11:00:00,120,LN-1,
AG-1,2026-09-01T11:30:00,60,LN-2,
"""


def coverage_db(policy=None) -> FakeDatabase:
    db = FakeDatabase()
    db.on("sentinel_pipeline_tenants", [(TENANT, 30, 365, "Asia/Kolkata")])
    db.on("FROM calls WHERE tenant_id",
          [("uid-1", STARTED, 118_000, "LN-1", None)])
    db.on("SELECT policy", [(policy or {"cdr_agent_map": {"AG-1": "uid-1"}},)])
    return db


def test_the_coverage_job_reconciles_a_day_and_writes_one_row_per_agent(tmp_path):
    export = tmp_path / TENANT / "2026-09-01.csv"
    export.parent.mkdir(parents=True)
    export.write_text(CSV)
    db = coverage_db()

    assert run_coverage({"SENTINEL_CDR_ADAPTER": "csv",
                         "SENTINEL_CDR_DIR": str(tmp_path)},
                        day=date(2026, 9, 1), db=db) == 0

    written = db.sql_for("INSERT INTO coverage_daily")
    assert len(written) == 1
    tenant, uid, day, dialer_calls, captured_calls = written[0].params[:5]
    assert (tenant, uid, day) == (TENANT, "uid-1", date(2026, 9, 1))
    assert (dialer_calls, captured_calls) == (2, 1)


def test_a_tenant_with_no_export_is_skipped_loudly_and_nothing_is_written(tmp_path):
    # Writing zeros would claim 100% coverage for a day nobody measured, and 100% is
    # the number this feature exists to be able to prove.
    db = coverage_db()
    assert run_coverage({"SENTINEL_CDR_ADAPTER": "csv",
                         "SENTINEL_CDR_DIR": str(tmp_path)},
                        day=date(2026, 9, 1), db=db) == 1
    assert not db.sql_for("INSERT INTO coverage_daily")


def test_no_cdr_adapter_configured_is_not_an_error(tmp_path):
    # OPEN-7: until the customer supplies an export there is nothing to reconcile.
    assert run_coverage({}, day=date(2026, 9, 1), db=coverage_db()) == 0


# -------------------------------------------------------------------- logging


def test_logs_are_structured_and_carry_only_the_fields_passed_to_them(capsys):
    configure_logging("INFO")
    try:
        logging.getLogger("test").info("finalized", extra={"tenant_id": TENANT,
                                                           "status": "complete"})
        line = capsys.readouterr().err.strip().splitlines()[-1]
        payload = json.loads(line)
        assert payload["msg"] == "finalized"
        assert payload["tenant_id"] == TENANT and payload["status"] == "complete"
        # Nothing the standard library put on the record comes along for the ride,
        # so an accidental exc_info cannot smuggle a provider's echoed input into
        # the log stream.
        assert "args" not in payload and "exc_info" not in payload
    finally:
        logging.getLogger().handlers = []
