# wx-stat Phase 2 — NBM probabilistic temperature

Phase 2 replaces the Phase 1 deterministic point forecast with NOAA's
**National Blend of Models** (NBM) probabilistic data, which gives a
full forecast distribution per grid point per forecast hour. With
that, the conviction-zone gate (Phase 1's blunt 5°F margin filter)
goes away and `model_p` becomes a real calibrated probability that
maps directly onto Kalshi's quoted price.

This doc records the data-source reconnaissance done 2026-05-06 and
specifies the implementation plan. Phase 1 (deterministic, conviction-
zone-gated) is shipped and inspection-ready; Phase 2 is the real
edge.

## Data source: NOAA NBM on AWS S3

**Bucket** (public, no auth): `s3://noaa-nbm-grib2-pds/`  
**HTTP**: `https://noaa-nbm-grib2-pds.s3.amazonaws.com/`

### Layout

```
blend.YYYYMMDD/CC/<product>/blend.tCCz.<product>.fHHH.<region>.grib2
                                                                      ^-- one .grib2 per (cycle × fcst-hour × region × product)
```

- `YYYYMMDD` — model run date (UTC)
- `CC` — model cycle hour (00–23 UTC). NBM runs hourly.
- `<product>`:
  - `core/` — the deterministic blend (used by NWS hourly forecast)
  - **`qmd/` — quantile/probabilistic model data** ← *Phase 2 needs this*
  - `text/` — human-readable model text products (irrelevant to us)
- `fHHH` — forecast hour offset, zero-padded (`f001` … `f168`+)
- `<region>`:
  - `co` — CONUS ← *the only region we care about for Kalshi US-city markets*
  - `ak` — Alaska, `gu` — Guam, `pr` — Puerto Rico, `hi` — Hawaii

### Sidecar `.grib2.idx` files

Every `.grib2` has a paired `.grib2.idx` listing every message:

```
1:0:d=2026050612:APTMP:2 m above ground:24 hour fcst:
2:2190106:d=2026050612:CDCB:reserved:24 hour fcst:
...
259:470367052:d=2026050612:TMP:2 m above ground:24 hour fcst:80% level
```

Format: `<msg_num>:<byte_offset>:d=<cycle>:<param>:<level>:<fcst_label>:[<extra>]`

The byte offset is the start of message `N`; message `N`'s length is
`offset[N+1] - offset[N]`. With both offsets we can issue an HTTP
`Range: bytes=offset[N]-offset[N+1]-1` and pull just that one message
without downloading the 600MB file.

**Validated**: range requests return `206 Partial Content` against
the bucket. Confirmed 2026-05-06 against
`blend.20260506/12/qmd/blend.t12z.qmd.f024.co.grib2`
(total size 601,478,156 bytes).

## What's in the `qmd` file (per forecast hour)

For each of TMP / APTMP / DPT / WIND / GUST / etc. at 2m above
ground, the qmd contains:

### Threshold probabilities

Fixed thresholds, returned as `prob > X` or `prob < X` fields:

```
prob <255.372  (=  0°F)
prob <270.928  (= 28°F)
prob <273.15   (= 32°F = freezing)
prob >299.817  (= 80°F)
prob >305.372  (= 90°F)
prob >310.928  (= 100°F)
prob >316.483  (= 110°F)
prob >322.039  (= 120°F)
```

These are direct `P(T_2m > X)` fields. **But the threshold spacing
(~10°F) is too coarse for Kalshi's integer-Fahrenheit markets**
(`KXHIGHDEN-26MAY07-T68`, `KXHIGHDEN-26MAY07-T75`, etc., spaced
1–3°F apart). So we don't use these directly — we use the
quantiles below.

### Quantile levels (the goldmine)

```
TMP:2 m above ground:24 hour fcst:0% level    ← min over the ensemble
TMP:2 m above ground:24 hour fcst:5% level
TMP:2 m above ground:24 hour fcst:10% level
...
TMP:2 m above ground:24 hour fcst:50% level   ← median
...
TMP:2 m above ground:24 hour fcst:95% level
TMP:2 m above ground:24 hour fcst:100% level  ← max
```

**21 quantile levels in 5% steps**. Each is a 2D field over CONUS
giving the temperature value such that `P(T_2m ≤ value) = quantile`.

Inverting: given a Kalshi threshold `X` (Fahrenheit → Kelvin),
look up the two adjacent quantiles whose temperatures bracket `X`,
linearly interpolate the quantile, and that interpolated quantile
IS our `model_p` (with the right sign for `>X` vs `<X` markets).

Worked example: Kalshi market **`KXHIGHDEN-26MAY07-T80`** — *will
the high in Denver be > 80°F on 7 May 2026?* Suppose at the
appropriate forecast hour the Denver grid cell shows:

```
50% level   = 75°F
70% level   = 79°F
75% level   = 80.5°F
80% level   = 82°F
```

The 80°F threshold falls between 70% (79°F) and 75% (80.5°F) so we
interpolate: `P(T ≤ 80) ≈ 0.70 + (0.75 − 0.70) × (80 − 79) / (80.5 − 79) = 0.733`.
Therefore `model_p = P(T > 80) = 1 − 0.733 = 0.267`.

That's a calibrated 27% probability YES — vs Phase 1's binary
0.97/0.03 conviction-zone gate. If the Kalshi YES is being asked
at 50¢, the model says we should bet NO at 50¢ (no edge), but
maybe the bid is 35¢ — meaning NO ask = 65¢; with model implying
NO at 73%, edge is 73−65 = 8¢. Now stat-trader can size with
real Kelly fractions instead of conviction-zone all-or-nothing.

## Implementation plan

### Sub-phase 2A: NBM ingest in `ext-feeds`

New module `crates/ext-feeds/src/nbm.rs`:

- `pub struct NbmClient { http: reqwest::Client, bucket_base: String }`
- `pub struct NbmCycle { date: NaiveDate, hour: u8 }`
- `async fn fetch_index(cycle, fcst_hour, region, product) -> Vec<IdxEntry>`
- `async fn fetch_message_bytes(cycle, fcst_hour, region, product, msg_num: usize) -> Vec<u8>`
- `pub fn locate_quantile_msgs(idx: &[IdxEntry], param: &str, level: &str) -> Vec<(u8, MessageRange)>`
  where `param = "TMP"`, `level = "2 m above ground"`, returns
  `[(quantile_pct, byte_range), ...]` for all 21 quantile messages.

The `.grib2.idx` parser is line-based, ~50 lines of code. The HTTP
range request is `reqwest::get(url).header("Range", format!("bytes={s}-{e}"))`.

**Tests**: unit-test the idx parser; an integration test that pulls
a real (small) .grib2.idx and asserts known message names/offsets.

### Sub-phase 2B: GRIB2 decode

Use the `grib` crate (0.15.x). Pure Rust with optional C deps for
JPEG2000 unpacking — NBM messages typically use JPEG2000 compression
so `openjpeg-sys` is required at build time (Homebrew: `brew install
openjpeg`).

`crates/ext-feeds/src/nbm_decode.rs`:

- `pub fn decode_message(bytes: &[u8]) -> Result<NbmField, Error>`
  where `NbmField` carries the f32 grid + the grid lat/lon mapping.
- The NBM CONUS grid is Lambert Conformal Conic; we'll let the
  `grib` crate's `LatLons` iterator compute the lat/lon per cell
  rather than rolling our own projection math.

### Sub-phase 2C: Airport → grid extraction + cache

`crates/ext-feeds/src/nbm_extract.rs`:

- `pub fn nearest_grid_cell(field: &NbmField, lat: f64, lon: f64) -> (usize, usize)`
- `pub fn sample_quantiles_at_airport(cycle, fcst_hour, lat, lon) -> Result<[f32; 21], Error>`

Per-airport caching: once we've extracted the 21 quantile temps for
(cycle, fcst_hour, lat, lon), persist as JSON under
`data/nbm_cache/{cycle}/{fcst_hour}/{airport_code}.json`. The next
market query against that cell is a file read, not a re-fetch +
re-decode of the GRIB.

Cache size: 21 × f32 = 84 bytes per (airport × forecast hour).
~30 airports × 168 forecast hours × 4 cycles per day = ~1.7MB/day.
Tiny. Can keep ~30 days of history without rotation.

### Sub-phase 2D: Wire into `wx-stat-curator`

Replace `forecast_to_p::derive_model_p` (Phase 1 deterministic) with
a Phase 2 path:

- Identify the appropriate cycle (most-recent 06Z or 18Z run usually
  contains the day-ahead forecast horizon Kalshi is pricing).
- Identify the forecast hour matching the market's settlement window
  (daily-high markets: take the max over forecast hours that fall in
  the settlement local-day; daily-low markets: take the min).
- Fetch quantile temps at the airport's grid cell.
- Linearly interpolate the CDF at the market threshold → model_p.
- Drop the conviction-zone gate entirely.

Phase 1 stays in tree as a fallback for cases where NBM is
unavailable (network failure, bucket outage). Phase 2 promotes when
both the NBM data and the deterministic forecast agree on direction;
disagrees → log loudly + skip the rule (don't trust either source
alone if they conflict).

### Sub-phase 2E: Calibration

NBM's quantiles are **model-derived**, not history-calibrated. They
encode the ensemble spread but not systematic bias — if NBM
historically over-estimates `P(T > 90F)` at LAX in May, the raw
model_p is wrong by exactly that bias.

Phase 2E adds a Platt-scaling layer per (airport × month-of-year ×
threshold-band). Train on NBM forecasts vs realized observations
over a rolling window. Apply at inference. This is a real ML chunk
— probably 1-2 days of work after 2A-D are stable.

## Cost / time budget

- 2A (ingest):     ~0.5 day — straightforward HTTP + parsing
- 2B (decode):     ~1 day — grib crate integration + openjpeg setup
- 2C (extract):    ~0.5 day — grid lookup + caching
- 2D (wire):       ~0.5 day — replace derive_model_p
- 2E (calibrate):  ~2 days — needs historical NBM + observed-temp data

Total 2A–2D: ~2.5 days for a working calibrated probability. 2E is
the right edge but optional for first deploy — uncalibrated NBM
quantiles are still a strict improvement over the conviction-zone
gate.

## Out of scope (Phase 3)

- 2-D and 3-D moisture / dewpoint / wind quantiles (different
  Kalshi markets, different lookup logic)
- Hurricane-track NHC tropical products (KXHURNJ etc.)
- Snowfall NDFD QPF (KXSNOWS etc.)
- Multi-cycle blending (combining 06Z + 18Z forecasts)
- Live NBM-update detection (re-pull when a new cycle becomes
  available rather than scheduling against the launchd 3h cadence)

## Reproduction

The reconnaissance commands used to validate this:

```bash
# 1. Layout
curl -s "https://noaa-nbm-grib2-pds.s3.amazonaws.com/?list-type=2&prefix=blend.YYYYMMDD/CC/&delimiter=/&max-keys=20"

# 2. List qmd cycle files
curl -s "https://noaa-nbm-grib2-pds.s3.amazonaws.com/?list-type=2&prefix=blend.20260506/12/qmd/&max-keys=10"

# 3. Index for forecast-hour 24
curl -sL "https://noaa-nbm-grib2-pds.s3.amazonaws.com/blend.20260506/12/qmd/blend.t12z.qmd.f024.co.grib2.idx"

# 4. Verify range requests
curl -sI -H "Range: bytes=0-1023" "https://noaa-nbm-grib2-pds.s3.amazonaws.com/blend.20260506/12/qmd/blend.t12z.qmd.f024.co.grib2"
# → 206 Partial Content
```
