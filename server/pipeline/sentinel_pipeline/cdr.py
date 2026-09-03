"""Dialer CDR export adapters — the pluggable half of coverage reconciliation.

**OPEN-7 is genuinely undecided and nothing here decides it.** The format and the
delivery mechanism differ per bank, the customer has not supplied a sample export,
and inventing a canonical shape would convert an open question into a hidden one —
the exact failure ``docs/open-decisions.md`` opens by warning about. So this module
holds *adapters*, one of which (CSV) is a reference implementation good enough for
development and for the first customer whose dialer can export a delimited file.
:mod:`sentinel_pipeline.coverage` keeps the arithmetic and knows nothing about
formats, which is what its docstring already asks for.

## Plugging in a real bank's export

1. Write a class with one method, ``calls_for(tenant_id, day) -> Iterable[CdrCall]``,
   satisfying :class:`sentinel_pipeline.coverage.CdrSource`. Put the parsing in it and
   nothing else — no matching, no percentages.
2. Register it in :data:`CDR_ADAPTERS` under a name.
3. Select it with ``SENTINEL_CDR_ADAPTER=<name>``.

Nothing in ``coverage.py`` changes, and the matching rules (``dialer_call_id`` when
present, otherwise ``(agent, started_at)`` within thirty seconds) stay in one place.

## The one thing an adapter must never do

An adapter that cannot find or read its export must **raise**, never return an empty
list. ``Coverage.pct`` is 100% when ``dialer_calls`` is zero — correctly, because a
day on which the dialer made no calls was fully covered — so a missing export that
returns "no dialer calls" reports every agent at 100% coverage on a day nobody
measured. That number goes in front of a bank as proof that 100% of calls are
monitored. A loud failure with yesterday's row left alone is the honest outcome.
"""

from __future__ import annotations

import csv
import logging
import os
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta
from pathlib import Path
from typing import Callable, Iterable, Mapping

from .coverage import CdrCall

log = logging.getLogger(__name__)


class CdrUnavailable(RuntimeError):
    """The export for this tenant and day could not be read.

    Distinct from "the export says there were no calls", which is a valid answer and
    arrives as an empty iterable from a source that *did* find its input.
    """


# ------------------------------------------------------------------- column mapping


@dataclass(frozen=True)
class ColumnMap:
    """Which columns of a delimited export mean what.

    Defaults are the names this repository uses in its own fixtures, not a claim
    about any real dialer. Every one of them is expected to be overridden per
    customer once a sample export exists (OPEN-7), which is why they are data rather
    than literals in the reader.

    ``agent_id``          the dialer's own agent identifier. Mapped to a Firebase uid
                          by ``coverage.reconcile``'s ``agent_for``, because the two
                          identifier spaces are unrelated.
    ``started_at``        when the dialer *dialed*. Not when speech began — that is
                          why ``coverage.MATCH_WINDOW`` exists.
    ``duration``          call duration; see ``duration_unit``.
    ``account_ref``       optional loan account reference.
    ``dialer_call_id``    optional; when the dialer supplies one, matching is exact.
    """

    agent_id: str = "agent_id"
    started_at: str = "started_at"
    duration: str = "duration_s"
    account_ref: str | None = "account_ref"
    dialer_call_id: str | None = "dialer_call_id"

    #: ``s``, ``ms`` or ``hms`` (``HH:MM:SS``). Dialers disagree, and a millisecond
    #: column read as seconds inflates dialer minutes by a thousand — which reads as
    #: catastrophically low coverage rather than as a unit error.
    duration_unit: str = "s"
    #: ``strptime`` format, or ``None`` for ISO 8601.
    timestamp_format: str | None = None
    #: Applied when the export's timestamps carry no offset, which most do not. The
    #: floor's own timezone is the only sane reading of a naive local timestamp, and
    #: assuming UTC would shift every call by five and a half hours and match nothing.
    timezone: str = "Asia/Kolkata"

    @staticmethod
    def from_spec(spec: str, **overrides: object) -> "ColumnMap":
        """Parse ``field=column,field=column`` into a map.

        Unknown field names raise rather than being ignored: a typo in a column
        mapping is a silent all-day coverage gap otherwise.
        """
        fields = {f.name for f in ColumnMap.__dataclass_fields__.values()}  # type: ignore[attr-defined]
        kwargs: dict[str, object] = dict(overrides)
        for entry in (part.strip() for part in spec.split(",")):
            if not entry:
                continue
            name, sep, column = entry.partition("=")
            name, column = name.strip(), column.strip()
            if not sep or name not in fields:
                raise ValueError(
                    f"SENTINEL_CDR_COLUMNS entry {entry!r} is not field=column with a "
                    f"known field; known fields are {', '.join(sorted(fields))}"
                )
            kwargs[name] = column
        return ColumnMap(**kwargs)  # type: ignore[arg-type]


def _parse_timestamp(raw: str, mapping: ColumnMap) -> datetime:
    from zoneinfo import ZoneInfo  # noqa: PLC0415 - stdlib, only this path needs it

    text = raw.strip()
    if mapping.timestamp_format:
        parsed = datetime.strptime(text, mapping.timestamp_format)
    else:
        parsed = datetime.fromisoformat(text.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=ZoneInfo(mapping.timezone))
    return parsed


def _parse_duration_ms(raw: str, mapping: ColumnMap) -> int:
    text = raw.strip()
    if not text:
        return 0
    if mapping.duration_unit == "ms":
        return int(float(text))
    if mapping.duration_unit == "hms":
        parts = [int(p) for p in text.split(":")]
        while len(parts) < 3:
            parts.insert(0, 0)
        hours, minutes, seconds = parts[-3], parts[-2], parts[-1]
        return int(timedelta(hours=hours, minutes=minutes,
                             seconds=seconds).total_seconds() * 1000)
    return int(float(text) * 1000)


# ----------------------------------------------------------------------- CSV source


@dataclass
class CsvCdrSource:
    """Reads one delimited file per tenant per day.

    The reference implementation, and a plausible first integration: a nightly SFTP
    drop of a delimited export is what most of these dialers can actually produce.

    ``path_template`` is formatted with ``tenant_id`` and ``day`` (ISO), so a
    customer's own layout is configuration rather than code.
    """

    root: str
    columns: ColumnMap = field(default_factory=ColumnMap)
    path_template: str = "{tenant_id}/{day}.csv"
    delimiter: str = ","

    def path_for(self, tenant_id: str, day: date) -> Path:
        return Path(self.root) / self.path_template.format(
            tenant_id=tenant_id, day=day.isoformat()
        )

    def calls_for(self, tenant_id: str, day: date) -> Iterable[CdrCall]:
        path = self.path_for(tenant_id, day)
        if not path.is_file():
            # Loud. See the module docstring: an unreadable export must never look
            # like a day with no dialer calls.
            raise CdrUnavailable(f"no CDR export at {path}")

        calls: list[CdrCall] = []
        skipped = 0
        with path.open(newline="", encoding="utf-8-sig") as fh:
            reader = csv.DictReader(fh, delimiter=self.delimiter)
            missing = [c for c in self._required_columns()
                       if c not in (reader.fieldnames or [])]
            if missing:
                raise CdrUnavailable(
                    f"CDR export {path.name} is missing the columns "
                    f"{', '.join(missing)}; check SENTINEL_CDR_COLUMNS against a "
                    f"sample export (OPEN-7)"
                )
            for row in reader:
                try:
                    calls.append(self._to_call(row))
                except (ValueError, KeyError, TypeError):
                    # One malformed row is skipped and counted; the log line carries
                    # no field values, because an account reference is borrower data.
                    skipped += 1
        if skipped:
            log.warning("skipped unparseable CDR rows",
                        extra={"rows": skipped, "parsed": len(calls)})
        if skipped and not calls:
            # Every row failed. That is a wrong column map or a changed export, not a
            # quiet day, and it must not reconcile as one.
            raise CdrUnavailable(
                f"every row of {path.name} failed to parse ({skipped} rows); the "
                f"column mapping or the export format has changed"
            )
        return calls

    def _required_columns(self) -> list[str]:
        return [self.columns.agent_id, self.columns.started_at, self.columns.duration]

    def _to_call(self, row: Mapping[str, str]) -> CdrCall:
        cols = self.columns
        return CdrCall(
            agent_id=(row[cols.agent_id] or "").strip(),
            started_at=_parse_timestamp(row[cols.started_at], cols),
            duration_ms=_parse_duration_ms(row[cols.duration], cols),
            account_ref=_optional(row, cols.account_ref),
            dialer_call_id=_optional(row, cols.dialer_call_id),
        )


def _optional(row: Mapping[str, str], column: str | None) -> str | None:
    if not column:
        return None
    value = (row.get(column) or "").strip()
    return value or None


# ------------------------------------------------------------------------ selection


def _csv_from_env(env: Mapping[str, str]) -> CsvCdrSource:
    root = env.get("SENTINEL_CDR_DIR")
    if not root:
        raise CdrUnavailable(
            "SENTINEL_CDR_ADAPTER=csv needs SENTINEL_CDR_DIR pointing at the "
            "directory the dialer export is delivered to"
        )
    columns = ColumnMap.from_spec(
        env.get("SENTINEL_CDR_COLUMNS", ""),
        duration_unit=env.get("SENTINEL_CDR_DURATION_UNIT", "s"),
        timestamp_format=env.get("SENTINEL_CDR_TIMESTAMP_FORMAT") or None,
        timezone=env.get("SENTINEL_CDR_TIMEZONE", "Asia/Kolkata"),
    )
    return CsvCdrSource(
        root=root,
        columns=columns,
        path_template=env.get("SENTINEL_CDR_PATH_TEMPLATE", "{tenant_id}/{day}.csv"),
        delimiter=env.get("SENTINEL_CDR_DELIMITER", ","),
    )


#: Name → factory. **This is the extension point.** A real bank's export becomes a
#: new entry here plus a class implementing ``calls_for``; nothing else in the
#: pipeline changes, and in particular ``coverage.py`` does not.
CDR_ADAPTERS: dict[str, Callable[[Mapping[str, str]], object]] = {
    "csv": _csv_from_env,
}


def cdr_source_from_env(env: Mapping[str, str] | None = None) -> object | None:
    """Build the configured CDR source, or ``None`` when coverage is not wired up.

    ``SENTINEL_CDR_ADAPTER``  adapter name, or ``none``/unset to skip reconciliation.
    ``SENTINEL_CDR_DIR``      (csv) directory the export is delivered to.
    ``SENTINEL_CDR_COLUMNS``  (csv) ``field=column`` overrides for :class:`ColumnMap`.

    Returning ``None`` rather than raising is right for the *unset* case only: until
    the customer supplies an export there is nothing to reconcile against, and the
    coverage job says so and exits. A *named but broken* adapter raises.
    """
    env = dict(os.environ if env is None else env)
    name = (env.get("SENTINEL_CDR_ADAPTER") or "").strip().lower()
    if not name or name == "none":
        return None
    factory = CDR_ADAPTERS.get(name)
    if factory is None:
        raise CdrUnavailable(
            f"unknown CDR adapter {name!r}; known adapters are "
            f"{', '.join(sorted(CDR_ADAPTERS))}. A new bank format is a new adapter "
            f"registered in sentinel_pipeline.cdr.CDR_ADAPTERS (OPEN-7)."
        )
    return factory(env)
