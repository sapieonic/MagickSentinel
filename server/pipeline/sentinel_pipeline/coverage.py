"""Nightly reconciliation of captured calls against the dialer's CDR export.

This is what turns tamper detection from a technical arms race into a management
conversation. An agent who disables the software does not get away with it quietly;
their coverage percentage drops, their supervisor sees it, and the conversation
happens where it belongs.

The CDR format and delivery mechanism are **OPEN-7** and differ per bank, so the
parsing lives behind :class:`CdrSource` and this module only does the arithmetic.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date, datetime, timedelta
from typing import Iterable, Protocol


@dataclass(frozen=True)
class CdrCall:
    """One row from the dialer's export."""

    agent_id: str
    started_at: datetime
    duration_ms: int
    account_ref: str | None = None
    dialer_call_id: str | None = None


@dataclass(frozen=True)
class CapturedCall:
    user_uid: str
    started_at: datetime
    duration_ms: int
    account_ref: str | None = None
    dialer_call_id: str | None = None


class CdrSource(Protocol):
    def calls_for(self, tenant_id: str, day: date) -> Iterable[CdrCall]: ...


@dataclass
class Coverage:
    tenant_id: str
    user_uid: str
    day: date
    dialer_calls: int
    captured_calls: int
    dialer_minutes: int
    captured_minutes: int
    gap_reason: str | None = None

    @property
    def pct(self) -> float:
        if self.dialer_calls == 0:
            return 100.0
        return 100.0 * self.captured_calls / self.dialer_calls


# How far apart a CDR row and a captured call may start and still be the same call.
# The dialer stamps the moment it dials; we stamp when speech is confirmed, which is
# after ringing. Thirty seconds covers a long ring without matching the next call.
MATCH_WINDOW = timedelta(seconds=30)


def reconcile(tenant_id: str, day: date, cdr: list[CdrCall], captured: list[CapturedCall],
              agent_for: dict[str, str] | None = None) -> list[Coverage]:
    """Match the two sides and produce one coverage row per agent.

    ``agent_for`` maps the **dialer's** agent id to the Firebase uid our own rows
    carry. The two identifier spaces are different — the bank's dialer knows nothing
    about our IdP — so without this mapping every call reads as uncaptured, which is
    the most alarming possible way to get a configuration error wrong.

    Matching is by ``dialer_call_id`` when the dialer supplies one, and otherwise by
    ``(agent, started_at)`` within :data:`MATCH_WINDOW`. Falling back rather than
    requiring the id matters because most dialers on these floors do not expose one,
    which is exactly why ``account_ref`` is allowed to be null on the client.
    """
    agent_for = agent_for or {}

    def uid_of(agent_id: str) -> str:
        return agent_for.get(agent_id, agent_id)

    remaining = list(captured)
    by_dialer_id = {c.dialer_call_id: c for c in remaining if c.dialer_call_id}

    matched: set[int] = set()
    per_agent: dict[str, Coverage] = {}

    def row(uid: str) -> Coverage:
        if uid not in per_agent:
            per_agent[uid] = Coverage(tenant_id, uid, day, 0, 0, 0, 0)
        return per_agent[uid]

    for cdr_call in cdr:
        uid = uid_of(cdr_call.agent_id)
        r = row(uid)
        r.dialer_calls += 1
        r.dialer_minutes += cdr_call.duration_ms // 60_000

        match = None
        if cdr_call.dialer_call_id and cdr_call.dialer_call_id in by_dialer_id:
            match = by_dialer_id[cdr_call.dialer_call_id]
        else:
            for i, cap in enumerate(remaining):
                if i in matched or cap.user_uid != uid:
                    continue
                if abs(cap.started_at - cdr_call.started_at) <= MATCH_WINDOW:
                    match, _ = cap, matched.add(i)
                    break
        if match is not None:
            r.captured_calls += 1
            r.captured_minutes += match.duration_ms // 60_000

    for r in per_agent.values():
        r.gap_reason = _gap_reason(r)
    return sorted(per_agent.values(), key=lambda c: c.user_uid)


def _gap_reason(c: Coverage) -> str | None:
    """A first guess at why coverage is short, for the supervisor's view.

    Deliberately coarse. The point is to start the right conversation, not to accuse
    anyone: "no captures at all today" and "a few calls missing" call for very
    different follow-ups, and the device's own heartbeat events settle which it is.
    """
    if c.dialer_calls == 0:
        return None
    if c.captured_calls == 0:
        return "no calls captured; check that the agent signed in and the headset was detected"
    if c.pct < 90:
        return "some calls not captured; check device events for headset loss or agent restarts"
    return None
