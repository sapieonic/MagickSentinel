// Package testdb hands each database-backed test package its own database.
//
// It exists because `go test ./...` runs packages in parallel — one test binary per
// package, up to GOMAXPROCS of them at once — while both database-backed suites were
// pointed at the single database named by SENTINEL_TEST_DATABASE_URL. Tests within a
// package are sequential, so each suite was internally consistent; across packages
// they were not. internal/api's fixture reseeds with
//
//	TRUNCATE … calls, devices, …, tenants RESTART IDENTITY CASCADE
//
// which is indiscriminate by construction: CASCADE has to reach call_finalize_outbox
// because that table references calls. Whenever that truncate landed between
// internal/store's seed and its next write, the store suite failed on a foreign key
// whose parent row it had just inserted:
//
//	insert or update on table "call_finalize_outbox" violates foreign key constraint
//	"call_finalize_outbox_call_id_fkey"  (Key is not present in table "calls".)
//
// Scoping each suite's deletes to its own rows cannot fix that — the truncate is what
// crosses the boundary, and it cannot be narrowed while the outbox cascades from
// calls, which is a property of the schema worth keeping. Serialising the packages
// with `go test -p 1` would also fix it, at the cost of making every future
// database-backed package wait on every other one.
//
// So the isolation moves down to the database. The database in the DSN is treated as
// a migrated *template*: the first fixture in each test binary clones it with
// CREATE DATABASE … TEMPLATE and every pool in that binary connects to the clone.
// Suites then get the real schema — the real RLS policies and the real SECURITY
// DEFINER functions, which is the whole point of these tests — and a truncate in one
// is invisible to the others.
//
// A schema per suite was the alternative and does not work here: the SECURITY DEFINER
// functions in db/migrations are declared `SET search_path = public`, so they would
// keep reaching into the shared schema no matter what the caller's search_path said.
//
// Adding a database-backed package needs no change to db/test/gateway_it.sh or to CI:
// call Open with a name nobody else is using.
package testdb

import (
	"context"
	"fmt"
	"os"
	"regexp"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

// Pools are the two connections a database-backed fixture needs, both pointed at the
// calling package's own clone of the template database.
type Pools struct {
	// App connects as the application role from SENTINEL_TEST_DATABASE_URL —
	// sentinel_app, which is NOBYPASSRLS. Every assertion about what a caller can
	// see has to run through this one.
	App *pgxpool.Pool
	// Admin connects as the schema owner from SENTINEL_TEST_ADMIN_DATABASE_URL. It
	// is for seeding, for TRUNCATE (which needs ownership) and for reading back rows
	// the application role is correctly forbidden from seeing.
	Admin *pgxpool.Pool
}

// Open returns pools onto a database private to the calling test package.
//
// suite names the clone and must be unique across packages; the package name is the
// obvious choice. The clone is created once per test binary and reused by every
// fixture in it, because the suites reseed themselves and cloning is not free.
//
// Skips — rather than fails — when the SENTINEL_TEST_* variables are unset, so
// `go test ./...` stays runnable without a database.
func Open(t *testing.T, suite string) *Pools {
	t.Helper()

	dsn := os.Getenv("SENTINEL_TEST_DATABASE_URL")
	if dsn == "" {
		t.Skip("SENTINEL_TEST_DATABASE_URL not set; run via db/test/gateway_it.sh")
	}
	adminDSN := os.Getenv("SENTINEL_TEST_ADMIN_DATABASE_URL")
	if adminDSN == "" {
		t.Skip("SENTINEL_TEST_ADMIN_DATABASE_URL not set; run via db/test/gateway_it.sh")
	}

	appCfg, err := pgxpool.ParseConfig(dsn)
	if err != nil {
		t.Fatalf("parse SENTINEL_TEST_DATABASE_URL: %v", err)
	}
	adminCfg, err := pgxpool.ParseConfig(adminDSN)
	if err != nil {
		t.Fatalf("parse SENTINEL_TEST_ADMIN_DATABASE_URL: %v", err)
	}

	ctx := context.Background()
	// The two DSNs must name the same database, or "the template" is ambiguous and
	// the app and admin pools would end up in different clones.
	if appCfg.ConnConfig.Database != adminCfg.ConnConfig.Database {
		t.Fatalf("the app and admin DSNs name different databases (%q and %q)",
			appCfg.ConnConfig.Database, adminCfg.ConnConfig.Database)
	}
	// Bounded, because a clone that never returns would otherwise surface as the
	// whole package timing out ten minutes later with no indication of why.
	cloneCtx, cancel := context.WithTimeout(ctx, 2*time.Minute)
	defer cancel()
	clone, err := cloneOnce(cloneCtx, adminCfg, appCfg.ConnConfig.Database, suite)
	if err != nil {
		t.Fatalf("clone the template database for %s: %v", suite, err)
	}

	appCfg.ConnConfig.Database = clone
	adminCfg.ConnConfig.Database = clone

	app, err := pgxpool.NewWithConfig(ctx, appCfg)
	if err != nil {
		t.Fatalf("connect as the application role: %v", err)
	}
	t.Cleanup(app.Close)
	if err := app.Ping(ctx); err != nil {
		t.Fatalf("ping as the application role: %v", err)
	}
	admin, err := pgxpool.NewWithConfig(ctx, adminCfg)
	if err != nil {
		t.Fatalf("connect as the schema owner: %v", err)
	}
	t.Cleanup(admin.Close)

	return &Pools{App: app, Admin: admin}
}

var (
	cloneMu   sync.Mutex
	cloneName string
	cloneErr  error
	cloned    bool
)

// cloneOnce creates the clone the first time it is called in a test binary and
// returns the same name afterwards, including when the first attempt failed — a
// second fixture retrying a CREATE DATABASE that has already failed once only
// produces a slower, noisier version of the same error.
func cloneOnce(ctx context.Context, adminCfg *pgxpool.Config, template, suite string) (string, error) {
	cloneMu.Lock()
	defer cloneMu.Unlock()
	if cloned {
		return cloneName, cloneErr
	}
	cloned = true
	cloneName, cloneErr = createClone(ctx, adminCfg, template, suite)
	return cloneName, cloneErr
}

var unsafeIdent = regexp.MustCompile(`[^a-z0-9_]+`)

// cloneLockKey namespaces the advisory lock taken while copying the template.
const cloneLockKey = int64(0x5e_47_1e_10_c1_04_1e)

func createClone(ctx context.Context, adminCfg *pgxpool.Config, template, suite string) (string, error) {
	name := template + "_" + unsafeIdent.ReplaceAllString(strings.ToLower(suite), "_")
	if len(name) > 63 { // Postgres truncates longer identifiers, which would silently collide.
		return "", fmt.Errorf("clone name %q exceeds the 63-byte identifier limit", name)
	}

	// CREATE DATABASE cannot run from inside the database being copied — a session
	// connected to the template counts as "being accessed by other users" — so the
	// maintenance connection goes to `postgres`, which initdb always creates.
	maint := adminCfg.ConnConfig.Copy()
	maint.Database = "postgres"
	conn, err := pgx.ConnectConfig(ctx, maint)
	if err != nil {
		return "", fmt.Errorf("connect to the maintenance database: %w", err)
	}
	defer func() { _ = conn.Close(ctx) }()

	// Two test binaries reach this point concurrently — that is the situation this
	// package exists for — and both then copy the same template. The advisory lock
	// serialises just the copy; it is session-scoped and this session is about to be
	// closed, so it needs no explicit unlock. The key is an arbitrary constant that
	// only this package uses.
	if _, err := conn.Exec(ctx, `SELECT pg_advisory_lock($1)`, cloneLockKey); err != nil {
		return "", fmt.Errorf("take the clone lock: %w", err)
	}

	// FORCE terminates connections a previous run left behind. Without it a
	// re-run against a long-lived cluster fails here rather than in a test, which
	// is a confusing way to learn that an old pool is still open.
	if _, err := conn.Exec(ctx, fmt.Sprintf(`DROP DATABASE IF EXISTS %s WITH (FORCE)`, quoteIdent(name))); err != nil {
		return "", fmt.Errorf("drop the previous clone: %w", err)
	}
	if _, err := conn.Exec(ctx, fmt.Sprintf(`CREATE DATABASE %s TEMPLATE %s`,
		quoteIdent(name), quoteIdent(template))); err != nil {
		return "", fmt.Errorf("copy %s: %w (the admin role needs CREATEDB)", template, err)
	}
	return name, nil
}

// quoteIdent double-quotes an identifier. The names here are derived from a DSN and a
// package-chosen suite name rather than from anything a request controls, but an
// unquoted identifier that happens to be a keyword fails in a way that reads like a
// database problem rather than a naming one.
func quoteIdent(s string) string {
	return `"` + strings.ReplaceAll(s, `"`, `""`) + `"`
}
