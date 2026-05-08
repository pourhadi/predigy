# Implementation Plan: Fill Growth + Calibration Evidence

## Scope

Increase pursued fills without raising capital caps or weakening entry gates by expanding high-quality opportunity supply and adding explicit calibration evidence before any `stat` re-enable.

Targets:
1. Expand `implication-arb` and `internal-arb` universes.
2. Add a read-only opportunity scanner that writes observations only, never intents/orders.
3. Expand `wx-stat` daily temperature coverage where the model is already defensible.
4. Expand the settlement strategy universe cautiously through configurable series discovery.
5. Build the stat calibration pipeline/report so re-enable decisions are evidence-based.

Non-goals:
- No cap raises.
- No lowering arb/settlement/wx-stat edge thresholds.
- No live `stat` re-enable until post-cleanup shadow evidence is available.
- No fallback models for weather gaps; unsupported products stay skipped and surfaced.

## Current project facts

- The live process is the consolidated `predigy-engine`; legacy traders/import are disabled.
- `stat` reads DB `rules`, but all `stat` rules are currently disabled.
- Existing DB schema already has the raw ingredients for calibration: `model_p_snapshots`, `fills`, `positions`, `rules`, `settlements`, `book_snapshots`, and `calibration` (`migrations/0001_initial.sql`).
- `wx-stat` already logs prediction JSONL sidecars (`bin/wx-stat-curator/src/predictions.rs`) and has a manual fitter (`bin/wx-stat-fit-calibration`).
- The dashboard currently surfaces trade/eval metrics and fill latency, but not probability reliability/calibration.
- The current `post-cleanup-live-watch` loop watches operational safety/reconciliation, not calibration.

## External references relied on

- Kalshi `GET /markets` supports status filters, `series_ticker`, `event_ticker`, pagination cursor, and `limit` up to 1000; settled markets can be scanned with settled timestamp filters: https://docs.kalshi.com/api-reference/market/get-markets.md
- Kalshi orderbooks return YES bids and NO bids only; a YES bid at X is equivalent to a NO ask at `100-X`, which is the key complement logic for arb scanning: https://docs.kalshi.com/api-reference/market/get-market-orderbook.md
- Kalshi `GET /events` can return nested markets and exposes `mutually_exclusive`; this is useful for internal-arb discovery, but exhaustiveness must still be proven separately: https://docs.kalshi.com/api-reference/events/get-events.md
- Kalshi `GET /portfolio/settlements` returns member settlement history with `market_result`, counts, revenue, and `fee_cost`; useful for traded P&L reconciliation, but shadow calibration also needs public settled market outcomes: https://docs.kalshi.com/api-reference/portfolio/get-settlements.md
- Kalshi fee rounding docs explain trade fee/rounding/rebate mechanics and settlement rounding, so scanner/reports must use the project fee model and not naive edge math: https://docs.kalshi.com/getting_started/fee_rounding.md
- NOAA NBM is a calibrated blend of NWS and non-NWS model guidance, appropriate as the quantitative base for `wx-stat`: https://vlab.noaa.gov/web/mdl/nbm
- Iowa State IEM ASOS/AWOS/METAR archive provides airport observations, sourced from Unidata IDD/NCEI ISD/MADIS and synced near-real-time, but with limited QC; reports must treat observation joins as auditable inputs: https://mesonet.agron.iastate.edu/request/download.phtml?network=AWOS

## Files to modify / add

### Schema
- Add `migrations/0003_opportunity_observations_calibration_reports.sql`
  - `opportunity_observations`: append-only scanner observations.
  - `calibration_reports`: latest/archived reliability reports by strategy/window.
  - Indexes on `(strategy, ts DESC)`, `(strategy, opportunity_key, ts DESC)`, `(strategy, net_edge_cents DESC)`, and report `(strategy, window_end DESC)`.

### REST client
- Modify `crates/kalshi-rest/src/types.rs`
  - Add event response types for `/events?with_nested_markets=true`.
  - Add settlement response types for `/portfolio/settlements`.
  - Extend market outcome fields if needed (`result`, `settled_time`, scalar `value`).
- Modify `crates/kalshi-rest/src/client.rs`
  - Add paginated `list_events(...)`.
  - Add `portfolio_settlements(...)`.
  - Add a more general `list_markets_filtered(...)` or enough filters for settled outcome sync.
  - Optionally support `orderbook_snapshot_depth(ticker, depth)`; scanner should use shallow depth to limit REST load.

### Shared scanner/evaluator code
- Add `bin/opportunity-scanner/`
  - `Cargo.toml`
  - `src/main.rs`
  - `src/arb.rs`, `src/wx_stat.rs`, `src/settlement.rs`, `src/db.rs`, `src/report.rs`
- Modify root `Cargo.toml` workspace members/dependencies.
- Modify `crates/strategies/implication-arb/src/lib.rs`
  - Extract pure `ImplicationOpportunity` evaluator used by both strategy and scanner.
  - Add tests for greater-than, less-than, and range-derived implication chains.
- Modify `crates/strategies/internal-arb/src/lib.rs`
  - Extract pure `InternalArbOpportunity` evaluator.
  - Add explicit config/provenance fields for `exhaustive` and `proof`.
  - Do not auto-trade newly discovered families unless `exhaustive=true` and proof is present.
  - Consider adding the NO-basket mirror only behind explicit `directions` config and payoff-matrix tests.

### Arb universe curation
- Add `bin/arb-universe-curator/` or implement as `opportunity-scanner arb --write-candidates`.
- Candidate output paths:
  - `~/.config/predigy/implication-arb-config.candidates.json`
  - `~/.config/predigy/internal-arb-config.candidates.json`
- Live config writes require an explicit `--write-live-config` and should atomically back up current configs first.

### `wx-stat`
- Modify `bin/wx-stat-curator/src/kalshi_scan.rs`
  - Emit machine-readable coverage/skip counts: unmapped airport, unsupported strike, no quote, no NBM quantiles, already observed, edge too small.
- Modify `bin/wx-stat-curator/src/main.rs`
  - Add `--coverage-report-out` JSON.
  - Keep `--write` semantics unchanged for rules.
- Modify `bin/wx-stat-curator/src/airports.rs`
  - Add verified missing Kalshi daily temperature airport mappings discovered by scanner.
- Do not add snowfall/hurricane/hourly weather live paths until separate calibrated models exist.

### Settlement universe
- Modify `crates/strategies/settlement/src/lib.rs`
  - Add `PREDIGY_SETTLEMENT_SERIES` CSV override.
  - Add optional `PREDIGY_SETTLEMENT_SERIES_FILE` append/override.
  - Keep current `DEFAULT_SERIES` as fallback.
- Extend `opportunity-scanner settlement` to recommend series based on near-term markets, quotes, expected-expiration behavior, and observed book asymmetry.

### Calibration/surfacing
- Add `bin/predigy-calibration/`
  - Subcommands: `sync-settlements`, `report`, `shadow-stat` (or integrate shadow write into `stat-curator`).
- Modify `bin/stat-curator/src/main.rs`
  - Add DB shadow-write mode: insert/upsert markets, insert `model_p_snapshots`, upsert `rules` with `enabled=false` only.
  - Never write enabled `stat` rules without a separate explicit future approval step.
- Modify `bin/dashboard/src/main.rs`
  - Add `/calibration` page and `/calibration/summary.json`.
  - Add calibration freshness/status into `/api/state`.
- Add dashboard static assets under `bin/dashboard/static/`.
- Add deploy scripts/plists:
  - `deploy/scripts/opportunity-scanner-run.sh`
  - `deploy/scripts/predigy-calibration-run.sh`
  - `deploy/macos/com.predigy.opportunity-scanner.plist`
  - `deploy/macos/com.predigy.calibration.plist`
- Update `docs/ARCHITECTURE.md`, `docs/RUNBOOK.md`, and `docs/SESSIONS.md` after implementation.

## Data model details

### `opportunity_observations`

Append-only; scanner writes here only.

Suggested columns:
- `id BIGSERIAL PRIMARY KEY`
- `ts TIMESTAMPTZ NOT NULL DEFAULT now()`
- `strategy TEXT NOT NULL` (`implication-arb`, `internal-arb`, `wx-stat`, `settlement`, `stat-shadow`)
- `opportunity_key TEXT NOT NULL`
- `tickers TEXT[] NOT NULL`
- `kind TEXT NOT NULL` (`implication_pair`, `internal_yes_basket`, `internal_no_basket`, `settlement_book_asymmetry`, `wx_stat_rule_candidate`, etc.)
- `raw_edge_cents DOUBLE PRECISION`
- `net_edge_cents DOUBLE PRECISION`
- `max_size INTEGER`
- `would_fire BOOLEAN NOT NULL DEFAULT false`
- `reason TEXT`
- `payload JSONB NOT NULL DEFAULT '{}'::jsonb`

### `calibration_reports`

Suggested columns:
- `id BIGSERIAL PRIMARY KEY`
- `strategy TEXT NOT NULL`
- `window_start TIMESTAMPTZ NOT NULL`
- `window_end TIMESTAMPTZ NOT NULL`
- `n_predictions INTEGER NOT NULL`
- `n_settled INTEGER NOT NULL`
- `brier DOUBLE PRECISION`
- `log_loss DOUBLE PRECISION`
- `net_pnl_cents BIGINT`
- `baseline JSONB`
- `bins JSONB NOT NULL`
- `diagnosis JSONB NOT NULL`
- `created_at TIMESTAMPTZ NOT NULL DEFAULT now()`

## Implementation phases

### Phase 0 — Invariants and shared pure evaluators

1. Add pure evaluator structs/functions for implication and internal arb.
2. Preserve existing strategy behavior by having strategies call the extracted functions.
3. Add payoff-matrix tests:
   - Implication pair: child YES implies parent YES; verify all allowed settlement states are nonnegative after fees.
   - Internal YES basket: exactly-one-YES family only; reject merely `mutually_exclusive` without exhaustiveness proof.
   - Internal NO basket: only for exhaustive mutually exclusive families; verify payout `(n-1)*100` less cost/fees.
4. Add tests that current live configs parse.

Exit gate: `cargo test -p predigy-strategy-implication-arb -p predigy-strategy-internal-arb` passes.

### Phase 1 — Read-only opportunity scanner foundation

1. Add `opportunity_observations` migration.
2. Add `bin/opportunity-scanner` with subcommands:
   - `arb`: scan implication/internal candidates, fetch shallow orderbooks, compute edges.
   - `wx-stat`: run market coverage scan and summarize skipped/actionable markets.
   - `settlement`: scan near-settlement sports series and summarize candidate series/opportunities.
3. Scanner permissions:
   - Must not link to `Oms`.
   - Must not insert into `intents`, `intent_events`, `fills`, or `positions`.
   - DB writes limited to `opportunity_observations`.
4. Add an integration test or SQL guard test asserting scanner execution leaves `intents` unchanged.

Exit gate: scanner can run once in production and produce observations while `SELECT COUNT(*) FROM intents WHERE strategy='opportunity-scanner'` remains zero.

### Phase 2 — Arb universe expansion

1. Implication discovery:
   - Scan open markets grouped by event/series.
   - Build strict threshold chains using `strike_type`, `floor_strike`, and `cap_strike`.
   - Generate parent/child pairs for:
     - greater-than thresholds: higher threshold child -> lower threshold parent.
     - less-than thresholds: lower threshold child -> higher threshold parent.
     - range-derived implications only where payoff sets are provably subsets.
   - Store observations for every candidate, including non-fired edge history.
2. Internal family discovery:
   - Use `/events?with_nested_markets=true&status=open`.
   - Treat `mutually_exclusive=true` as necessary but not sufficient.
   - Require an `exhaustive` proof before writing live config candidates.
   - For ambiguous events, write observations with `would_fire=false` and `reason='not_exhaustive_proven'`.
3. Candidate promotion:
   - Write `*.candidates.json` with proof/provenance.
   - After at least one scanner pass confirms quotes and no proof warnings, promote to live config using atomic backup/write.
   - Existing strategies hot-reload config files; no cap changes.
4. Optional follow-up after YES basket is stable: enable internal NO-basket direction per family only with payoff tests and explicit config.

Exit gate: more configured pairs/families, but all generated from strict proofs; live fills, if any, still pass current OMS caps.

### Phase 3 — `wx-stat` coverage expansion

1. Add coverage report output to `wx-stat-curator`.
2. Use scanner to identify:
   - unmapped daily high/low airport codes,
   - markets skipped for unsupported strike type,
   - markets with no NBM quantiles,
   - markets with valid probabilities but insufficient edge.
3. Add only verified airport mappings first; this expands the existing calibrated mechanism without adding a new weather model.
4. Keep range/snow/hurricane/hourly products out of live rules until separate probability logic and calibration samples exist.
5. Surface calibration file freshness and bucket counts in the dashboard.

Exit gate: curator output has more eligible daily high/low markets or explicit proof that live Kalshi has no additional mapped daily temp markets.

### Phase 4 — Settlement universe expansion

1. Add `PREDIGY_SETTLEMENT_SERIES` and `PREDIGY_SETTLEMENT_SERIES_FILE` support.
2. Build settlement scanner recommendations by category/series:
   - near-term `expected_expiration_time` exists,
   - quote is present,
   - market is a game/outcome market, not an outright/tournament/season prop,
   - observed book asymmetry would have met current strategy gates.
3. Start with scanner-only observations for new series.
4. Promote the highest-count, cleanest series into env/file config; restart engine during a quiet window.
5. Keep fade disabled unless separately reviewed.

Exit gate: discovery logs show new settlement tickers subscribed; fills, if any, come from current strategy rules and caps.

### Phase 5 — Stat calibration evidence path

1. Settlement sync:
   - Backfill public settled market outcomes for tickers in `model_p_snapshots` and `rules`.
   - Use `GET /markets`/market detail for public outcomes; use `portfolio/settlements` for traded P&L/fees reconciliation.
2. Shadow predictions:
   - Modify `stat-curator` to keep generating probabilities post-cleanup but write them as disabled DB rules and `model_p_snapshots` only.
   - No enabled `stat` rules.
3. Report computation:
   - Join prediction-time probabilities to final outcomes.
   - Compute reliability bins, Brier score, log loss, empirical hit rate by bin, expected vs realized edge, and net P&L after fees/slippage for actual fills.
   - Compare against a market-price baseline where book snapshot or intent price is available.
4. Dashboard surfacing:
   - Show `stat`: disabled, collecting shadow evidence, `n_predictions`, `n_settled`, Brier/log loss, worst bin, stale settlement gaps.
   - Show `wx-stat`: prediction samples, fitted calibration file age/bucket count, recent realized reliability once settlements are available.
5. Re-enable criteria for a future separate approval:
   - `stat` remains disabled until a saved report shows adequate settled sample size, monotonic-ish reliability, Brier/log-loss better than market baseline, and positive net expectancy after fees/slippage.
   - Initial re-enable, if later approved, should be max size 1 and existing caps only.

Exit gate: dashboard and CLI answer “is stat calibrated?” without manual SQL.

### Phase 6 — Deployment / live test plan

1. Apply additive migrations.
2. Run unit + integration tests.
3. Deploy scanner/calibration launchd jobs in read-only/observation mode.
4. Let scanner collect for 12–24h.
5. Promote only strict arb candidates first; monitor fills and reconciliation.
6. Promote settlement series only after scanner confirms recurring near-settlement candidates.
7. Keep `stat` disabled; start shadow prediction collection and calibration reports.
8. Update docs and commit at end of implementation round.

## Tests

Minimum test commands after implementation:

```sh
cargo test -p predigy-kalshi-rest
cargo test -p predigy-strategy-implication-arb
cargo test -p predigy-strategy-internal-arb
cargo test -p predigy-strategy-settlement
cargo test -p wx-stat-curator
cargo test -p opportunity-scanner
cargo test -p predigy-calibration
cargo test -p dashboard
cargo test --workspace
```

Live/read-only checks:

```sh
# Scanner produces observations but no intents/orders.
cargo run -p opportunity-scanner -- arb --once --write-observations
psql -d predigy -c "SELECT strategy, COUNT(*) FROM opportunity_observations GROUP BY strategy;"
psql -d predigy -c "SELECT COUNT(*) FROM intents WHERE strategy='opportunity-scanner';"

# Calibration report is generated without enabling stat.
cargo run -p predigy-calibration -- report --strategy stat --window 30d
psql -d predigy -c "SELECT strategy, n_predictions, n_settled, brier, log_loss FROM calibration_reports ORDER BY created_at DESC LIMIT 10;"
psql -d predigy -c "SELECT enabled, COUNT(*) FROM rules WHERE strategy='stat' GROUP BY enabled;"
```

Operational checks after promotion:

```sh
launchctl list 2>/dev/null | grep predigy
psql -d predigy -c "SELECT strategy, status, COUNT(*) FROM intents GROUP BY strategy, status ORDER BY strategy, status;"
psql -d predigy -c "SELECT strategy, COUNT(*), SUM(qty), SUM(fee_cents) FROM fills WHERE ts > now() - interval '24 hours' GROUP BY strategy;"
tail -n 100 ~/Library/Logs/predigy/engine.stderr.log
```

## Risks and mitigations

- **False implication pair**: require structured strike proof and payoff tests; ambiguous pairs stay scanner-only.
- **Internal family not exhaustive**: `mutually_exclusive` is not enough. Require explicit `exhaustive=true` proof before live config.
- **REST rate limits**: use pagination, shallow orderbook depth, backoff already in `kalshi-rest`, and scanner cadence limits.
- **Observation DB growth**: append-only with indexes; add retention/aggregation once volume is known.
- **Calibration selection bias**: use shadow predictions, not only traded fills; report traded P&L separately.
- **Weather observation mismatch**: record station/date/threshold in prediction records and cross-check against Kalshi settlement where available.
- **More subscriptions burden WS/router**: promote in batches; settlement series expansion via env/file rollback.
- **Partial fill arb risk**: keep IOC leg groups and OMS caps unchanged; do not increase size/caps in this plan.

## Rollback

- Stop scanner/calibration launchd jobs; they do not affect live orders.
- Restore previous arb config files from automatic backups.
- Clear settlement series env/file override and restart `predigy-engine`.
- Restore prior `wx-stat` rule/calibration files if coverage changes misbehave.
- Keep `stat` disabled by ensuring `rules.enabled=false` for `strategy='stat'` and/or arming `kill_switches.scope='stat'`.
- Additive DB tables can remain unused; no live strategy depends on them for order submission.

## Calibration evidence answer

Calibration evidence comes from:
1. **Prediction at decision time**: `model_p_snapshots` and `wx-stat` prediction JSONL sidecars store raw/model probabilities and provenance.
2. **Realized outcomes**: public Kalshi settled market results for binary/scalar outcomes, plus weather ASOS observations for `wx-stat` model fitting.
3. **Execution/P&L**: `intents`, `fills`, `positions`, fees, and portfolio settlements show whether traded edges survived fees/slippage.
4. **Reports**: reliability bins, Brier score, log loss, hit rate by probability bin, expected-vs-realized edge, and net P&L.

Today, this is **not yet watched/surfaced as a dedicated calibration product**. Operational monitoring is active, and dashboard eval endpoints exist, but they do not answer whether `stat` probabilities are reliable. `wx-stat-fit-calibration` exists as an offline/manual fitter, and `wx-stat-curator` logs prediction records, but there is no recurring calibration report or dashboard card yet. This plan adds that missing watcher/surface.