"""Postgres access for the pipeline, under row-level security.

The pipeline connects as ``sentinel_pipeline``, created ``NOBYPASSRLS`` by
``db/migrations/0003_roles.up.sql``. That is not a formality: it means a query in
this package that forgets its tenant context returns **zero rows** rather than every
tenant's, and ``db/test/rls_test.sh`` asserts exactly that property of the sibling
role. Nothing here may be the thing that undoes it.

Three transaction-local settings carry the context, and the policies in
``db/migrations/0002_rls.up.sql`` read them through ``sentinel_tenant()``,
``sentinel_uid()`` and ``sentinel_role()``:

    SELECT set_config('sentinel.tenant_id', $1, true),
           set_config('sentinel.user_uid',  $2, true),
           set_config('sentinel.role',      $3, true);

This mirrors ``Store.AsIdentity`` in ``server/gateway/internal/store/store.go``
deliberately, including the details that look like details:

* ``set_config(..., true)`` is transaction-scoped, so the context cannot leak to the
  next borrower of a pooled connection. Setting it on connect — the obvious
  optimisation — would leak one tenant's context into another tenant's query the
  first time the pool handed the connection on.
* Statement preparation is disabled. The gateway sets
  ``QueryExecModeDescribeExec`` for the same reason: server-side prepared statements
  do not survive a transaction-pooling proxy, and this service is expected to sit
  behind one.
* ``as_system`` refuses to run with an empty tenant. A "system" path with no tenant
  is a path with no policy constraint, and the correct behaviour for a bug that
  loses the tenant id is to fail the call, not to widen the query.

``psycopg`` is imported inside the methods that need it so the unit tests need no
database and no driver, the same convention the provider adapters use for their SDKs.
"""

from __future__ import annotations

import logging
import os
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Iterator

log = logging.getLogger(__name__)

#: The role the pipeline claims in the RLS context. ``admin`` because the pipeline
#: legitimately writes rows belonging to any user in the tenant — it is finalizing
#: whatever call the gateway handed it — while remaining unable to cross a tenant
#: boundary. ``AsSystem`` in the gateway makes the same choice for the same reason.
SYSTEM_ROLE = "admin"
#: The actor recorded in ``audit_log`` for work no human asked for individually.
SYSTEM_UID = "system"

_SET_CONTEXT = """
SELECT set_config('sentinel.tenant_id', %s, true),
       set_config('sentinel.user_uid',  %s, true),
       set_config('sentinel.role',      %s, true)
"""


class DatabaseConfigError(RuntimeError):
    """The database configuration cannot be honoured as written."""


@dataclass(frozen=True)
class DatabaseConfig:
    """Connection settings.

    ``SENTINEL_PIPELINE_DATABASE_URL`` is deliberately a different variable from the
    gateway's DSN: the two connect as different roles, and one shared variable is how
    a pipeline ends up running as ``sentinel_app`` — or worse, as the schema owner,
    which does bypass RLS.
    """

    dsn: str
    min_size: int = 1
    max_size: int = 8
    connect_timeout_s: int = 10
    #: Shows up in ``pg_stat_activity``, which is where someone looks first when a
    #: nightly job is holding a lock.
    application_name: str = "sentinel-pipeline"

    @staticmethod
    def from_env(env: dict[str, str] | None = None) -> "DatabaseConfig":
        env = dict(os.environ if env is None else env)
        dsn = env.get("SENTINEL_PIPELINE_DATABASE_URL", "").strip()
        if not dsn:
            raise DatabaseConfigError(
                "SENTINEL_PIPELINE_DATABASE_URL is not set; the pipeline must connect "
                "as the sentinel_pipeline role (NOBYPASSRLS, db/migrations/0003)"
            )
        return DatabaseConfig(
            dsn=dsn,
            min_size=int(env.get("SENTINEL_PIPELINE_DB_POOL_MIN", "1")),
            max_size=int(env.get("SENTINEL_PIPELINE_DB_POOL_MAX", "8")),
            connect_timeout_s=int(env.get("SENTINEL_PIPELINE_DB_CONNECT_TIMEOUT", "10")),
        )


class Database:
    """A psycopg connection pool that only hands out transactions with a context.

    There is no method that returns a bare connection. Everything goes through
    :meth:`as_identity` or :meth:`as_system`, which is the same shape as the
    gateway's store: making the unscoped path unavailable is more reliable than
    remembering to scope every query.
    """

    def __init__(self, config: DatabaseConfig, *, pool: object | None = None) -> None:
        self.config = config
        self._pool = pool

    # ------------------------------------------------------------------ lifecycle

    def open(self, *, wait: bool = True) -> "Database":
        if self._pool is not None:
            return self
        from psycopg_pool import ConnectionPool  # noqa: PLC0415 - lazy, see module docstring

        self._pool = ConnectionPool(
            conninfo=self.config.dsn,
            min_size=self.config.min_size,
            max_size=self.config.max_size,
            timeout=self.config.connect_timeout_s,
            kwargs={
                # See the module docstring: prepared statements do not survive a
                # transaction pooler, and this is the pipeline's SET LOCAL problem
                # in exactly the form the gateway hit.
                "prepare_threshold": None,
                "application_name": self.config.application_name,
                "autocommit": False,
            },
            open=False,
        )
        self._pool.open(wait=wait, timeout=self.config.connect_timeout_s)
        return self

    def close(self) -> None:
        if self._pool is not None:
            self._pool.close()
            self._pool = None

    def __enter__(self) -> "Database":
        return self.open()

    def __exit__(self, *exc_info: object) -> None:
        self.close()

    # ------------------------------------------------------------------- contexts

    @contextmanager
    def as_identity(self, tenant_id: str, user_uid: str, role: str) -> Iterator[object]:
        """Run inside a transaction carrying the RLS context.

        Commits on a clean exit and rolls back on an exception, so a finalize that
        fails half way leaves no partial compliance record behind — the message is
        redelivered and the whole call is written again.
        """
        if not tenant_id:
            raise DatabaseConfigError(
                "refusing to query without a tenant: the RLS policies would return "
                "nothing and the caller would read that as an empty tenant"
            )
        if self._pool is None:
            raise DatabaseConfigError("database pool is not open; call open() first")
        with self._pool.connection() as conn:
            with conn.transaction():
                conn.execute(_SET_CONTEXT, (tenant_id, user_uid, role))
                yield conn

    @contextmanager
    def without_tenant(self, reason: str) -> Iterator[object]:
        """A transaction with **no** RLS context. Only for ``SECURITY DEFINER`` calls.

        There is exactly one thing a scheduled job legitimately needs before it has a
        tenant: the list of tenants to visit. That lookup goes through the
        ``SECURITY DEFINER`` function added by ``db/migrations/0008``, which returns
        tenant ids and retention periods and nothing else — the same trade
        ``db/migrations/0005`` made for the gateway's three bootstrap lookups, and
        the same reason: a narrow function is auditable, a loosened policy is not.

        Every ordinary table query inside this block returns **zero rows**, because
        the policies evaluate ``sentinel_tenant()`` as NULL. That is the designed
        failure mode and it must stay that way: if a query in here ever starts
        returning rows, row-level security has been turned off somewhere.

        ``reason`` is required and logged so each use has to say what it is for.
        """
        if self._pool is None:
            raise DatabaseConfigError("database pool is not open; call open() first")
        log.debug("querying without a tenant context", extra={"reason": reason})
        with self._pool.connection() as conn:
            with conn.transaction():
                yield conn

    def as_system(self, tenant_id: str) -> "object":
        """Run tenant-scoped work that no user requested: finalize, retention, coverage.

        Mirrors ``Store.AsSystem``. It still sets ``sentinel.tenant_id``, so the
        policies constrain it to one tenant; what it cannot do is cross a tenant
        boundary, and that is the property worth keeping.
        """
        return self.as_identity(tenant_id, SYSTEM_UID, SYSTEM_ROLE)

    # ------------------------------------------------------------------ assertions

    def assert_rls_enforced(self) -> None:
        """Refuse to start if the connected role can bypass row-level security.

        Cheap, once, at boot. The failure it catches is a deployment that points
        ``SENTINEL_PIPELINE_DATABASE_URL`` at the schema owner or a superuser — at
        which point every query in this package silently sees every tenant and no
        test anywhere notices, because all the application-level filters still pass.
        """
        if self._pool is None:
            raise DatabaseConfigError("database pool is not open; call open() first")
        with self._pool.connection() as conn:
            row = conn.execute(
                "SELECT current_user, rolbypassrls, rolsuper FROM pg_roles "
                "WHERE rolname = current_user"
            ).fetchone()
        if row is None:  # pragma: no cover - current_user is always in pg_roles
            raise DatabaseConfigError("could not read the connected role from pg_roles")
        user, bypass, superuser = row[0], bool(row[1]), bool(row[2])
        if bypass or superuser:
            raise DatabaseConfigError(
                f"the pipeline is connected as {user!r}, which bypasses row-level "
                f"security. Point SENTINEL_PIPELINE_DATABASE_URL at the "
                f"sentinel_pipeline role (db/migrations/0003_roles.up.sql)."
            )
        log.info("row-level security enforced for the pipeline role",
                 extra={"db_role": user})


# ------------------------------------------------------------------- identifiers

#: Crockford base32, as ``contracts/wire.md`` §3.1 specifies for ``call_id``.
_CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
_CROCKFORD_INDEX = {c: i for i, c in enumerate(_CROCKFORD)}


def call_uuid(call_id: str) -> str:
    """Reinterpret a 26-character ULID as the UUID the ``calls`` table is keyed by.

    The client mints the call id as a ULID (that is what makes retry-after-reconnect
    idempotent, ``wire.md`` §5.1) and the gateway stores its 128 bits verbatim in a
    ``uuid`` column — ``callUUIDFromULID`` in
    ``server/gateway/internal/ingest/sink.go``. This is the same conversion, and it
    has to stay the same conversion: the finalize message carries the ULID, every row
    is keyed by the UUID, and a divergence would look like a call that does not
    exist.

    A call id that is already in UUID form is accepted and returned as-is. The
    gateway's finalize outbox (``db/migrations/0007``) stores ``call_id`` as a
    ``uuid``, so whether the drainer publishes the ULID or the UUID it read back is
    an implementation detail of the producer — and both spellings name the same 128
    bits. Refusing one of them would turn a cosmetic difference into every call
    dead-lettering.
    """
    text = call_id.strip().upper()
    if len(text) == 36 and text.count("-") == 4:
        from uuid import UUID  # noqa: PLC0415 - stdlib, only this branch needs it

        return str(UUID(text.lower()))
    if len(text) != 26:
        raise ValueError(f"call id is not a 26-character ULID: {call_id!r}")
    value = 0
    for char in text:
        index = _CROCKFORD_INDEX.get(char)
        if index is None:
            raise ValueError(f"call id is not Crockford base32: {call_id!r}")
        value = value * 32 + index
    if value >= 1 << 128:
        # 26 base32 characters carry 130 bits; a valid ULID's top two are zero.
        # oklog/ulid's ParseStrict rejects the overflow, so this does too rather than
        # truncating into a UUID that collides with a different call.
        raise ValueError(f"call id overflows 128 bits: {call_id!r}")
    hexed = f"{value:032x}"
    return f"{hexed[0:8]}-{hexed[8:12]}-{hexed[12:16]}-{hexed[16:20]}-{hexed[20:32]}"
