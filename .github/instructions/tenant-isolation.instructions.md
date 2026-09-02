---
applyTo: "db/migrations/**,server/gateway/internal/store/**,server/gateway/internal/auth/**"
description: "Row-level security, tenant isolation and migration rules for db/migrations and the gateway store/auth packages. Use when adding a table, writing a policy, adding a query path, or debugging a query that returns no rows."
---

# Tenant isolation

`db/` is the security boundary. The application does not filter by tenant — PostgreSQL does.
Anything that weakens that is a defect, not a convenience.

## The context contract

The gateway connects as `sentinel_app` (`NOBYPASSRLS`) and, before every query, sets three
transaction-local settings **from verified token claims only**:

```sql
SET LOCAL sentinel.tenant_id = '<uuid>';
SET LOCAL sentinel.user_uid  = '<firebase uid>';
SET LOCAL sentinel.role      = 'agent' | 'supervisor' | 'qa' | 'compliance' | 'admin' | 'client';
```

Nothing from a request body, path or query string may reach these. Policies read them through
`sentinel_tenant()`, `sentinel_uid()`, `sentinel_role()`, `sentinel_team()` — defined in
[db/migrations/0002_rls.up.sql](db/migrations/0002_rls.up.sql).

**Absent context must yield zero rows.** `sentinel_role()` falls back to `'none'`, which no
branch of `sentinel_can_see_call` matches. If a query unexpectedly returns nothing, the bug is a
missing `AsIdentity` wrapper — fix the call site, never relax the policy.

## Adding a query path

Go through `Store.AsIdentity` (caller-scoped) or `Store.AsSystem` (ingest and other trusted
writers) in [server/gateway/internal/store/store.go](server/gateway/internal/store/store.go).
`SET LOCAL` is transaction-scoped precisely so context cannot leak to the next borrower of a
pooled connection; do not hoist it to connection setup, and do not open a bare `pool.Query`.

## Adding a table

If it carries tenant data:

1. `ALTER TABLE … ENABLE ROW LEVEL SECURITY` **and** `FORCE ROW LEVEL SECURITY` — `FORCE` is
   what makes the policy apply to the table owner too.
2. Add a tenant policy `USING (tenant_id = sentinel_tenant())`. For child tables that only carry
   `call_id`, use `sentinel_call_visible(call_id)`.
3. Grant to `sentinel_app` / `sentinel_pipeline` explicitly. Neither role may be given
   `BYPASSRLS`.
4. Extend the visibility matrix in `sentinel_can_see_call` if the role semantics differ.
   The `client` role sees flagged calls only — that is a contractual promise to the bank.

`SECURITY DEFINER` functions are reserved for the three operations that legitimately precede a
tenant context: consuming an enrollment token, registering the enrolled device, and resolving a
client certificate to a device. Adding a fourth needs a written justification.

## Migration mechanics

- Sequential prefix, both directions: `NNNN_name.up.sql` and `NNNN_name.down.sql`. The down
  migration must genuinely reverse the up, including restoring prior function bodies.
- Wrap each file in `BEGIN; … COMMIT;`.
- Migrations are applied in filename order and are never edited once merged — add a new pair.
- The schema uses `vector(1024)`, so local Postgres must be `pgvector/pgvector:pg16`. The stub
  extension installed by `db/test/pgtest.sh` exists only so tests can run without pgvector;
  production images carry the real one.

## Verification

```bash
bash db/test/rls_test.sh      # required after any db/migrations/ change
bash db/test/gateway_it.sh    # gateway against a freshly migrated throwaway cluster
```

`rls_test.sh` asserts against the database as `sentinel_app` itself. Add a case to it for every
new isolation property — cross-tenant reads, agent-to-agent isolation, supervisor team scoping,
the client flagged-only view, and the missing-context case.

The role/capability matrix in `server/gateway/internal/auth` is the authoritative one;
[web/shared/src/auth/roles.ts](web/shared/src/auth/roles.ts) is a UI mirror and must be updated
to match, never the other way round.
