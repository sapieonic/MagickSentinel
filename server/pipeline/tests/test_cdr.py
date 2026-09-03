"""The CDR adapter layer (OPEN-7).

No format is asserted to be *the* format here — that decision is not ours to make and
the tests are careful not to imply it has been made. What is tested is the behaviour
every adapter has to have, whatever the bank's export turns out to look like:

* a mapping that is configuration rather than code, and that fails loudly on a typo,
* units and timezones stated rather than guessed, and
* an unreadable export raising instead of quietly reconciling as a day with no
  dialer calls, which would report every agent at 100% coverage.
"""

from datetime import date, datetime, timezone

import pytest

from sentinel_pipeline.cdr import (
    CDR_ADAPTERS,
    CdrUnavailable,
    ColumnMap,
    CsvCdrSource,
    cdr_source_from_env,
)

DAY = date(2026, 9, 1)
TENANT = "t1"

DEFAULT_CSV = """agent_id,started_at,duration_s,account_ref,dialer_call_id
AG-1,2026-09-01T10:00:00,120,LN-1,D-1
AG-1,2026-09-01T10:15:00,45,LN-2,
AG-2,2026-09-01T11:00:00,300,,D-3
"""


def write(tmp_path, body: str, name: str = "t1/2026-09-01.csv") -> CsvCdrSource:
    path = tmp_path / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    return CsvCdrSource(root=str(tmp_path))


def test_a_delimited_export_parses_into_cdr_calls(tmp_path):
    source = write(tmp_path, DEFAULT_CSV)
    calls = list(source.calls_for(TENANT, DAY))

    assert [c.agent_id for c in calls] == ["AG-1", "AG-1", "AG-2"]
    assert calls[0].duration_ms == 120_000
    assert calls[0].dialer_call_id == "D-1"
    # An empty cell is absent, not the empty string: coverage.reconcile branches on
    # whether the dialer supplied an id at all.
    assert calls[1].dialer_call_id is None
    assert calls[2].account_ref is None


def test_naive_timestamps_are_read_in_the_floors_timezone(tmp_path):
    source = write(tmp_path, DEFAULT_CSV)
    first = list(source.calls_for(TENANT, DAY))[0]
    # Assuming UTC would shift every call by five and a half hours and match nothing.
    assert first.started_at.utcoffset().total_seconds() == 5.5 * 3600
    assert first.started_at.astimezone(timezone.utc) == \
        datetime(2026, 9, 1, 4, 30, tzinfo=timezone.utc)


def test_an_offset_in_the_export_is_respected(tmp_path):
    body = ("agent_id,started_at,duration_s\n"
            "AG-1,2026-09-01T10:00:00Z,60\n")
    source = write(tmp_path, body)
    assert list(source.calls_for(TENANT, DAY))[0].started_at.utcoffset() == \
        timezone.utc.utcoffset(None)


@pytest.mark.parametrize("unit,cell,expected_ms", [
    ("s", "90", 90_000),
    ("ms", "90000", 90_000),
    ("hms", "00:01:30", 90_000),
    ("hms", "1:30", 90_000),
])
def test_duration_units_are_stated_rather_than_guessed(tmp_path, unit, cell, expected_ms):
    # A millisecond column read as seconds inflates dialer minutes a thousandfold,
    # which reads as catastrophically low coverage rather than as a unit error.
    body = f"agent_id,started_at,duration_s\nAG-1,2026-09-01T10:00:00,{cell}\n"
    source = write(tmp_path, body)
    source.columns = ColumnMap(duration_unit=unit)
    assert list(source.calls_for(TENANT, DAY))[0].duration_ms == expected_ms


def test_a_customers_own_column_names_are_configuration(tmp_path):
    body = ("Agent Code|Call Start|Talk Time\n"
            "AG-1|01/09/2026 10:00|120\n")
    path = tmp_path / "t1" / "2026-09-01.csv"
    path.parent.mkdir(parents=True)
    path.write_text(body, encoding="utf-8")
    source = CsvCdrSource(
        root=str(tmp_path),
        delimiter="|",
        columns=ColumnMap(agent_id="Agent Code", started_at="Call Start",
                          duration="Talk Time", account_ref=None,
                          dialer_call_id=None,
                          timestamp_format="%d/%m/%Y %H:%M"),
    )
    calls = list(source.calls_for(TENANT, DAY))
    assert calls[0].agent_id == "AG-1" and calls[0].duration_ms == 120_000


def test_a_column_spec_typo_is_refused_rather_than_ignored():
    # A silently ignored mapping is an all-day coverage gap that looks like tampering.
    with pytest.raises(ValueError, match="known field"):
        ColumnMap.from_spec("agent=AgentID")
    assert ColumnMap.from_spec("agent_id=AgentID").agent_id == "AgentID"


def test_a_missing_export_raises_rather_than_reading_as_a_quiet_day(tmp_path):
    # The single most important behaviour in this module. Coverage.pct is 100% when
    # there are no dialer calls, so an empty return would put "100% monitored" in
    # front of a bank for a day nobody measured.
    with pytest.raises(CdrUnavailable):
        list(CsvCdrSource(root=str(tmp_path)).calls_for(TENANT, DAY))


def test_a_missing_required_column_names_what_is_missing(tmp_path):
    source = write(tmp_path, "agent_id,started_at\nAG-1,2026-09-01T10:00:00\n")
    with pytest.raises(CdrUnavailable, match="duration_s"):
        list(source.calls_for(TENANT, DAY))


def test_one_unparseable_row_is_skipped_and_the_rest_still_reconcile(tmp_path):
    body = ("agent_id,started_at,duration_s\n"
            "AG-1,2026-09-01T10:00:00,60\n"
            "AG-2,not-a-timestamp,60\n"
            "AG-3,2026-09-01T11:00:00,60\n")
    source = write(tmp_path, body)
    calls = list(source.calls_for(TENANT, DAY))
    assert [c.agent_id for c in calls] == ["AG-1", "AG-3"]


def test_an_export_where_every_row_fails_is_a_format_change_not_a_quiet_day(tmp_path):
    body = ("agent_id,started_at,duration_s\n"
            "AG-1,not-a-timestamp,60\n"
            "AG-2,also-not,60\n")
    source = write(tmp_path, body)
    with pytest.raises(CdrUnavailable, match="failed to parse"):
        list(source.calls_for(TENANT, DAY))


def test_the_path_template_follows_the_customers_delivery_layout(tmp_path):
    path = tmp_path / "acme" / "cdr-20260901.txt"
    path.parent.mkdir(parents=True)
    path.write_text("agent_id,started_at,duration_s\nAG-1,2026-09-01T10:00:00,60\n")
    source = CsvCdrSource(root=str(tmp_path),
                          path_template="acme/cdr-{day}.txt")
    # {day} is ISO in the template; a customer whose files are named differently gets
    # a different template rather than a patched reader.
    assert source.path_for(TENANT, DAY).name == "cdr-2026-09-01.txt"
    with pytest.raises(CdrUnavailable):
        list(source.calls_for(TENANT, DAY))  # the ISO name does not exist


# ------------------------------------------------------------------- selection


def test_no_adapter_configured_means_coverage_is_simply_not_wired_up():
    # OPEN-7: until the customer supplies an export there is nothing to reconcile
    # against, and the job should say so rather than raise on every run.
    assert cdr_source_from_env({}) is None
    assert cdr_source_from_env({"SENTINEL_CDR_ADAPTER": "none"}) is None


def test_a_named_adapter_that_cannot_be_built_raises(tmp_path):
    with pytest.raises(CdrUnavailable, match="SENTINEL_CDR_DIR"):
        cdr_source_from_env({"SENTINEL_CDR_ADAPTER": "csv"})


def test_an_unknown_adapter_points_at_the_extension_point():
    with pytest.raises(CdrUnavailable, match="CDR_ADAPTERS"):
        cdr_source_from_env({"SENTINEL_CDR_ADAPTER": "acme-dialer"})


def test_the_csv_adapter_is_built_from_the_environment(tmp_path):
    source = cdr_source_from_env({
        "SENTINEL_CDR_ADAPTER": "csv",
        "SENTINEL_CDR_DIR": str(tmp_path),
        "SENTINEL_CDR_COLUMNS": "agent_id=AgentID,duration=TalkMs",
        "SENTINEL_CDR_DURATION_UNIT": "ms",
        "SENTINEL_CDR_TIMEZONE": "Asia/Kolkata",
        "SENTINEL_CDR_DELIMITER": ";",
    })
    assert isinstance(source, CsvCdrSource)
    assert source.columns.agent_id == "AgentID"
    assert source.columns.duration == "TalkMs"
    assert source.columns.duration_unit == "ms"
    assert source.delimiter == ";"


def test_the_registry_is_where_a_real_bank_format_plugs_in():
    # Named explicitly so the extension point is visible from the tests too: a new
    # export is a new entry here, not an edit to coverage.py.
    assert "csv" in CDR_ADAPTERS
