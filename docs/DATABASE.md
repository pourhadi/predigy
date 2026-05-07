# Database setup & operations

Setup recipe and day-to-day operational reference for predigy's
Postgres. Architectural rationale lives in `ARCHITECTURE.md`; this
doc is the runbook.

---

## One-time setup

### Install Postgres 16 (macOS)

```bash
brew install postgresql@16
brew services start postgresql@16

# Persist the bin path so future shells can use psql / pg_dump / etc.
echo 'export PATH="/opt/homebrew/opt/postgresql@16/bin:$PATH"' >> ~/.zprofile
```

The Homebrew formula creates a default cluster at
`/opt/homebrew/var/postgresql@16` with `peer` auth on the local
UNIX socket — the OS user (`dan`) becomes the DB role with no
password. That's the auth model we use; do NOT enable password
auth without changing `pg_hba.conf` and adding password storage
to the engine config.

### Create the predigy database

```bash
createdb predigy
psql -U dan -d predigy -c "SELECT current_database(), current_user;"
# expected:
#   current_database | current_user
#   ─────────────────┼──────────────
#   predigy          │ dan
```

### Apply the schema

```bash
psql -U dan -d predigy -f migrations/0001_initial.sql
```

The file is idempotent (`CREATE TABLE IF NOT EXISTS`), safe to
re-run.

### Bootstrap from existing JSON state

The `predigy-import` tool mirrors the legacy JSON state files
(`~/.config/predigy/oms-state-*.json`, `*-rules.json`) into the
DB. Idempotent — running it twice doesn't duplicate.

```bash
cargo build --release -p predigy-import
./target/release/predigy-import
# Reports:
#   DB now has: <N> markets, <M> intents, <K> rules, ... positions
```

While the migration is in progress (Phases 1-3 in
`ARCHITECTURE.md`), schedule `predigy-import` hourly via launchd
so the DB stays in sync with whatever the existing daemons are
writing to JSON. After Phase 3 (engine writes directly), retire
the import tool.

---

## Connection convention

All predigy code connects via:

```
postgresql:///predigy
```

No host, no port, no user, no password. Postgres falls through
to the local UNIX socket and authenticates as the running OS
user. Single-machine, single-OS-user deployment for the
foreseeable future.

If we ever go multi-machine, override with the `DATABASE_URL`
env var or the binary's `--database-url` flag. Don't bake hosts
into source.

---

## Schema overview

13 tables. Full definitions in `migrations/0001_initial.sql`;
quick reference here.

| Table | Purpose | Append-only? | FK refs |
|---|---|---|---|
| `markets` | Static-ish per-market metadata. | No (last_updated_at) | — |
| `intents` | Audit trail: every order ever submitted. | Updated in place for status transitions. | markets |
| `intent_events` | Per-intent state-transition history. | Yes | intents |
| `fills` | Every fill ever, with venue id for dedup. | Yes | intents, markets |
| `positions` | Per (strategy, ticker, side) lifecycle. | Updated as fills land. | markets |
| `model_p_snapshots` | Time series of every model_p computed. | Yes | markets |
| `model_p_inputs` | Raw inputs (NBM quantiles, etc.) for replay. | Yes | — |
| `rules` | Currently-active strategy rules (upserted). | No (upserts) | markets |
| `kill_switches` | Per-scope kill flags. | No (toggled) | — |
| `calibration` | Per-bucket Platt coefficients. | No (upserts) | — |
| `settlements` | Resolved markets with outcome. | Yes | markets |
| `book_snapshots` | Latest top-of-book per ticker. | No (latest only) | markets |
| `schema_meta` | Application-level schema version pin. | No | — |

### Big-volume tables

`model_p_snapshots` is the one that grows fast — every strategy
emits rows continuously. ~5K-50K rows/day depending on coverage.
Indexed on `(ticker, ts DESC)` for "latest model_p" queries.

When the table reaches ~10M rows (~1-2 years), evaluate either:
- TimescaleDB extension to convert to a hypertable
- Tiered retention (delete `WHERE ts < now() - INTERVAL '90 days'`)

`fills` and `intents` grow more slowly (limited by trade rate).
No special handling needed for years.

---

## Day-to-day queries

### Current open positions per strategy

```sql
SELECT strategy, ticker, side, current_qty,
       avg_entry_cents,
       (now() - opened_at)::TEXT AS open_for
  FROM positions
 WHERE closed_at IS NULL
 ORDER BY strategy, opened_at DESC;
```

### Today's realised P&L by strategy

```sql
SELECT strategy,
       SUM(realized_pnl_cents) AS pnl_cents,
       COUNT(*)                AS n_positions
  FROM positions
 WHERE closed_at >= date_trunc('day', now())
 GROUP BY strategy
 ORDER BY pnl_cents DESC;
```

### Latest model_p per ticker

```sql
SELECT DISTINCT ON (ticker)
       ticker, strategy, ts, raw_p, model_p, source
  FROM model_p_snapshots
 ORDER BY ticker, ts DESC;
```

### Calibration histogram (samples per bucket)

```sql
SELECT strategy, scope_key, month, n_samples, a, b, fitted_at
  FROM calibration
 ORDER BY strategy, scope_key, month;
```

### Settled markets we predicted on (for offline calibration fit)

```sql
SELECT m.ticker,
       s.resolved_value,
       p.raw_p,
       p.model_p,
       p.ts        AS predicted_at,
       s.settled_at
  FROM settlements s
  JOIN model_p_snapshots p
    ON p.ticker = s.ticker
   AND p.ts <= s.settled_at
  JOIN markets m
    ON m.ticker = s.ticker
 ORDER BY s.settled_at DESC
 LIMIT 100;
```

---

## Backups

### Manual

```bash
pg_dump predigy | gzip > ~/.config/predigy/backups/predigy-$(date +%F-%H%M).sql.gz
```

### Automatic (recommended once DB has live writes)

A daily launchd job that runs the above, plus rotates older than
30 days. Plist to be added in Phase 2. Until then, do the manual
dump before any risky DB operation.

### Restore

```bash
gunzip < predigy-FILE.sql.gz | psql predigy
```

(Restore into an empty `predigy` DB. If you're restoring on top
of a partial DB, drop and recreate first: `dropdb predigy &&
createdb predigy`.)

---

## Migrations going forward

Adding a new migration:

```bash
cargo install sqlx-cli --no-default-features --features postgres   # one-time
cd /Users/dan/code/predigy
sqlx migrate add <name>      # creates migrations/<ts>_<name>.sql
$EDITOR migrations/<ts>_<name>.sql
sqlx migrate run --database-url postgresql:///predigy
```

`sqlx migrate run` is forward-only. To "roll back" a bad change,
write a fix-forward migration with `DROP COLUMN ... ; ADD COLUMN
... ;` etc. Don't try to use `sqlx migrate revert` — too easy to
diverge dev / prod state.

The engine binary will run pending migrations on startup
(`sqlx::migrate!()` macro). For dev, run them manually first so
the `cargo build`-time query checks see the up-to-date schema.

---

## Compile-time query checking

`sqlx::query!` and friends type-check queries against the live
database at `cargo build` time. To make this work:

```bash
# One-time: tell sqlx where the DB is for build-time checks.
echo 'DATABASE_URL=postgresql:///predigy' > .env
```

Then `cargo build` connects to the DB during compilation,
verifies every `sqlx::query!` against the current schema, and
generates typed result rows. Catches "forgot to add column to a
SELECT" before deploy.

For CI / offline builds without a DB available, generate query
metadata: `cargo sqlx prepare`. Commits the `.sqlx/` directory.
Then `SQLX_OFFLINE=true cargo build` works against the cached
metadata.

---

## When something looks wrong

### DB out of sync with JSON state files

Symptom: `predigy-import` reports counts that don't match the
JSON. Usually means a daemon wrote since the last import.

Fix: run `predigy-import` again. It's idempotent. If still out
of sync, inspect the timestamps:

```sql
SELECT MAX(last_updated_at) FROM intents;
SELECT MAX(last_updated_at) FROM markets;
```

vs `stat -f %Sm ~/.config/predigy/oms-state-*.json`. If the JSON
is newer, the import is stale; re-run. If the DB is newer,
something in the engine path is writing already (good).

### Postgres won't start after sleep / reboot

```bash
brew services restart postgresql@16
```

If that fails: check `/opt/homebrew/var/log/postgresql@16.log`
for startup errors. Common culprit is a stale `postmaster.pid`:

```bash
rm /opt/homebrew/var/postgresql@16/postmaster.pid
brew services restart postgresql@16
```

### Connection refused from a binary

The binary is using `peer` auth, so `whoami` must equal the DB
role. `dan` works for everything launched as the user;
`launchctl`-spawned tasks inherit the user; sudo'd commands
become root and break.

Verify: `psql -U dan -d predigy -c "SELECT 1"` from the same
shell. If that works and the binary fails, check the binary
isn't sudo'd or running under a different user.

### Disk full on the DB volume

Most likely `model_p_snapshots` exploded. Check size:

```sql
SELECT pg_size_pretty(pg_relation_size('model_p_snapshots'));
SELECT pg_size_pretty(pg_total_relation_size('model_p_snapshots'));
```

Trim old rows:

```sql
DELETE FROM model_p_snapshots WHERE ts < now() - INTERVAL '90 days';
VACUUM (ANALYZE) model_p_snapshots;
```

If still tight, run `VACUUM FULL` (locks the table — schedule
during a quiet window).

---

## Migration status (live tracker)

Update this section as Phase 0-7 (see `ARCHITECTURE.md`)
progress.

| Phase | Status | Notes |
|---|---|---|
| 0. Setup (install + DB) | **Done 2026-05-07** | Postgres 16.13, peer auth |
| 1. Schema + import tool | **Done 2026-05-07** | 13 tables, predigy-import idempotent |
| 2. Engine skeleton + DB read path | Pending | Dashboard moves to DB |
| 3. Stat-trader as first module + dual-write | Pending | First strategy ports |
| 4. FIX wired in | Pending | Hot-path orders |
| 5. Port remaining strategies | Pending | latency, cross-arb, settlement, wx-stat |
| 6. Active position management | Pending | Per-position re-eval |
| 7. Retire scaffolding | Pending | JSON output compat layer goes |
