"""Database access: the RLS context, and the call-id conversion.

``db/test/rls_test.sh`` asserts what Postgres does when the context is missing — zero
rows rather than every tenant's. These tests assert the other half, which no database
test can see: that this package always sets it, sets it transaction-locally, and
refuses the two configurations that would quietly turn row-level security off.
"""

from contextlib import contextmanager

import pytest

from sentinel_pipeline.db import (
    SYSTEM_ROLE,
    SYSTEM_UID,
    Database,
    DatabaseConfig,
    DatabaseConfigError,
    call_uuid,
)


# ------------------------------------------------------------------- fake psycopg


class FakeConn:
    def __init__(self, log, rows=None):
        self.log = log
        self.rows = rows or []
        self.in_transaction = False

    @contextmanager
    def transaction(self):
        self.in_transaction = True
        self.log.append(("begin", None))
        try:
            yield self
            self.log.append(("commit", None))
        except Exception:
            self.log.append(("rollback", None))
            raise
        finally:
            self.in_transaction = False

    def execute(self, sql, params=None):
        # Every statement records whether a transaction was open when it ran. A
        # set_config(..., true) outside one is not transaction-local, and a context
        # that is not transaction-local leaks to the next borrower of the connection.
        self.log.append((" ".join(sql.split()), params, self.in_transaction))
        return FakeCursor(self.rows)


class FakeCursor:
    def __init__(self, rows):
        self.rows = rows

    def fetchone(self):
        return self.rows[0] if self.rows else None

    def fetchall(self):
        return list(self.rows)


class FakePool:
    def __init__(self, rows=None):
        self.log = []
        self.rows = rows or []
        self.closed = False
        self.handed_out = 0

    @contextmanager
    def connection(self):
        self.handed_out += 1
        yield FakeConn(self.log, self.rows)

    def close(self):
        self.closed = True


def build(rows=None) -> tuple[Database, FakePool]:
    pool = FakePool(rows)
    return Database(DatabaseConfig(dsn="postgresql:///test"), pool=pool), pool


def statements(pool):
    return [entry for entry in pool.log if len(entry) == 3]


# ------------------------------------------------------------------ the context


def test_the_three_session_variables_are_set_before_any_query():
    db, pool = build()
    with db.as_identity("tenant-1", "agent-a", "supervisor") as conn:
        conn.execute("SELECT 1")

    first = statements(pool)[0]
    assert "set_config('sentinel.tenant_id', %s, true)" in first[0]
    assert "set_config('sentinel.user_uid', %s, true)" in first[0]
    assert "set_config('sentinel.role', %s, true)" in first[0]
    assert first[1] == ("tenant-1", "agent-a", "supervisor")


def test_the_context_is_transaction_local_and_inside_the_transaction():
    # `true` as the third argument to set_config is what makes it SET LOCAL. Setting
    # it on connect instead — the obvious optimisation — hands one tenant's context
    # to the next query that borrows the connection from the pool.
    db, pool = build()
    with db.as_identity("t", "u", "admin"):
        pass
    context_stmt = statements(pool)[0]
    assert context_stmt[2] is True, "set_config ran outside a transaction"
    assert pool.log[0] == ("begin", None)
    assert pool.log[-1] == ("commit", None)


def test_a_failure_rolls_back_so_no_half_written_compliance_record_survives():
    db, pool = build()
    with pytest.raises(RuntimeError):
        with db.as_identity("t", "u", "admin") as conn:
            conn.execute("INSERT INTO flags DEFAULT VALUES")
            raise RuntimeError("provider died mid-finalize")
    assert ("rollback", None) in pool.log
    assert ("commit", None) not in pool.log


def test_as_system_claims_admin_in_one_tenant_and_nothing_wider():
    # Mirrors Store.AsSystem in the gateway: the pipeline writes rows for any user in
    # the tenant it was handed, and cannot cross a tenant boundary.
    db, pool = build()
    with db.as_system("tenant-9"):
        pass
    assert statements(pool)[0][1] == ("tenant-9", SYSTEM_UID, SYSTEM_ROLE)


def test_querying_with_no_tenant_is_refused_rather_than_run():
    # Under RLS an empty tenant returns zero rows, which a caller reads as "this
    # tenant has no calls". Failing loudly is the only honest option.
    db, _ = build()
    with pytest.raises(DatabaseConfigError):
        with db.as_system(""):
            pass


def test_without_tenant_opens_a_transaction_with_no_context_at_all():
    # The one legitimate use is the SECURITY DEFINER tenant listing. Any ordinary
    # query in here returns nothing, by design.
    db, pool = build()
    with db.without_tenant("listing tenants for a scheduled job") as conn:
        conn.execute("SELECT * FROM sentinel_pipeline_tenants()")
    assert not [s for s in statements(pool) if "set_config" in s[0]]


def test_a_closed_pool_is_a_configuration_error_not_an_attribute_error():
    db = Database(DatabaseConfig(dsn="postgresql:///x"))
    with pytest.raises(DatabaseConfigError):
        with db.as_system("t"):
            pass


# -------------------------------------------------------------- the role check


def test_a_role_that_bypasses_rls_refuses_to_start():
    # The deployment error this catches: SENTINEL_PIPELINE_DATABASE_URL pointed at
    # the schema owner. Every query then silently sees every tenant, and every
    # application-level filter still passes, so nothing else notices.
    db, _ = build(rows=[("sentinel_owner", True, False)])
    with pytest.raises(DatabaseConfigError, match="bypasses row-level security"):
        db.assert_rls_enforced()

    db, _ = build(rows=[("postgres", False, True)])
    with pytest.raises(DatabaseConfigError, match="bypasses row-level security"):
        db.assert_rls_enforced()


def test_the_pipeline_role_passes_the_check():
    db, _ = build(rows=[("sentinel_pipeline", False, False)])
    db.assert_rls_enforced()


def test_the_dsn_variable_is_the_pipelines_own():
    with pytest.raises(DatabaseConfigError, match="SENTINEL_PIPELINE_DATABASE_URL"):
        DatabaseConfig.from_env({})
    config = DatabaseConfig.from_env({
        "SENTINEL_PIPELINE_DATABASE_URL": "postgresql:///sentinel",
        "SENTINEL_PIPELINE_DB_POOL_MAX": "16",
    })
    assert config.dsn == "postgresql:///sentinel" and config.max_size == 16


# ---------------------------------------------------------------- call id → uuid

CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
# Python's int(s, 32) uses 0-9 then a-v, which is a different alphabet from
# Crockford's; translating between them gives a genuinely independent decoder to
# check the implementation against rather than a restatement of it.
_TO_PY32 = str.maketrans(CROCKFORD, "0123456789abcdefghijklmnopqrstuv")


def independent_call_uuid(ulid: str) -> str:
    value = int(ulid.upper().translate(_TO_PY32), 32)
    hexed = f"{value:032x}"
    return f"{hexed[0:8]}-{hexed[8:12]}-{hexed[12:16]}-{hexed[16:20]}-{hexed[20:32]}"


def test_a_ulid_becomes_the_uuid_the_calls_table_is_keyed_by():
    # The client mints a ULID, the gateway stores its 128 bits verbatim in a uuid
    # column (callUUIDFromULID in ingest/sink.go). If this conversion drifts, every
    # finalize looks up a call that does not exist.
    ulid = "01J8ZQ8H2Q7X9K3M4N5P6R7S8T"
    assert call_uuid(ulid) == independent_call_uuid(ulid)
    assert len(call_uuid(ulid)) == 36


@pytest.mark.parametrize("ulid", [
    "01J8ZQ8H2Q7X9K3M4N5P6R7S8T",
    "00000000000000000000000000",
    "7ZZZZZZZZZZZZZZZZZZZZZZZZZ",
])
def test_the_conversion_matches_an_independent_decoder_across_the_range(ulid):
    assert call_uuid(ulid) == independent_call_uuid(ulid)


def test_lowercase_is_accepted_because_base32_case_carries_no_information():
    ulid = "01J8ZQ8H2Q7X9K3M4N5P6R7S8T"
    assert call_uuid(ulid.lower()) == call_uuid(ulid)


def test_a_call_id_already_in_uuid_form_passes_through():
    # The gateway's finalize outbox (db/migrations/0007) holds call_id as a uuid, so
    # the producer may publish either spelling of the same 128 bits.
    assert call_uuid("0192F3E0-1234-4567-89AB-CDEF01234567") == \
        "0192f3e0-1234-4567-89ab-cdef01234567"


@pytest.mark.parametrize("bad", [
    "",
    "not-a-ulid",
    "01J8ZQ8H2Q7X9K3M4N5P6R7S8",       # 25 characters
    "01J8ZQ8H2Q7X9K3M4N5P6R7S8TT",     # 27
    "01J8ZQ8H2Q7X9K3M4N5P6R7S8I",      # I is not in the Crockford alphabet
    "ZZZZZZZZZZZZZZZZZZZZZZZZZZ",      # 130 bits set: overflows 128
])
def test_an_unusable_call_id_raises_rather_than_becoming_a_different_call(bad):
    # Truncating an overflowing id would produce a valid-looking uuid that collides
    # with some other call, which is worse than dead-lettering the message.
    with pytest.raises(ValueError):
        call_uuid(bad)
