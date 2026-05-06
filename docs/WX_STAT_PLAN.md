# wx-stat — forecast-driven calibration arbitrage on Kalshi weather markets

## Edge thesis

Kalshi temperature markets (`KXHIGHDEN-26MAY07-T80`, etc.) settle on a
single observed value at a specific airport. The crowd prices them
roughly off the public NWS daily forecast plus narrative. That public
NWS forecast is itself a *deterministic* product — a single number per
hour. The hourly point forecast hides two things the trader can
exploit:

1. **The full distribution.** NWS publishes a probabilistic gridded
   product (the National Blend of Models, NBM) that gives mean +
   spread + percentile bands. The hourly point forecast collapses
   this to a single value. Kalshi's market price reflects the public
   point forecast, not the calibrated distribution.
2. **The forecast-vs-outcome calibration error.** Public-forecast
   miss bias is small but persistent — NWS hourly forecasts run
   roughly +/-1F off the eventual observed max in the 12–24h window,
   with city-specific bias. The crowd doesn't apply that bias.

Both produce a calibrated `model_p` that differs from the market's
implied probability. The difference is the trade.

This is the same pattern as `stat-curator`/`stat-trader` (LLM-derived
calibrated probabilities → StatRule). The difference: forecasts are
quantitative, deterministic, and free, so we don't need an LLM in the
hot path. `wx-stat-curator` is `stat-curator` with the LLM replaced
by an NWS-fetch + math layer.

## Architecture

```
NWS API ──┐
          │
ext-feeds/nws_forecast.rs        ──── hourly point forecast
                                      (Phase 1)

ext-feeds/nws_nbm.rs (Phase 2)   ──── probabilistic gridded
                                      (mean + percentiles)

bin/wx-stat-curator              ──── for each Kalshi weather market:
  - kalshi_scan (reuse pattern        1. parse ticker → airport, threshold,
    from wx-curator)                     valid window
  - ticker_parse                      2. lookup airport → grid point
  - airports                          3. fetch forecast for grid point
  - forecast_to_p                     4. compute model_p (P(temp > thresh))
  - main                              5. compare to market yes_ask
                                      6. emit StatRule[] to JSON

stat-trader (existing)           ──── reads StatRule[] file,
                                      executes Kelly-sized bets when
                                      market price diverges from model_p

launchd plist                    ──── re-runs curator every 1–3h to
                                      pull fresh forecasts as they
                                      update
```

**Re-uses**:
- Kalshi scan pattern — copy from `bin/wx-curator/src/kalshi_scan.rs`
- StatRule output format — already consumed by stat-trader
- launchd plist pattern — copy from `deploy/macos/com.predigy.stat-curate.plist`
- ext-feeds error type — `crates/ext-feeds/src/error.rs`

**New**:
- `crates/ext-feeds/src/nws_forecast.rs` — point-forecast fetcher
- `bin/wx-stat-curator/` — the curator binary (5 modules)
- `deploy/macos/com.predigy.wx-stat-curate.plist` — schedule (Phase 3)
- Static airport→(lat,lon) map (~30 entries — DEN, LAX, NYC etc that
  Kalshi covers)

## Phasing

### Phase 1 — Deterministic point forecast (this round)

Goal: end-to-end pipeline working with deterministic point forecast.
`model_p` is binary 0/1 based on whether the forecast max over the
market's valid window crosses the threshold. NOT yet calibrated, NOT
yet probabilistic. This is enough to validate the plumbing.

**Deliverables**:
1. `ext-feeds/nws_forecast.rs` — HTTP client (User-Agent required),
   `/points/{lat,lon}` lookup, `/gridpoints/.../forecast/hourly`
   fetcher, parser. Tests against the live API for one location.
2. Static airport map (~30 entries covering Kalshi's actual market
   set: DEN, LAX, NYC, MIA, AUS, CHI, etc.).
3. Ticker parser: `KXHIGHDEN-26MAY07-T80` → (DEN, "high", 80F,
   2026-05-07).
4. `wx-stat-curator` binary: scan → parse → fetch → derive
   `model_p` → emit StatRule[].
5. Smoke test against live Kalshi + live NWS for at least one market.

**Phase 1 explicitly does NOT do**:
- NBM probabilistic forecast (Phase 2).
- Calibration vs historical observations (Phase 2).
- Confidence sizing — model_p stays binary 0/1, edge thresholds
  guard sizing (Phase 1 just acts as a "is the market wildly wrong"
  filter).
- Live deployment — dry-run only until Phase 3.

### Phase 2 — Probabilistic forecast + calibration

NBM gridded data gives `P(temp_max > threshold)` directly per grid
point per forecast hour. **Detailed plan with reconnaissance findings
in [WX_STAT_NBM_PHASE2.md](WX_STAT_NBM_PHASE2.md).** Summary:

2a. **NBM ingest.** Public S3 bucket `noaa-nbm-grib2-pds` (no auth);
HTTP range requests work; `.grib2.idx` sidecars give per-message
byte offsets. The `qmd/` subtree carries probabilistic quantile
data (21 levels: 0%, 5%, …, 100%) at 2m above ground.

2b. **GRIB2 decode.** Use the `grib` crate (0.15.x, pure Rust + C
deps for JPEG2000 compression).

2c. **Airport → grid sampling + cache.** Per-airport quantile vector
cached under `data/nbm_cache/`; ~84 bytes per (airport × fcst hour),
trivial size.

2d. **CDF interpolation → model_p.** Bracketing two adjacent
quantile values for the Kalshi threshold lets us read
`P(T ≤ threshold)` directly. Drops the Phase 1 conviction-zone
gate entirely.

2e. **Calibration.** Track forecast-vs-outcome over a rolling
window. If NBM systematically overstates `P(>80F)` at LAX in May,
shift accordingly via Platt scaling. Persisted per
(airport, threshold-band, lead-time) bucket.

### Phase 3 — Production deploy

- launchd plist re-running curator every 1–3h.
- Confidence-aware Kelly sizing in stat-trader (or in StatRule
  itself by computing `min_edge_cents` higher when forecast spread
  is wide).
- Honest performance log per market.

## Risk register

- **R1: NWS API throttling.** NWS asks for User-Agent + reasonable
  rates. Curator runs every 1–3h × ~200 markets is fine. If we
  later run real-time, switch to NDFD GRIB pull (single bulk file)
  rather than per-station hits.
- **R2: Ticker shape changes.** Kalshi could change the
  `KXHIGHDEN-26MAY07-T80` shape. Parser must fail loudly, not
  fall through to a default.
- **R3: Airport map coverage gap.** Some Kalshi markets reference
  airports we don't have lat/lon for. Mitigation: fail-loud at
  parse — emit no StatRule for unmapped airport, log warning.
- **R4: Phase 1 binary `model_p` is too crude.** A 0/1 probability
  bet is fine when forecast says +95F and market says 30¢ for
  "high > 80F" — clear edge. But near the threshold it's wrong:
  forecast says 81F, market says 50¢, model_p=1 implies an enormous
  bet. Mitigation in Phase 1: only emit StatRule when forecast
  margin to threshold is ≥ 5°F. Markets within 5°F of the forecast
  point are skipped until Phase 2 calibration arrives.
- **R5: Settlement timing.** Some Kalshi temp markets settle on
  midnight local; some on observed daily max from a specific data
  feed (NWS, NOAA AWOS). The market description specifies the
  source. Parser must resolve this; getting it wrong means model_p
  is computed against the wrong reference.

## Out of scope for this plan

- Hurricane / severe weather markets (different probabilistic
  source — NHC tropical products).
- Snowfall markets (different gridded product — NDFD QPF + winter
  weather).
- Wind / precipitation markets (different products entirely).

These can be added later as separate model_p computers; the
StatRule scaffold supports them with no changes.
