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
| 2 | OMS + risk + FIX exec + first live strategy (intra-venue arb) | ✅ Done (logic): `risk` + `oms` (with durable cid + mass-cancel-on-kill) + `kalshi-exec` (REST) + `kalshi-fix` (FIX 4.4) + `arb-trader`. Live shake-down with real capital is the open item. |
| 3 | Backtester + sim | ✅ (logic): `predigy-sim` with IOC SimExecutor + NDJSON Replay + queue-position module for resting orders + `bin/sim-runner` CLI. Queue model integration into SimExecutor pending. |
| 4 | Market making + rebate capture | ⬜ Deferred (≥$25k). FIX exec ready when MM lands. |
| 5 | Cross-venue signal arb (primary engine) | ✅ (logic): `bin/cross-arb-trader`. Stat-arb on Kalshi vs Polymarket reference; live shake-down pending. |
| 6 | News/data latency (free feeds first) | ✅ (logic): `predigy-ext-feeds` (NWS active-alerts) + `bin/latency-trader` wired through OMS. Live shake-down pending. |
| 7 | Statistical / model alpha | ✅ (logic): `predigy-signals` (Beta-Binomial / Elo / Kelly) + `bin/stat-trader` wired through OMS. Live shake-down pending. |
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
│   │   └── tests/
│   │       ├── loopback_session.rs    end-to-end against an in-process mock; covers
│   │       │                          single-frame events and JSON-array batching
│   │       └── reconnect_replay.rs    server drops, client adds a second asset
│   │                                  during backoff, reconnect sends the union
│   └── risk/                          ✅ Phase 2 part 1
│       └── src/
│           ├── lib.rs                 module roots + re-exports + quick-start
│           ├── limits.rs              Limits / PerMarketLimits / AccountLimits /
│           │                          RateLimits config (0 = disabled by convention).
│           │                          Per-market overrides supported. JSON-friendly
│           │                          duration_ms serde for the rate-limit window.
│           ├── state.rs               AccountState — positions per (market, side),
│           │                          daily realised P&L, sliding window of recent
│           │                          order timestamps for rate limiting,
│           │                          kill-switch flag. Pruning amortised over
│           │                          orders_in_window calls.
│   │       └── engine.rs              RiskEngine.check(intent, state, now) →
│   │                                  Decision::Approve | Reject(Reason). First
│   │                                  breach wins; checks every limit type
│   │                                  including kill switch, order rate, daily
│   │                                  loss, per-market position/notional, and
│   │                                  account gross notional.
│   └── oms/                           ✅ Phase 2 part 2
│       ├── src/
│       │   ├── lib.rs                 module roots + re-exports
│       │   ├── cid.rs                 CidAllocator: deterministic
│       │   │                          {strategy_id}:{market}:{seq:08} ids
│       │   ├── executor.rs            Executor trait (submit/cancel async fns,
│       │   │                          no async-trait crate needed); ExecutionReport
│       │   │                          envelope (Acked/PartiallyFilled/Filled/
│       │   │                          Cancelled/Rejected); StubExecutor for tests
│       │   ├── position_math.rs       apply_fill — pure VWAP + realised-P&L math
│       │   │                          (Buy blends VWAP w/ banker's rounding;
│       │   │                          Sell shrinks position and books P&L vs
│       │   │                          old avg; sells caps at held qty)
│       │   ├── record.rs              OrderRecord — per-order tracking
│       │   │                          (cid, state machine, cumulative fill,
│       │   │                          VWAP per order, cancel-in-flight flag,
│       │   │                          venue order id). Drops out-of-order
│       │   │                          fill reports.
│       │   └── runtime.rs             Oms<E> + OmsHandle. Single tokio task
│       │                              owns AccountState, orders map, cid
│       │                              allocator, risk engine, executor.
│       │                              All inputs cross mpsc boundaries
│       │                              (submit/cancel/kill/reconcile +
│       │                              ExecutionReports). biased select! so
│       │                              fills can't starve under heavy submits.
│       │                              OmsEvent stream surfaces every state
│       │                              transition + PositionUpdated.
│   │   └── tests/runtime.rs           submit→ack→fill (happy path);
│   │                                  risk rejection blocks executor; partial
│   │                                  then terminal fill blends VWAP;
│   │                                  sell after buy realises P&L; cancel;
│   │                                  kill switch blocks/unblocks; reconcile
│   │                                  flags mismatches; executor failure
│   │                                  doesn't book a phantom order.
│   ├── kalshi-exec/                   ✅ Phase 2 part 3 (REST flavour)
│   │   ├── src/
│   │   │   ├── lib.rs                 module roots + re-exports + quick-start
│   │   │   ├── error.rs               Error enum (Rest, Unsupported, Decode)
│   │   │   ├── mapping.rs             Order → Kalshi V2 CreateOrderRequest:
│   │   │   │                          (Yes, Buy)→bid, (Yes, Sell)→ask,
│   │   │   │                          (No, Buy)→ask at complement,
│   │   │   │                          (No, Sell)→bid at complement.
│   │   │   │                          PostOnly→GTC + post_only=true.
│   │   │   │                          FillRecord → predigy_core::Fill.
│   │   │   └── executor.rs            RestExecutor implements oms::Executor.
│   │   │                              submit() POSTs and synthesises
│   │   │                              Acked / Rejected; cancel() DELETEs and
│   │   │                              synthesises Cancelled. Background task
│   │   │                              polls /portfolio/fills (jittered
│   │   │                              ±10%) and maps each new fill into an
│   │   │                              ExecutionReport (PartiallyFilled /
│   │   │                              Filled when cumulative reaches target).
│   │   │                              Aborted on drop.
│   │   └── tests/
│   │       ├── http_mock.rs           hand-rolled HTTP/1.1 mock server;
│   │       │                          one request per connection,
│   │       │                          mutable route table.
│   │       └── oms_integration.rs     end-to-end: submit→Acked→polled fill
│   │                                  drives Filled+PositionUpdated;
│   │                                  cancel emits Cancelled; submit
│   │                                  failure (4xx) leaves zero state.
│   └── sim/                           ✅ Phase 3 part 1
│       ├── src/
│       │   ├── lib.rs                 module roots + re-exports
│       │   ├── book_store.rs          BookStore — Arc<Mutex<HashMap<MarketTicker,
│       │   │                          OrderBook>>> shared between Replay and
│       │   │                          SimExecutor; with_book / with_book_mut
│       │   │                          callbacks scope the lock tightly so the
│       │   │                          (non-Send) MutexGuard never crosses an
│       │   │                          await.
│       │   ├── matching.rs            Pure match_ioc: walks the touch only,
│       │   │                          mutates the book via a synthetic Delta,
│       │   │                          handles Buy YES + Buy NO (with
│       │   │                          NO-at-complement mapping).
│       │   │                          Sells flagged Unsupported (strategies
│       │   │                          should express exits as buy-of-opposite).
│       │   ├── executor.rs            SimExecutor implements oms::Executor.
│       │   │                          IOC only for v1; emits Acked then
│       │   │                          Filled / PartiallyFilled+Cancelled
│       │   │                          (or Cancelled with "no liquidity" if
│       │   │                          the limit doesn't cross). GTC and FOK
│       │   │                          rejected with Unsupported. Aborted on drop.
│       │   └── replay.rs              Replay: streams md-recorder NDJSON
│       │                              line-by-line through the BookStore,
│       │                              calling an async on_update callback
│       │                              after each event so the strategy can
│       │                              run inline. Surfaces sequence gaps as
│       │                              ReplayUpdate::Gap; rejects unknown
│       │                              schema versions.
│       └── tests/
│           └── arb_replay.rs          end-to-end: build a 2-snapshot NDJSON
│                                      payload (no-arb, then arb-fires),
│                                      drive ArbStrategy via Replay through
│                                      a real OMS+SimExecutor, assert YES
│                                      and NO position updates and book
│                                      consumption (75 left at each touch).
└── bin/
    ├── md-recorder/                   ✅ Phase 1 part 4
    │   ├── src/
    │   │   ├── lib.rs                 module roots + re-exports
    │   │   ├── recorded.rs            on-disk NDJSON schema (versioned), with a
    │   │   │                          synthetic RestResync event the recorder
    │   │   │                          injects after a Gap-triggered REST fetch
    │   │   ├── recorder.rs            Recorder<P: SnapshotProvider> — drains the
    │   │   │                          kalshi-md Connection, writes one NDJSON
    │   │   │                          line per event, applies snapshot/delta to
    │   │   │                          a per-market OrderBook, on Gap pulls a
    │   │   │                          fresh snapshot via P and emits RestResync,
    │   │   │                          on Reconnected forces a resync per market
    │   │   └── main.rs                CLI (clap): --output, --market…,
    │   │                              --kalshi-key-id, --kalshi-pem; SIGINT for
    │   │                              graceful shutdown; tracing-subscriber logs
    │   └── tests/
    │       └── replay_vs_recorder.rs  Phase 1 acceptance: drive recorder
    │                                  through subscribe→snapshot→delta→
    │                                  gap-induced resync; replay the NDJSON;
    │                                  assert replayed book ≡ recorder's
    │                                  in-memory book
    ├── latency-trader/                ✅ Phase 6
    │   └── src/
    │       ├── lib.rs                 module roots + re-exports
    │       ├── strategy.rs            LatencyStrategy + LatencyRule (serde):
    │       │                          event-substring + optional area-substring
    │       │                          + min-Severity match against NwsAlert.
    │       │                          First armed rule fires once → IOC Intent
    │       │                          at rule.max_price_cents; rule disarms
    │       │                          until rearm_all/rearm.
    │       └── main.rs                CLI (clap): --rule-file (JSON),
    │                                  --nws-states, --nws-user-agent,
    │                                  per-market + account risk caps,
    │                                  --cid-store, --dry-run. Single
    │                                  tokio::select! over nws_rx +
    │                                  oms.next_event + ctrl-c.
    ├── stat-trader/                   ✅ Phase 7
    │   └── src/
    │       ├── lib.rs                 module roots + re-exports
    │       ├── strategy.rs            StatStrategy + StatRule (serde):
    │       │                          per-market model probability + side.
    │       │                          Derives ask via complement-of-opposite-bid;
    │       │                          fires IOC when ask < model_p * 100 by
    │       │                          ≥ min_edge_cents; size = quarter-Kelly
    │       │                          contracts (factor configurable) capped
    │       │                          by max_size and book depth; per-market
    │       │                          cooldown.
    │       └── main.rs                CLI (clap): --rule-file (JSON),
    │                                  --bankroll-cents, --kelly-factor,
    │                                  --cooldown-ms, risk caps, --cid-store,
    │                                  --dry-run. Subscribes to Kalshi WS
    │                                  for the rules' markets, evaluates on
    │                                  every snapshot/delta.
    └── arb-trader/                    ✅ Phase 2 part 4
        └── src/
            ├── lib.rs                 module roots + re-exports
            ├── strategy.rs            ArbStrategy + ArbConfig:
            │                          detects when 100¢ - yes_ask - no_ask
            │                          - taker_fee(both) >= min_edge_cents,
            │                          caps size at thinnest leg's touch qty,
            │                          enforces a per-market cooldown.
            │                          ArbOpportunity carries the per-pair
            │                          and total edge for logging even when
            │                          the strategy chooses not to fire.
            ├── runner.rs              Runner: single tokio task, single
            │                          select! over md.next_event +
            │                          oms.next_event + stop. Submits IOC
            │                          pairs; logs OMS lifecycle events
            │                          (Acked/Filled/PositionUpdated/...).
            │                          Drops the local book on WS sequence
            │                          gap and waits for a fresh snapshot.
            └── main.rs                CLI (clap): --market…,
                                       --kalshi-key-id, --kalshi-pem,
                                       per-market + account risk caps,
                                       --min-edge-cents, --size-per-pair,
                                       --cooldown-ms, --dry-run.
```

## Test counts

```
predigy-core       18 tests   (price 4, side 1, position 2, fees 8, intent 3)
predigy-book        6 tests   (snapshot/delta/gap/wrong-market/edge cases)
predigy-kalshi-rest 6 tests   (auth round-trip, PSS non-determinism, bad PEM,
                               url builder, public client, auth required)
predigy-kalshi-md  27 tests   (25 unit: backoff, decode, messages, client;
                                + 2 integration: loopback session, reconnect
                                replay)
predigy-poly-md    14 tests   (12 unit + 2 integration: loopback session w/
                                batched JSON-array framing, reconnect replay)
predigy-risk       21 tests   (limits round-trip / overrides / for_market;
                                state position+pnl+rate-window invariants;
                                engine: kill switch, per-market position +
                                notional, gross notional, daily-loss, rate
                                limit, sell-shrinks-only, 0=disabled)
predigy-oms        31 tests   (22 unit: cid allocator + persistent cid
                                store round-trips + chunk refill +
                                no-repeat-across-restart, position_math
                                VWAP + P&L, OrderRecord state machine;
                                + 9 integration: submit→ack→fill,
                                risk-reject, partial+final fill, sell
                                realises P&L, cancel, kill switch,
                                kill-switch mass-cancel,
                                reconcile mismatch, executor-failure)
predigy-kalshi-exec 15 tests   (12 unit: Yes/No mapping (4 cases) including
                                NO-at-complement, post_only as GTC+flag,
                                Market rejected, FillRecord → domain
                                with side-aware price, tracking
                                round-trip, jitter band, jitter-zero;
                                + 3 integration: submit+polled-fill→Filled,
                                cancel→Cancelled, 4xx submit→Executor err)
md-recorder         5 tests   (4 unit: RecordedEvent round-trips for
                                snapshot/delta/rest_resync + schema tag;
                                1 integration: Phase 1 acceptance)
arb-trader          8 tests   (strategy: balanced market → no arb;
                                meaningful edge detected w/ correct math;
                                intents are Buy YES + Buy NO at the
                                derived asks; size capped by thinnest
                                leg; cooldown blocks repeat then expires;
                                marginal opportunity blocked by fees;
                                empty book side → no arb;
                                reset_cooldown clears throttle)
predigy-sim        28 tests   (27 unit: BookStore + matching (7 cases) +
                                SimExecutor (6 cases) + Replay (4 cases) +
                                queue-position (8 cases: queue-ahead
                                consumed before fill, fills after
                                queue-pass, cap-at-remaining, side-
                                mismatch, price-mismatch, NO-bid hit by
                                YES-taker, maker-fee on synth fill,
                                zero-count noop);
                                + 1 integration: ArbStrategy through
                                Replay+OMS+SimExecutor)
predigy-kalshi-fix 23 tests   (21 unit: frame round-trip + corruption +
                                partial + garbage detection; messages
                                Logon/NewOrderSingle/Cancel/Heartbeat
                                round-trip, ExecutionReport parse for
                                Filled and Rejected, post_only-rejected,
                                missing-required-tag; session: out_seq
                                monotonic, in_seq advances on happy path,
                                rejects compid/seq mismatch + replay;
                                + 2 integration: full Logon→Submit→
                                Acked→Filled against a TCP loopback
                                FIX server)
cross-arb-trader    6 tests   (no-intent-until-poly, buys YES when Kalshi
                                under-prices vs Poly, NO mirror, no-edge
                                when over-prices, cooldown throttle,
                                unknown-market ignored)
latency-trader      8 tests   (substring match → IOC at max_price_cents;
                                disarm-after-fire; rearm; area filter;
                                min-severity gate; first-rule-wins;
                                Severity ordering; Unknown < Minor)
stat-trader         8 tests   (no-intent when ask above model_p; YES
                                fires when under-priced vs model; NO
                                mirror via 1-model_p; min_edge_cents
                                gate; Kelly size cap by max_size;
                                cooldown blocks repeat then expires;
                                book-depth caps qty; unknown market
                                ignored)
                   ─────────
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

Phase 3 in flight. Sim runtime + matching + replay + arb integration
test landed. Still open:

1. **Queue-position model for resting orders.** The current sim
   handles IOC takers only; GTC / `PostOnly` makers sit in a queue
   and fill iff cumulative trade volume past their seq number exceeds
   their queue-ahead. Lands when a maker strategy needs it (Phase 4
   MM) — earliest. `arb-trader` doesn't exercise it.
2. **Realistic-cadence replay.** Today the simulator walks the file
   as fast as the disk. A `--respect-timestamps` mode that sleeps
   between events so latency-sensitive sims see the right
   inter-arrival distribution.
3. **Backtest binary** (`bin/sim-runner` or similar). Today the sim
   is exercised via `cargo test`; a CLI that loads a strategy +
   NDJSON file + risk limits and prints PnL summary statistics is a
   small follow-up.

Pending Phase 2 items remain valid:

- Live shake-down of `arb-trader` with a real Kalshi key and a
  $500 cap.
- Live shake-down of `md-recorder` against production endpoints.
- Phase-2 hardening (durable cid storage, mass-cancel-on-kill,
  persistent OMS state, order amend).
- `predigy-kalshi-fix` — FIX 4.4 executor for the eventual MM
  strategy.

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
| _pending_ | Add `predigy-sim` (backtester runtime) — Phase 3, part 1 |
| `7bf53fd` | Merge PR #10: `bin/arb-trader` (intra-venue arb, first live strategy) — Phase 2, part 4 |
| `b1e1370` | Merge PR #9: promote `oms` + `kalshi-exec` stack to main |
| `b931435` | Merge PR #8: `predigy-kalshi-exec` (REST executor) — Phase 2, part 3 |
| `07a48d7` | Merge PR #7: `predigy-oms` (order management state machine) — Phase 2, part 2 |
| `1c1e848` | Merge PR #6: `predigy-risk` (pre-trade limits + breakers) — Phase 2, part 1 |
| `bb1b072` | Merge PR #4: `predigy-poly-md` (Polymarket WS reference client) |
| `efe0c1f` | Merge PR #5: `bin/md-recorder` (NDJSON recorder w/ REST resync) — Phase 1, part 4 |
| `df6bb53` | Merge PR #3: `predigy-kalshi-md` (Kalshi WS client) |
| `c5ed5be` | Merge PR #2: docs + CI workflow |
| `bdc8019` | Fix `clippy::map_unwrap_or` in `current_unix_ms` |
| `18dcede` | Add CI workflow and remove "manual setup" docs note |
| `9fc43cf` | Document plan and current build state in repo |
| `9884459` | Add `predigy-book` and `predigy-kalshi-rest` crates (Phase 1, part 1) |
| `1eafd3f` | Scaffold Cargo workspace and `predigy-core` crate (Phase 0) |
| `b15fc05` | Initial commit (README only) |
