"""An in-memory stand-in for :class:`sentinel_pipeline.db.Database`.

The point is not to simulate Postgres — it is to make the *discipline* around every
statement testable without one. Two properties matter more than any individual
query's result and neither is visible in a live-database test that happens to pass:

* every statement runs inside a transaction that set the RLS context first, and
* the context it set names the tenant the caller was working on.

A statement recorded here carries the context that was open when it ran, so a test can
assert both. ``db/test/rls_test.sh`` covers what the database does when the context is
missing; this covers whether the pipeline ever forgets to set it.
"""

from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import Any, Iterator


@dataclass(frozen=True)
class Statement:
    sql: str
    params: tuple | None
    #: ``(tenant_id, user_uid, role)``, or ``None`` for a transaction opened through
    #: ``without_tenant`` (which only SECURITY DEFINER calls may use).
    context: tuple[str, str, str] | None

    @property
    def squashed(self) -> str:
        """The SQL with runs of whitespace collapsed, for readable assertions."""
        return " ".join(self.sql.split())


class FakeCursor:
    def __init__(self, rows: list[tuple], rowcount: int | None = None) -> None:
        self.rows = rows
        self.rowcount = len(rows) if rowcount is None else rowcount

    def fetchone(self) -> tuple | None:
        return self.rows[0] if self.rows else None

    def fetchall(self) -> list[tuple]:
        return list(self.rows)


class FakeConnection:
    def __init__(self, db: "FakeDatabase", context: tuple[str, str, str] | None) -> None:
        self._db = db
        self._context = context

    def execute(self, sql: str, params: tuple | None = None) -> FakeCursor:
        self._db.statements.append(Statement(sql=sql, params=params,
                                             context=self._context))
        return self._db.respond(sql, params)


@dataclass
class _Handler:
    match: str
    results: list[FakeCursor]


@dataclass
class FakeDatabase:
    statements: list[Statement] = field(default_factory=list)
    contexts: list[tuple[str, str, str] | None] = field(default_factory=list)
    handlers: list[_Handler] = field(default_factory=list)
    #: Raised by the next ``execute`` whose SQL contains this fragment.
    raise_on: dict[str, Exception] = field(default_factory=dict)
    config: Any = None

    # ------------------------------------------------------------------ scripting

    def on(self, match: str, rows: list[tuple] | None = None,
           rowcount: int | None = None) -> "FakeDatabase":
        """Queue a result for statements containing ``match``.

        Queued in order, so a repository that runs the same shape of query twice can
        be handed two different answers.
        """
        cursor = FakeCursor(rows or [], rowcount)
        for handler in self.handlers:
            if handler.match == match:
                handler.results.append(cursor)
                return self
        self.handlers.append(_Handler(match=match, results=[cursor]))
        return self

    def respond(self, sql: str, params: tuple | None) -> FakeCursor:
        # Matched against the whitespace-collapsed SQL so a test's fragment can span
        # the line breaks the real statements are formatted with.
        squashed = " ".join(sql.split())
        for fragment, exc in self.raise_on.items():
            if fragment in squashed:
                raise exc
        for handler in self.handlers:
            if handler.match in squashed and handler.results:
                return handler.results.pop(0)
        # An unscripted statement is a write in nearly every case; report one row
        # affected so an upsert's rowcount reads as success.
        return FakeCursor([], rowcount=1)

    # -------------------------------------------------------------------- contexts

    @contextmanager
    def as_identity(self, tenant_id: str, user_uid: str,
                    role: str) -> Iterator[FakeConnection]:
        if not tenant_id:
            raise AssertionError("refusing to query without a tenant")
        context = (tenant_id, user_uid, role)
        self.contexts.append(context)
        yield FakeConnection(self, context)

    def as_system(self, tenant_id: str):
        return self.as_identity(tenant_id, "system", "admin")

    @contextmanager
    def without_tenant(self, reason: str) -> Iterator[FakeConnection]:
        self.contexts.append(None)
        yield FakeConnection(self, None)

    # ------------------------------------------------------------------ assertions

    def sql_for(self, match: str) -> list[Statement]:
        return [s for s in self.statements if match in s.squashed]

    def tenants_seen(self) -> set[str]:
        return {s.context[0] for s in self.statements if s.context}
