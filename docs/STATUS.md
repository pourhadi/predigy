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
| 1 | Read-only stack: `kalshi-rest`, `book`, `kalshi-md` (WS), `md-recorder`, `poly-md` | ✅ Done (logic). Live shake-down on a real Kalshi key still open. |
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
├── .github/workflows/ci.yml           fmt + clippy + test on push/PR
├── docs/
│   ├── PLAN.md                        full architecture / strategy / infra plan
│   └── STATUS.md                      this file
├── crates/
│   ├── core/                          ✅ Phase 0
│   │   └── src/
│   │       ├── lib.rs                 module roots + re-exports
│   │       ├── price.rs               Price (cents 1..=99), Qty (non-zero u32)
│   │       ├── side.rs                Side (Yes/No), Action (Buy/Sell)
│   │       ├── market.rs              MarketTicker, Market, MarketStatus
│   │       ├── order.rs               Order, OrderId, OrderType, TimeInForce, OrderState
│   │       ├── fill.rs                Fill (with maker flag, fee_cents)
│   │       ├── position.rs            Position with unrealized PnL
│   │       └── fees.rs                Kalshi Feb-2026 fee formula (integer cents)
│   ├── book/                          ✅ Phase 1 part 1
│   │   └── src/lib.rs                 OrderBook, Snapshot, Delta, ApplyOutcome
│   │                                  - apply_snapshot / apply_delta
│   │                                  - sequence-gap detection (last_seq preserved on gap)
│   │                                  - best YES bid/ask/no-bid, mid, spread
│   │                                  - YES asks derived from NO bids by complement
│   ├── kalshi-rest/                   ✅ Phase 1 part 1
│   │   ├── src/
│   │   │   ├── lib.rs                 module roots + re-exports
│   │   │   ├── auth.rs                Signer (RSA-PSS-SHA256, PKCS#1 or PKCS#8 PEM)
│   │   │   ├── client.rs              Client (auth-optional, reqwest, rustls-tls)
│   │   │   ├── error.rs               Error enum (Http, Api, Auth, Decode, Url)
│   │   │   └── types.rs               JSON response types (decimal price schema)
│   │   └── examples/smoke.rs          live read-only smoke test
│   ├── kalshi-md/                     ✅ Phase 1 part 2
│   │   ├── src/
│   │   │   ├── lib.rs                 module roots + re-exports + quick-start
│   │   │   ├── messages.rs            wire types: Outgoing (Subscribe/Unsubscribe/
│   │   │   │                          UpdateSubscription) + Incoming envelope
│   │   │   │                          (snapshot/delta/ticker/trade/subscribed/error/ok)
│   │   │   ├── decode.rs              "0.0800"→Price, "300.00"→u32, "-54.00"→i32;
│   │   │   │                          wire snapshot/delta → predigy_book::{Snapshot,Delta}
│   │   │   ├── backoff.rs             exp backoff w/ full jitter (Brooker 2015)
│   │   │   ├── client.rs              Client + Connection: auth on upgrade, command
│   │   │   │                          and event channels, single-task multiplexer,
│   │   │   │                          reconnect with replay of saved subscriptions
│   │   │   └── error.rs               Error enum (WebSocket, Upgrade, Server, Decode,
│   │   │                              OutOfRange, Closed, Invalid, Url)
│   │   └── tests/
│   │       ├── loopback_session.rs    end-to-end: subscribe → snapshot → delta →
│   │       │                          ticker → trade against an in-process mock
│   │       └── reconnect_replay.rs    server drops, client reconnects, replays the
│   │                                  saved sub with the original req_id
│   └── poly-md/                       ✅ Phase 1 part 3
│       ├── src/
│       │   ├── lib.rs                 module roots + re-exports + quick-start
│       │   ├── messages.rs            wire types: Subscribe (assets_ids/type/
│       │   │                          custom_feature_enabled) + Incoming
│       │   │                          tagged on `event_type` (book / price_change /
│       │   │                          last_trade_price / tick_size_change / Other)
│       │   ├── decode.rs              parse_price (string → f64 ∈ [0,1]) and
│       │   │                          parse_size (non-negative)
│       │   ├── backoff.rs             same algorithm as kalshi-md (duplicated,
│       │   │                          ~80 lines, no shared crate yet)
│       │   ├── client.rs              Client + Connection: no auth, single-payload
│       │   │                          subscribe, BTreeSet of saved asset_ids,
│       │   │                          reconnect with consolidated re-subscribe.
│       │   │                          Handles both `{...}` and `[{...},{...}]`
│       │   │                          framing (Polymarket batches multiple events).
│       │   └── error.rs               Error enum
│       └── tests/
│           ├── loopback_session.rs    end-to-end against an in-process mock; covers
│           │                          single-frame events and JSON-array batching
│           └── reconnect_replay.rs    server drops, client adds a second asset
│                                      during backoff, reconnect sends the union
└── bin/                               ✅ Phase 1 part 4
    └── md-recorder/
        ├── src/
        │   ├── lib.rs                 module roots + re-exports
        │   ├── recorded.rs            on-disk NDJSON schema (versioned), with a
        │   │                          synthetic RestResync event the recorder
        │   │                          injects after a Gap-triggered REST fetch
        │   ├── recorder.rs            Recorder<P: SnapshotProvider> — drains the
        │   │                          kalshi-md Connection, writes one NDJSON
        │   │                          line per event, applies snapshot/delta to
        │   │                          a per-market OrderBook, on Gap pulls a
        │   │                          fresh snapshot via P and emits RestResync,
        │   │                          on Reconnected forces a resync per market
        │   └── main.rs                CLI (clap): --output, --market…,
        │                              --kalshi-key-id, --kalshi-pem; SIGINT for
        │                              graceful shutdown; tracing-subscriber logs
        └── tests/
            └── replay_vs_recorder.rs  Phase 1 acceptance: drive recorder
                                       through subscribe→snapshot→delta→
                                       gap-induced resync; replay the NDJSON;
                                       assert replayed book ≡ recorder's
                                       in-memory book
```

## Test counts

```
predigy-core       15 tests   (price 4, side 1, position 2, fees 8)
predigy-book        6 tests   (snapshot/delta/gap/wrong-market/edge cases)
predigy-kalshi-rest 6 tests   (auth round-trip, PSS non-determinism, bad PEM,
                               url builder, public client, auth required)
predigy-kalshi-md  27 tests   (25 unit: backoff, decode, messages, client;
                                + 2 integration: loopback session, reconnect
                                replay)
predigy-poly-md    14 tests   (12 unit: backoff, decode, messages;
                                + 2 integration: loopback session w/ batched
                                JSON-array framing, reconnect replay)
md-recorder         5 tests   (4 unit: RecordedEvent round-trips for snapshot/
                                delta/rest_resync + schema-version tag;
                                + 1 integration: Phase 1 acceptance —
                                replay-vs-recorder identical book)
                   ─────────
                   73 tests   (+ 3 doctests)
```

CI gates (run by `.github/workflows/ci.yml` on push to `main` /
`claude/**` and on every PR against `main`):
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

### Kalshi WebSocket
- URL: `wss://api.elections.kalshi.com/trade-api/ws/v2`
- Same auth headers as REST on the upgrade request (path = WS URL path,
  method = `GET`).
- Public channels implemented: `orderbook_delta`, `ticker`, `trade`.
- Authenticated channels (`fill`, `user_orders`, `market_positions`)
  deferred to Phase 2.
- Snapshots include a sequence number; deltas must be applied in strict
  order. The book emits `Gap { expected, got }` on a sequence break — the
  caller (md-recorder) re-fetches a REST snapshot to resync.
- Repeated subscriptions on the same connection are no-ops post-2026, so
  on reconnect we replay all saved subs unconditionally.
- Decimal-string fixed-point on the wire: prices are quoted as
  `"0.0800"`, sizes as `"300.00"`, deltas as `"-54.00"`. Decoded by
  `predigy_kalshi_md::decode` into the integer-cent / `u32` / `i32` types
  the book expects.
- Server sends Ping frames every 10s with body `"heartbeat"`;
  `tokio-tungstenite` auto-replies with Pongs.

### Kalshi FIX 4.4 (not yet implemented)
- Request access via `[email protected]`.
- Used for order entry/cancel/amend in Phase 2.

### Polymarket WS
- URL: `wss://ws-subscriptions-clob.polymarket.com/ws/market`
- No auth needed for the public market channel.
- Subscribe payload: `{ "assets_ids": [...], "type": "market",
  "custom_feature_enabled": false }`. Note the documented spelling:
  **`assets_ids`** (plural with trailing `s` on `assets`).
- No in-band unsubscribe — to drop a subscription, close the connection.
- Events tagged on `event_type`: `book` (full snapshot), `price_change`
  (incremental, carries `best_bid`/`best_ask`), `last_trade_price`,
  `tick_size_change`. Multi-event JSON-array framing is supported by the
  decoder.
- Numerics are decimal strings (variable tick size); parsed to f64.
  Reference price only — never used for execution sizing.

## Known limitations / open items

- **No live API key tested yet** — `Signer` is unit-test-verified
  (round-trip with the public key) but has not signed a real Kalshi REST or
  WS request end-to-end. The integration tests in `predigy-kalshi-md` use
  an in-process loopback WS server with auth disabled, so they validate
  protocol/decoding/reconnect but not the auth handshake against
  production.
- **Bare-metal Chicago VPS not yet ordered** — that's a manual vendor
  process; not blocked on code.

## Next chunk to build

The remaining Phase 1 work is operational, not code:

1. **Live shake-down on a real Kalshi key**: run `md-recorder` against
   the production WS+REST endpoints from a workstation with public CA
   roots, capture an hour or two of data, replay it, confirm
   replay-vs-recorder identical. The integration test in
   `bin/md-recorder/tests/replay_vs_recorder.rs` proves the logic; this
   step proves the wire/auth path.
2. Optional: capture a 24h sample for the Phase 1 acceptance corpus.
3. Optional: parquet rotation for the on-disk schema (deferred to
   Phase 3 unless storage volume forces the issue earlier).

Then we move to Phase 2: OMS + risk + FIX exec + first live strategy
(intra-venue arb).

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

| SHA (short) | Subject |
|---|---|
| _pending_ | Add `predigy-poly-md` (Polymarket WS reference client) — Phase 1, part 3 |
| `efe0c1f` | Merge PR #5: `bin/md-recorder` (NDJSON recorder w/ REST resync) — Phase 1, part 4 |
| `df6bb53` | Merge PR #3: `predigy-kalshi-md` (Kalshi WS client) |
| `c5ed5be` | Merge PR #2: docs + CI workflow |
| `bdc8019` | Fix `clippy::map_unwrap_or` in `current_unix_ms` |
| `18dcede` | Add CI workflow and remove "manual setup" docs note |
| `9fc43cf` | Document plan and current build state in repo |
| `9884459` | Add `predigy-book` and `predigy-kalshi-rest` crates (Phase 1, part 1) |
| `1eafd3f` | Scaffold Cargo workspace and `predigy-core` crate (Phase 0) |
| `b15fc05` | Initial commit (README only) |
