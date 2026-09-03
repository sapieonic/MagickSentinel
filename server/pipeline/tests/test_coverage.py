"""Coverage reconciliation arithmetic.

This is the number that turns tamper detection into a management conversation, and
the number that backs the "100% of calls monitored" claim the product is bought for.
It had no tests at all, which for a figure shown to a bank client is the wrong amount.

The cases below are the ones that produce a *wrong* number rather than an error: an
unmapped agent id (everything reads as uncaptured), a match window that is too tight
(a long ring reads as a missed call), a captured call counted twice, and an empty CDR
reading as perfect coverage.
"""

from datetime import date, datetime, timedelta, timezone

from sentinel_pipeline.coverage import (
    MATCH_WINDOW,
    CapturedCall,
    CdrCall,
    Coverage,
    reconcile,
)

DAY = date(2026, 9, 1)
T0 = datetime(2026, 9, 1, 4, 30, tzinfo=timezone.utc)  # 10:00 IST
TENANT = "t1"


def cdr(agent="AG-1", at=T0, seconds=120, account=None, dialer_id=None):
    return CdrCall(agent_id=agent, started_at=at, duration_ms=seconds * 1000,
                   account_ref=account, dialer_call_id=dialer_id)


def captured(uid="uid-1", at=T0, seconds=118, dialer_id=None):
    return CapturedCall(user_uid=uid, started_at=at, duration_ms=seconds * 1000,
                        dialer_call_id=dialer_id)


MAP = {"AG-1": "uid-1", "AG-2": "uid-2"}


def test_a_dialer_call_id_matches_exactly_and_ignores_the_clock():
    # When the dialer supplies an id, the timestamps do not have to agree at all —
    # which matters because the two systems stamp different moments.
    rows = reconcile(TENANT, DAY,
                     [cdr(dialer_id="D-1", at=T0)],
                     [captured(dialer_id="D-1", at=T0 + timedelta(minutes=9))],
                     agent_for=MAP)
    assert (rows[0].dialer_calls, rows[0].captured_calls) == (1, 1)
    assert rows[0].pct == 100.0


def test_without_a_dialer_id_a_call_matches_inside_the_window():
    # The dialer stamps the moment it dials; we stamp when speech is confirmed, which
    # is after ringing. A window that is too tight reads a long ring as a missed call.
    rows = reconcile(TENANT, DAY, [cdr()], [captured(at=T0 + MATCH_WINDOW)],
                     agent_for=MAP)
    assert rows[0].captured_calls == 1


def test_a_call_outside_the_window_is_a_different_call():
    rows = reconcile(TENANT, DAY, [cdr()],
                     [captured(at=T0 + MATCH_WINDOW + timedelta(seconds=1))],
                     agent_for=MAP)
    assert rows[0].captured_calls == 0
    assert rows[0].pct == 0.0


def test_one_captured_call_cannot_satisfy_two_dialer_rows():
    # Otherwise a single capture would cover a whole shift and hide the gap.
    rows = reconcile(TENANT, DAY,
                     [cdr(at=T0), cdr(at=T0 + timedelta(minutes=10))],
                     [captured(at=T0)],
                     agent_for=MAP)
    assert (rows[0].dialer_calls, rows[0].captured_calls) == (2, 1)
    assert rows[0].pct == 50.0


def test_an_unmapped_dialer_agent_id_reads_as_a_total_capture_failure():
    # The two identifier spaces are unrelated: the bank's dialer knows nothing about
    # our IdP. Without the mapping, every call on the floor reads as uncaptured —
    # the most alarming possible way to get a configuration error wrong, which is why
    # the mapping is read per tenant and this test exists to name the symptom.
    rows = reconcile(TENANT, DAY, [cdr()], [captured()], agent_for=None)
    assert len(rows) == 1
    assert rows[0].user_uid == "AG-1", "unmapped ids fall through as themselves"
    assert rows[0].captured_calls == 0


def test_minutes_come_from_each_side_separately():
    rows = reconcile(TENANT, DAY,
                     [cdr(seconds=180), cdr(at=T0 + timedelta(hours=1), seconds=60)],
                     [captured(seconds=175)],
                     agent_for=MAP)
    assert rows[0].dialer_minutes == 4      # 3 + 1 whole minutes
    assert rows[0].captured_minutes == 2    # 175 s of the matched call


def test_one_row_per_agent_sorted_by_uid():
    rows = reconcile(TENANT, DAY,
                     [cdr(agent="AG-2"), cdr(agent="AG-1"), cdr(agent="AG-1")],
                     [captured(uid="uid-2")],
                     agent_for=MAP)
    assert [r.user_uid for r in rows] == ["uid-1", "uid-2"]
    assert [r.dialer_calls for r in rows] == [2, 1]


def test_no_dialer_calls_at_all_produces_no_rows():
    # And in particular does not produce a row claiming 100%. A day the dialer has no
    # record of is a day there is nothing to reconcile — which is why a *missing*
    # export must raise rather than arrive here as an empty list (see cdr.py).
    assert reconcile(TENANT, DAY, [], [captured()], agent_for=MAP) == []


def test_pct_of_a_day_the_dialer_made_no_calls_on_is_a_hundred():
    # The arithmetic's own convention, and the reason an unreadable export is never
    # allowed to look like an empty one.
    empty = Coverage(TENANT, "uid-1", DAY, 0, 0, 0, 0)
    assert empty.pct == 100.0


def test_the_gap_reason_distinguishes_no_captures_from_a_few_missing():
    # Deliberately coarse: "check the agent signed in" and "check device events" are
    # different follow-ups, and the point is to start the right conversation.
    none_captured = reconcile(TENANT, DAY, [cdr()], [], agent_for=MAP)[0]
    assert "check that the agent signed in" in none_captured.gap_reason

    ten = [cdr(at=T0 + timedelta(minutes=5 * i)) for i in range(10)]
    nine = [captured(at=T0 + timedelta(minutes=5 * i)) for i in range(9)]
    partial = reconcile(TENANT, DAY, ten, nine, agent_for=MAP)[0]
    assert partial.pct == 90.0
    assert partial.gap_reason is None, "90% is the threshold, not a gap"

    eight = nine[:8]
    short = reconcile(TENANT, DAY, ten, eight, agent_for=MAP)[0]
    assert short.pct == 80.0
    assert "device events" in short.gap_reason


def test_captured_calls_belonging_to_another_agent_do_not_count():
    rows = reconcile(TENANT, DAY, [cdr(agent="AG-1")], [captured(uid="uid-2")],
                     agent_for=MAP)
    assert rows[0].captured_calls == 0
