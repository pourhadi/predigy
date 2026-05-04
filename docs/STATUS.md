# Predigy Build Status

> Living snapshot of what's implemented vs. what's planned. Update with every
> phase commit. The architectural plan is in [`PLAN.md`](./PLAN.md).

## Current configuration

| Setting | Value |
|---|---|
| Capital | $5,000 |
| Infra budget | ~$80-150/mo |
| Hosting target | Single Chicago VPS (TBD; QuantVPS / NYC Servers / BuyVM Chicago) |
| Active strategies (when live) | Cross-venue arb (primary), intra-venue static arb, stat-arb on long-tail |
| Deferred until ≥$25k | Market making + Kalshi MM designation |
| Deferred until ≥$50k | AWS Tier B, paid sports/news feeds |
| Branch | `claude/hft-prediction-market-system-s0rcz` |
| Rust toolchain | stable, edition 2024 |

## Phase status

| Phase | Description | Status |
|---|---|---|
| 0 | Plumbing (workspace, core types, fees, lints) | ✅ Done |
| 1 | Read-only stack: `kalshi-rest`, `book`, `kalshi-md` (WS), `md-recorder`, `poly-md` | 🟡 In progress (REST + book done; WS, recorder, poly-md pending) |
| 2 | OMS + risk + FIX exec + first live strategy (intra-venue arb) | ⬜ Not started |
| 3 | Backtester + sim | ⬜ Not started |
| 4 | Market making + rebate capture | ⬜ Deferred (≥$25k) |
| 5 | Cross-venue signal arb (primary engine) | ⬜ Not started |
| 6 | News/data latency (free feeds first) | ⬜ Not started |
| 7 | Statistical / model alpha | ⬜ Not started |
| 8 | Hardening & scaling | ⬜ Ongoing |

## What's in the repo right now

```
predigy/
├── Cargo.toml                         workspace manifest, lints, profiles
├── rust-toolchain.toml                pins stable, components: rustfmt, clippy
├── .gitignore                         ignores target/, secrets, .env
├── README.md                          quick start + status
├── docs/
│   ├── PLAN.md                        full architecture / strategy / infra plan
│   └── STATUS.md                      this file
└── crates/
    ├── core/                          ✅ Phase 0
    │   └── src/
    │       ├── lib.rs                 module roots + re-exports
    │       ├── price.rs               Price (cents 1..=99), Qty (non-zero u32)
    │       ├── side.rs                Side (Yes/No), Action (Buy/Sell)
    │       ├── market.rs              MarketTicker, Market, MarketStatus
    │       ├── order.rs               Order, OrderId, OrderType, TimeInForce, OrderState
    │       ├── fill.rs                Fill (with maker flag, fee_cents)
    │       ├── position.rs            Position with unrealized PnL
    │       └── fees.rs                Kalshi Feb-2026 fee formula (integer cents)
    ├── book/                          ✅ Phase 1 part 1
    │   └── src/lib.rs                 OrderBook, Snapshot, Delta, ApplyOutcome
    │                                  - apply_snapshot / apply_delta
    │                                  - sequence-gap detection (last_seq preserved on gap)
    │                                  - best YES bid/ask/no-bid, mid, spread
    │                                  - YES asks derived from NO bids by complement
    └── kalshi-rest/                   ✅ Phase 1 part 1
        ├── src/
        │   ├── lib.rs                 module roots + re-exports
        │   ├── auth.rs                Signer (RSA-PSS-SHA256, PKCS#1 or PKCS#8 PEM)
        │   ├── client.rs              Client (auth-optional, reqwest, rustls-tls)
        │   ├── error.rs               Error enum (Http, Api, Auth, Decode, Url)
        │   └── types.rs               JSON response types (decimal price schema)
        └── examples/smoke.rs          live read-only smoke test
```

## Test counts

```
predigy-core       15 tests   (price 4, side 1, position 2, fees 8)
predigy-book        6 tests   (snapshot/delta/gap/wrong-market/edge cases)
predigy-kalshi-rest 6 tests   (auth round-trip, PSS non-determinism, bad PEM,
                               url builder, public client, auth required)
                   ─────────
                   27 tests
```

CI gates (all currently passing locally):
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
  (clippy::pedantic with sensible allows: `module_name_repetitions`,
  `must_use_candidate`, `missing_errors_doc`, `missing_panics_doc`,
  `cast_possible_truncation`, `cast_sign_loss`)
- `cargo test --workspace --locked`

## Confirmed external API contracts

### Kalshi REST
- Base URL: `https://api.elections.kalshi.com/trade-api/v2`
- Auth headers (PSS-SHA256 over `timestamp_ms + method + path`):
  - `KALSHI-ACCESS-KEY`
  - `KALSHI-ACCESS-TIMESTAMP`
  - `KALSHI-ACCESS-SIGNATURE` (base64)
- Path = full URL path from API root, no query string.
- Post-Mar-2026 schema: prices are decimals (`yes_price_dollars`); legacy
  integer-cent fields removed.
- Orderbook returns `yes_bids` + `no_bids` only — no asks. YES asks =
  complement of NO bids.

### Kalshi WebSocket (not yet implemented)
- URL: `wss://api.elections.kalshi.com/trade-api/ws/v2`
- Same auth headers as REST on the upgrade request.
- Subscriptions: `orderbook_delta`, `ticker`, `trade`, `fill` (auth).
- Snapshots include a sequence number; deltas must be applied in strict order.
- Repeated subscriptions on the same connection are now no-ops (was an error
  pre-2026).

### Kalshi FIX 4.4 (not yet implemented)
- Request access via `[email protected]`.
- Used for order entry/cancel/amend in Phase 2.

### Polymarket WS (not yet implemented)
- URL: `wss://ws-subscriptions-clob.polymarket.com/ws/market`
- No auth needed for the public market channel (book + price).
- Used as a reference price; we never execute on Polymarket.

## Known limitations / open items

- **CI workflow**: `.github/workflows/ci.yml` is **not committed** because
  the dev environment's git proxy and GitHub MCP both lack `workflows`
  permission scope. Add manually via the GitHub UI; canonical content is in
  the second commit message and reproduced in the README.
- **TLS in this sandbox**: outbound TLS to `api.elections.kalshi.com` failed
  with `InvalidCertificate(UnknownIssuer)` — the sandbox proxy doesn't trust
  Kalshi's CA. The code uses `rustls-tls` with `webpki-roots` and works on
  any real network; verify by running `cargo run -p predigy-kalshi-rest
  --example smoke` from your laptop or the production VPS.
- **No live API key tested yet** — `Signer` is unit-test-verified
  (round-trip with the public key) but has not signed a real Kalshi request
  end-to-end.
- **Bare-metal Chicago VPS not yet ordered** — that's a manual vendor
  process; not blocked on code.

## Next chunk to build

Still inside Phase 1:

1. `crates/kalshi-md/` — WebSocket client
   - tokio-tungstenite + rustls-tls; auth headers on upgrade
   - decode `orderbook_snapshot`, `orderbook_delta`, `ticker`, `trade`
   - feed snapshots/deltas into `book::OrderBook`; on `Gap`, fetch a fresh
     REST snapshot and resync
   - reconnect with exponential backoff + jitter
2. `crates/poly-md/` — Polymarket WS client (reference book, no exec)
3. `bin/md-recorder/` — long-running binary that subscribes to a configured
   list of markets and writes raw JSON events to disk (NDJSON in Phase 1,
   parquet rotation in Phase 3)
4. Integration test: feed a captured WS log into a fresh `OrderBook`,
   assert it reconstructs identically to a final REST snapshot

After that, Phase 1 acceptance: 24h of recorded data, replay-vs-snapshot
identical book.

## Build / run

```bash
# All tests
cargo test --workspace

# Lints (CI parity)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Live read-only smoke test (needs network with public CA roots)
cargo run -p predigy-kalshi-rest --example smoke
```

## Recent commits

| SHA | Subject |
|---|---|
| `9884459` | Add `predigy-book` and `predigy-kalshi-rest` crates (Phase 1, part 1) |
| `1eafd3f` | Scaffold Cargo workspace and `predigy-core` crate (Phase 0) |
| `b15fc05` | Initial commit (README only) |
