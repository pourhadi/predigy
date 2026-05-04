# Predigy Plan — Automated HFT Trading System for Kalshi

> Authoritative architecture / strategy / infrastructure plan. Source of truth
> for design decisions. Build state lives in [`STATUS.md`](./STATUS.md).

## Context

**Goal.** Build an automated, robust, profitable trading system that executes
only on **Kalshi** (CFTC-regulated, US-accessible, FIX 4.4, formal MM rebate
program), but consumes signals from every adjacent venue (Polymarket,
sportsbooks, CME futures, BLS/NWS/etc.) to extract every available edge.

**Bootstrap profile (current).** $5,000 risk capital, ~$80-150/mo infra
budget. Single Chicago VPS, no AWS Tier B yet, free data feeds only. Market
making and premium feeds gate on account-value thresholds (see Capital
Allocation below). The plan still describes the full production system;
phases 4 / 6 / 7 / 8 are explicitly deferred but kept here so the
architecture supports them.

## Confirmed Kalshi fee schedule (Feb 2026)

```
taker_fee_per_trade = ceil(0.07   * C * P * (1 - P))   // dollars; ceil to nearest cent
maker_fee_per_trade = ceil(0.0175 * C * P * (1 - P))   // 75% cheaper
```
where `C` = number of contracts, `P` = price in dollars (0.01-0.99).

| Price | Taker /contract | Maker /contract | Round-trip taker | Round-trip maker |
|---|---|---|---|---|
| $0.50 | 1.75¢ (3.5%) | 0.44¢ (0.875%) | 7.0% | 1.75% |
| $0.30 / $0.70 | 1.47¢ (~2.1-4.9%) | 0.37¢ (~0.5-1.2%) | ~4-9% | ~1-3% |
| $0.10 / $0.90 | 0.63¢ (0.7-7%) | 0.16¢ (~0.2-2%) | ~1.4-14% | ~0.4-3.5% |

Implemented in [`crates/core/src/fees.rs`](../crates/core/src/fees.rs) with
integer-cent arithmetic (no float drift on the hot path).

**Implications baked into the strategy thresholds:**
- Cross-venue arb at p≈$0.50 needs > ~3.5¢ Kalshi-vs-reference dislocation for
  taker-only execution; > ~2.2¢ if at least one leg is a maker fill.
- Stat-arb model edge must clear ~3.5% net at midpoint just to be EV-positive
  on a single round-trip taker. Accept only signals > 5%.
- MM can only quote net-positive when half-spread > maker round-trip
  (~0.875%). Two-sided quoting at < ~2¢ wide at midpoint is unprofitable
  absent rebates or directional alpha. Hence MM is deferred until we have
  rebate qualification and inventory tools.
- Round-up to a whole cent makes very small orders disproportionately
  expensive. Minimum economic order size at midpoint is ~10 contracts.

## Why this design

- Kalshi-only execution removes jurisdictional risk (US person-friendly),
  gives us FIX 4.4 and a published MM rebate (up to 1% with a $7k/wk cap),
  and avoids on-chain settlement latency.
- Polymarket is the deepest prediction-market book in the world; treating its
  mid as a free reference price is one of the highest-quality alphas
  available to a Kalshi-only trader.
- The four strategy families (MM, arb, news/data latency, statistical) share
  ~80% of infra (md, OMS, risk, observability). Building them as plug-in
  `Strategy` traits over a single core lets each one compound on the other
  (e.g. MM uses statistical fair-value as anchor; news/latency uses MM
  inventory as constraint).
- Rust gives sub-ms in-process latency, zero-GC hot paths, and safe
  concurrency for an OMS that absolutely cannot double-fire orders.

## Edges to capture (in expected $/risk order)

1. **Kalshi MM rebates** — up to 1% rebate, $7k/week cap, plus the new
   Sportsbook Hedging Rebate Program (effective Feb 2026, runs through Feb
   2027). Pure subsidy on volume we'd quote anyway.
2. **Intra-venue static arb on Kalshi** — `YES + NO < $1 - 2*fee`,
   complementary contracts in a series summing > $1 or < $1, basket vs.
   component mispricings.
3. **Cross-venue signal arb** — Polymarket / Betfair / sportsbook mid is
   materially different from Kalshi; lift/hit Kalshi when its quote is stale
   relative to the deeper book. Hold inventory, hedge via opposing Kalshi
   contract or by liquidating into Kalshi liquidity later.
4. **News/data latency alpha** — race the book on FOMC, NFP, CPI, NWS weather
   releases, sports score feeds, election calls. Edge measured in 50-500ms;
   requires colo + parsed feed.
5. **Statistical / model alpha** — calibrated probabilistic models (elo +
   injuries for sports, polling+fundamentals for elections, term-structure
   for macro) take stale resting orders.
6. **Adverse-selection-aware market making** — quote two-sided around model
   fair value, skew to inventory, widen on toxicity signals.

## System Architecture

A single-process Rust binary per strategy *family* (deployable
independently), all sharing a common `predigy-core` crate. Co-located in
**Chicago metro** (Kalshi's matching engine; sub-2ms RTT measured) on
dedicated bare-metal or Equinix CH1/CH2/CH4 colo. AWS us-east-1 is
~25-40ms from Chicago and disqualifying for serious HFT.

```
predigy/
├── crates/
│   ├── core/            # types: Market, Order, Fill, Side, Px, Qty, fees
│   ├── kalshi-md/       # WebSocket book/trade feed, snapshot+diff reconciliation
│   ├── kalshi-exec/     # FIX 4.4 session (preferred) + REST fallback for non-order ops
│   ├── kalshi-rest/     # REST client (auth, market metadata, positions, settlements)
│   ├── poly-md/         # Polymarket CLOB WS for reference prices (no exec)
│   ├── ext-feeds/       # ESPN, NWS, BLS, FRED, CME, Betfair, Bluesky firehose, etc.
│   ├── book/            # In-memory L2/L3 order book, lock-free, per-market arena alloc
│   ├── oms/             # Order state machine, idempotency, reconciliation, kill switch
│   ├── risk/            # Pre-trade checks, position/notional/drawdown limits, breakers
│   ├── strategy/        # Strategy trait + impls: mm, arb, latency, stat-arb
│   ├── signals/         # Fair-value engine, toxicity, microstructure features
│   ├── sim/             # Event-driven backtester replaying recorded md + matching engine
│   ├── store/           # Append-only event log (parquet/arrow), Postgres for state
│   └── ops/             # Metrics (Prom), structured logs (tracing+otel), control-plane API
└── bin/
    ├── md-recorder      # 24/7: record every WS msg to parquet for replay
    ├── mm-trader        # market-making bot
    ├── arb-trader       # static + cross-venue arb
    ├── latency-trader   # news/data race bot
    └── stat-trader      # model-driven directional
```

### Critical invariants (enforced in code, tested in sim)

- **Order idempotency**: every `NewOrder` carries a deterministic
  `client_order_id = hash(strategy, market, intent_seq)`; duplicate sends are
  a no-op on the exchange and a panic in OMS.
- **Single source of truth for position**: OMS reconciles against
  `/portfolio/positions` every 5s and on every reconnect; mismatch > 0 →
  kill switch.
- **Pre-trade risk is synchronous**: no order leaves OMS without passing
  `risk::check(order, current_state)` on the calling thread.
- **Strategy never touches the network**: strategies emit `Intent` enum values
  into a channel; OMS owns all venue I/O. Makes sim trivial and prevents bugs
  where a strategy retry causes a double-fire.
- **Kill switch is hardware-simple**: a single atomic bool, polled before
  every send and on every md tick; flipping it cancels all open orders via
  FIX `OrderMassCancelRequest` (tag 35=q) and halts new sends.

## Tech Stack

| Layer | Choice | Why |
|---|---|---|
| Language | Rust (stable, edition 2024) | Latency, safety, zero-GC, trait-based plugin model |
| Async | `tokio` (multi-thread) for I/O, dedicated thread per strategy hot loop | Standard, ecosystem fit |
| FIX | `quickfix-rs` or hand-rolled FIX 4.4 (Kalshi spec) | FIX gives lowest order latency vs REST |
| WS | `tokio-tungstenite` + `simd-json` for parsing | Fast JSON parse on hot path |
| Auth | `rsa` + `sha2` for Kalshi RSA-PSS request signing | Per Kalshi auth spec |
| Order book | Custom intrusive linked-list per price level, `BTreeMap` for levels | O(log n) updates, cache-friendly |
| Storage | Append-only parquet via `arrow-rs`; Postgres (`sqlx`) for OMS state | Fast columnar replay, ACID for orders |
| Metrics | `prometheus` + Grafana | Standard |
| Tracing | `tracing` → OTLP → Tempo | Per-order trace across strategy → OMS → exchange |
| Backtest | Custom event-driven sim replaying parquet | Matching engine simulates queue position |
| Deploy | Single binary per bot, systemd units on Chicago-metro bare-metal (Equinix CH1/CH2/CH4 or QuantVPS Chicago); AWS us-east-2 (Ohio) for non-latency-critical services | Kalshi matching engine in Chicago metro (~1.14ms RTT). AWS us-east-1 ~25-40ms — disqualifying. |
| Secrets | AWS Secrets Manager + IAM role (control plane), Vault on the trading host (data plane), RSA key sealed at rest | API key hygiene |

## Data Sources

**Execution venue**
- Kalshi WebSocket (`wss://api.elections.kalshi.com/trade-api/ws/v2`):
  order book, ticker, trade, fill streams.
- Kalshi REST (`https://api.elections.kalshi.com/trade-api/v2`): market
  metadata, series taxonomy, positions, settlements, balance.
- Kalshi FIX 4.4 (request via `[email protected]`): order entry/cancel/amend.

**Reference prices (signal only, no exec)**
- Polymarket CLOB WS (`wss://ws-subscriptions-clob.polymarket.com/ws/market`)
  — book + price.
- Betfair Exchange Stream API (sports, where Kalshi has copycat contracts).
- PredictIt (where still active).

**Underlying data feeds (latency alpha)**
- Sports: SportRadar (paid, sub-second), ESPN scoreboard JSON (free,
  5-15s), NFL/NBA Gamebook PDFs (slow, post-hoc only).
- Weather: NWS API + NOAA NEXRAD for storm/temperature contracts.
- Macro: BLS direct (CPI/NFP), BEA (GDP), FRED for derived series.
  Pre-register for embargoed feeds where possible.
- Crypto: CME BTC/ETH futures (CME MDP 3.0 if budget allows; else
  Coinbase/Binance L2) for crypto contracts.
- News: X firehose / Bluesky firehose / Reuters terminal if budget;
  otherwise NewsAPI / GDELT for backfill.
- Polling: 538 / RealClearPolitics / Nate Silver scrapes for election
  contracts.

## Infrastructure

> **Provisioning note.** The current dev environment has no AWS MCP / CLI /
> IAM credentials wired up, so AWS resources can't be created from inside
> Claude. Tier B will be delivered as Terraform (recommended) or AWS CDK so
> it's reproducible and code-reviewable. Bare-metal Chicago colo cannot be
> ordered via AWS API regardless — those vendors require a manual
> quote/contract.

### Hosting layout (two tiers)

**Tier A — Latency-critical (Chicago metro, NOT AWS).** Hosts everything on
the order path: FIX session, OMS, risk, MM/arb/latency strategies. Kalshi's
matching engine sits in the Chicago metro; AWS us-east-1 is ~25-40ms away.

| Option | Vendor | Spec | ~$/mo | Notes |
|---|---|---|---|---|
| **A1 (recommended start)** | QuantVPS / NYC Servers Chicago | Dedicated bare-metal, 16-32 cores, 64-128GB RAM, NVMe, 10Gbps | $400-1,200 | <2ms to Kalshi, fastest to stand up, month-to-month |
| **A2 (scale)** | Equinix CH1/CH2/CH4 colo cabinet | 1U server (e.g. Dell R660, Xeon Gold 6442Y, 128GB, NVMe RAID) | $1,500-3,500 (cab+power) + ~$8k cap-ex per server | True colo, cross-connect options, multi-year commit |
| **A3 (overkill)** | Equinix + cross-connect to Kalshi | Same as A2 + private cross-connect | A2 + ~$300/mo per XC | Only if Kalshi will sell a direct XC; ask via [email protected] |

Run **two identical Tier A boxes** (active/warm-standby) once account ≥
$15k. Before that the cost of a second box exceeds expected revenue.

**Tier B — Everything else (AWS us-east-2 Ohio, ~6ms to Chicago).**
Postgres, Prometheus/Grafana, Loki, Tempo, the md-recorder long-term
archive, the backtester, CI runners, control-plane API, dashboards.
Defer until account ≥ $50k; until then run all of these on the Chicago
VPS or use free-tier SaaS (Grafana Cloud free, Cloudflare R2 archive).

| Component | AWS resource | Size | ~$/mo on-demand | With 1yr RI |
|---|---|---|---|---|
| Postgres (OMS state, daily PnL) | RDS Postgres `db.m7g.large` Multi-AZ + 200GB gp3 | 2vCPU/8GB | $280 | $180 |
| Metrics + log stack | EC2 `m7g.xlarge` ×1 + 500GB gp3 + Grafana Cloud free | 4vCPU/16GB | $130 | $80 |
| md-recorder long-term store | S3 Standard-IA + Glacier Deep Archive lifecycle | ~5TB/yr growth | $50-150 | flat |
| Backtest workers | EC2 `c7i.4xlarge` Spot, on-demand only when running | 16vCPU/32GB | $50-200 (intermittent) | n/a |
| Secrets / KMS | Secrets Manager + KMS CMK | a handful | $5-15 | flat |
| VPC, NAT, traffic | NAT GW + cross-AZ + egress | modest | $40-80 | flat |
| **Tier B subtotal** | | | **~$555-875** | **~$435-705** |

**Tier C — Recording-only (NY metro, optional).** A single
`c7i.large`-class box in NY/NJ to colocate near Polymarket's relayer. Used
only by `md-recorder` for Polymarket. ~$60-120/mo.

### Per-server software stack (Tier A)

- Ubuntu 24.04 LTS, kernel ≥6.8, with `tuned-adm profile latency-performance`,
  IRQ pinning, `isolcpus` for strategy hot threads, transparent hugepages
  off, `nohz_full` on isolated cores, `chrony` against a local PTP/GPS
  source if Equinix supports it.
- ext4 on NVMe for the parquet event log; XFS on a separate volume for
  Postgres WAL (only if running PG locally — preferred to keep PG in
  Tier B once we get there).
- `systemd` units per binary, `journald` → Vector → Loki in Tier B.
- Node exporter + custom exporter for FIX/WS health, latency histograms,
  fill ratios.

### Connectivity

- Primary: vendor's public Internet (10Gbps), with BGP-anycast resolver
  disabled and Kalshi IPs pinned in `/etc/hosts` after first DNS resolve.
- Backup: a second IP transit if at Equinix; otherwise vendor SLA.
- Outbound only — no inbound except the control-plane Wireguard tunnel
  back to Tier B.
- Exchange auth: RSA private key sealed in `systemd-creds` (Tier A) or
  pulled from Vault on boot, never on disk in cleartext.

## Subscriptions (full list, with cost ranges)

Costs are 2026 ranges. Items marked **MVP** are needed for Phases 0-3; the
rest layer in as the corresponding strategy phase activates.

### Exchange & venue access

| Item | Purpose | Cost | Phase |
|---|---|---|---|
| Kalshi account (funded) | Trading | $0 + capital | MVP |
| Kalshi API key (RSA) | Auth | $0 | MVP |
| Kalshi FIX 4.4 access | Order entry latency | $0 (request via [email protected]) | Phase 2 |
| Kalshi MM program designation | Rebates up to 1%, $7k/wk | $0, has quoting obligations | Phase 4 (≥$25k) |
| Kalshi Sportsbook Hedging Rebate enrollment | Sports rebates | $0, must apply | Phase 4 (≥$25k) |
| Polymarket read-only API key | Reference price (no trading) | $0 | MVP |

### Hosting

| Item | Cost |
|---|---|
| Chicago bare-metal (single box, current) | **$30-150/mo** at the smallest viable tier |
| Chicago bare-metal (primary + standby) | **$800-7,000/mo** depending on A1 vs A2 |
| AWS us-east-2 Tier B (deferred until ≥$50k) | **$555-875/mo** on-demand, ~$435-705 with 1yr RI |
| Optional NY box for Polymarket md | $60-120/mo |
| Equinix cross-connect to Kalshi (if available) | $300/mo |

### Sports data (only if trading sports markets)

| Item | Coverage | Cost | Phase |
|---|---|---|---|
| **SportRadar** | Sub-second NFL/NBA/MLB/NHL/soccer official | **$10k+/mo** enterprise | Phase 6 (≥$250k) |
| **OpticOdds** | Sportsbook line aggregator | $1k-5k/mo tier-dependent | Phase 6 (≥$50k) |
| ESPN scoreboard JSON | Free, 5-15s lag | $0 | MVP (development) |
| MySportsFeeds / Rolling Insights | Cheaper alt to SportRadar | $100-2k/mo | Phase 6 if budget-constrained |
| Betfair Exchange Stream API | Liquidity reference for sports | £0 (with funded BF account) + £200/mo data charge after threshold | Phase 5 |

### Macro / economic

| Item | Coverage | Cost | Phase |
|---|---|---|---|
| BLS direct download (CPI/NFP) | Embargo lifts at release; race the parser | $0 | Phase 6 |
| BEA, FRED | GDP, derived series | $0 | Phase 6 |
| **Bloomberg Terminal** (optional) | Lowest-latency macro feed | **~$32k/yr per seat** | Phase 6/7 (≥$250k) |
| Refinitiv / LSEG Eikon | Alternative to Bloomberg | similar tier | Phase 6/7 |
| CME MDP 3.0 (futures) | Crypto/macro reference | $1k-25k/mo depending on subs | Phase 7 |

### Crypto

| Item | Coverage | Cost | Phase |
|---|---|---|---|
| Coinbase Advanced WS | Free L2 BTC/ETH | $0 | Phase 6 |
| Binance WS | Free L2 | $0 | Phase 6 |
| Kaiko / Amberdata aggregated feed | Cleaner cross-exchange | $1k-10k/mo | Phase 7 if needed |

### Weather

| Item | Cost |
|---|---|
| NWS API + NOAA NEXRAD | $0 |
| Weather Source / Tomorrow.io enterprise (optional) | $500-5k/mo |

### News / social / political

| Item | Cost | Phase |
|---|---|---|
| X (Twitter) Enterprise tier (firehose) | $42k+/mo | Skip unless news desk is core |
| X PRO tier | $5k/mo | Phase 6 |
| Bluesky firehose | $0 (still permissive) | Phase 6 |
| GDELT | $0 backfill | Phase 7 |
| NewsAPI / Aylien | $500-2k/mo | Phase 6 |
| RealClearPolitics / 538 polling scrape | $0 (be polite) | Phase 7 |
| AP Election API | $5k+/mo around election cycles | Phase 7 (election cycles only) |

### Software / SaaS

| Item | Cost |
|---|---|
| GitHub Team or Enterprise | $4-21/user/mo |
| PagerDuty / Opsgenie | $20-40/user/mo |
| Sentry (error tracking) | $26+/mo |
| Grafana Cloud (or self-host on Tier B) | Free tier sufficient at start; $50+/mo as series grow |
| Tailscale / Wireguard (control plane) | Free for small teams; $6/user/mo paid |
| 1Password Business / Vault | $8-20/user/mo |
| Domain + DNS (Cloudflare) | $20/yr + free DNS |
| TLS certs | Let's Encrypt, $0 |
| HashiCorp Vault (self-hosted on Tier B) | $0 OSS, ~$1.5k/mo Cloud if preferred |

### Legal / compliance / accounting

| Item | Cost |
|---|---|
| LLC / entity formation + ongoing registered agent | $500-2k upfront, $200-500/yr |
| Trading attorney (CFTC, state-by-state for sports) | $5-25k initial, hourly thereafter |
| Tax accountant familiar with §1256 / event contract tax treatment | $3-10k/yr |
| D&O / E&O insurance | $2-10k/yr |
| Books / ledger (QuickBooks or accounting service) | $50-300/mo |

### Cost summary

| Mode | Approx burn (monthly) | Notes |
|---|---|---|
| **Bootstrap (current — $5k capital)** | **$80-150/mo** | Single Chicago VPS, free SaaS, no paid feeds |
| **MVP (Phases 0-3, scaled — ≥$25k capital)** | **$1.5-3k/mo** | One Chicago VPS + Tier B + ops SaaS |
| **Production w/o premium feeds** | **$3-7k/mo** | Standby box, full Tier B with RIs, backups |
| **Production + sports (SportRadar)** | **$15-25k/mo** | Adds enterprise sports contract |
| **Full HFT shop (Bloomberg + SportRadar + colo cab + XC)** | **$50-80k/mo** | Only justify if AUM/revenue supports it |

### Provisioning

When account size justifies AWS Tier B, deliver as a Terraform module
(`infra/aws/`) with:
- VPC, subnets, NAT, SGs (least-privilege egress to Kalshi/Polymarket
  IPs only on Tier A)
- RDS Postgres Multi-AZ
- EC2 for Prom/Grafana/Loki/Tempo (or Grafana Cloud)
- S3 bucket with lifecycle policy for parquet archive
- KMS, Secrets Manager, IAM roles
- Route53 + ACM for control-plane endpoints
- VPN endpoint for control-plane access from Tier A

Tier A is provisioned manually with the vendor (signed quote → server
delivered with IP/iLO) and configured via Ansible playbooks under
`infra/ansible/` (kernel tuning, IRQ pinning, systemd units, Vector→Loki,
Wireguard back to Tier B).

## Risk & Ops

- **Position limits**: per-market notional, per-series notional, per-strategy
  notional, account-wide notional. Hard-coded in `risk::Limits`,
  hot-reloadable from a YAML.
- **Drawdown circuit breakers**: rolling 1h, 1d, 7d. Breach → flatten +
  halt strategy.
- **Toxicity halt**: if 3 consecutive fills move against us by > X bps →
  widen quotes; if 5 → pull and re-evaluate.
- **Reconnect storm protection**: exponential backoff with jitter on WS/FIX
  disconnect; if exchange unreachable > 10s → cancel-all and halt.
- **Two-person review** required for changes to `risk/` and `oms/` (enforced
  via CODEOWNERS + branch protection).
- **Daily reconciliation**: end-of-day Kalshi statement vs internal book;
  mismatch > $1 → page on-call.
- **Observability**: per-strategy PnL (realized/unrealized), fill ratio,
  adverse-selection (10s mark-out), latency histograms (md→signal,
  signal→intent, intent→FIX out, FIX out→ack), inventory by market,
  rebate accrual.

## Capital Allocation & Strategy Gates

| Account value | Active strategies | Infra |
|---|---|---|
| **$5k (start)** | Cross-venue arb (primary), intra-venue static arb, stat-arb on long-tail markets | Single Chicago VPS, local Postgres, Grafana Cloud free, R2 archive — ~$80-120/mo |
| $10k | + free-feed news/data latency (NWS, BLS direct, ESPN, Bluesky, Coinbase WS) | unchanged |
| $15k | + warm-standby Chicago VPS | ~$150-200/mo |
| $25k | + market making on liquid markets, apply for Kalshi MM designation, enroll in Sportsbook Hedging Rebate | unchanged |
| $50k | + paid feeds case-by-case (OpticOdds, MySportsFeeds, news API). AWS Tier B for Postgres/metrics. | $500-1k/mo |
| $250k+ | + SportRadar / Bloomberg / Equinix colo as EV justifies | $5k-25k/mo |

Of the initial $5k, hold $1k as drawdown buffer (don't deploy). Allocate the
remaining $4k 100% to cross-venue arb in Phase 5; layer stat-arb on top
once account ≥ $6k.

## Build Sequence

Sequenced so each phase produces a working artifact and the next phase
compounds on it.

**Phase 0 — Plumbing (Week 1)**
- Cargo workspace, CI (cargo test/clippy/fmt + sim regression), Postgres
  schema, Prom/Grafana, secret loading.
- `core` types: `Price` (cents, u8), `Qty`, `Side`, `Market`, `Order`,
  `Fill`, `Position`, `fees`.

**Phase 1 — Read-only stack (Weeks 2-3)**
- `kalshi-rest`: auth (RSA-PSS), market list, market detail, positions,
  orderbook snapshot.
- `kalshi-md`: WS subscribe, decode, snapshot+diff reconcile, `book` crate.
- `md-recorder` binary: 24/7 capture to parquet (this dataset is itself an
  asset; start recording day 1).
- `poly-md`: same for Polymarket reference book.
- Acceptance: 24h of recorded data, replay reconstructs the book identically
  to a fresh snapshot.

**Phase 2 — OMS + Risk + first live strategy (Weeks 4-6)**
- `kalshi-exec`: FIX 4.4 session (logon, heartbeat, NewOrderSingle,
  OrderCancelRequest, ExecutionReport handling). REST fallback behind same
  trait.
- `oms`: state machine, idempotent cid, reconciliation loop, kill switch.
- `risk`: pre-trade limits, drawdown breakers.
- `arb-trader`: static intra-venue arb only (`YES+NO<1` after fees).
  Smallest blast radius, easiest to validate.
- Acceptance: bot runs 1 week with $500 limit, OMS reconciles to within
  $0.00, no orphan orders, positive-or-flat PnL after fees.

**Phase 3 — Backtester + sim (Weeks 7-8)**
- `sim`: event-driven replay of recorded parquet, naive queue-position model
  (assume FIFO, our fills happen iff our quote is at the front when a contra
  order arrives sized > queue ahead).
- All strategies must run unchanged in sim and live (same `Strategy` trait,
  same `Intent` outputs).
- Acceptance: replay last week's live trading; sim PnL within 10% of
  realized.

**Phase 4 — Market making + rebate capture (DEFERRED until account ≥ $25k)**
- `signals::FairValue` v1: weighted mid of Kalshi book + Polymarket reference
  (when correlated).
- `mm-trader`: quote both sides at FV ± half-spread, skew to inventory,
  widen on toxicity, multi-market.
- Track Kalshi MM tier qualification; aim for tier that maximizes
  rebate / $7k cap.
- Apply for the Sportsbook Hedging Rebate Program if running sports
  markets.
- Acceptance: positive PnL net of fees, MM tier qualified, rebate accruing.

**Phase 5 — Cross-venue signal arb (Weeks 12-13, primary engine for
bootstrap)**
- `arb-trader` extension: when Polymarket mid diverges from Kalshi by > Z
  (calibrated per market), take Kalshi quote in the direction of Polymarket.
  Inventory bounded; exit via Kalshi liquidation or hedging contract.
- Acceptance: Sharpe > 2 on out-of-sample replay window.

**Phase 6 — News/data latency (Weeks 14-16; free feeds first)**
- `ext-feeds`: parsers for SportsRadar (later), NWS alerts, BLS releases
  (with embargo handling), NFL/NBA play-by-play (ESPN free first).
- `latency-trader`: on event → infer probability shift → race the book.
  Co-located in same AWS AZ as feed source where possible.
- Acceptance: measured edge per event type (mark-out at 1m), positive net
  of fees.

**Phase 7 — Statistical / model alpha (Weeks 17-20)**
- `signals::Models` per asset class: sports (elo + injuries + market priors),
  elections (poll aggregation + fundamentals), macro (term-structure /
  surprise indices), weather (NWS ensemble means).
- `stat-trader`: take resting Kalshi quotes when model probability differs
  from market by > calibrated threshold, sized by Kelly-fraction.
- Acceptance: positive PnL net of fees over 30 trading days, drawdown
  within limit.

**Phase 8 — Hardening & scaling (ongoing)**
- Move to bare-metal in same AWS AZ as Kalshi matching engine (or upgrade
  to Equinix colo).
- Profile and remove allocations from hot path (bumpalo arenas).
- Add per-strategy A/B harness for parameter changes.
- Quarterly red-team exercise: simulated exchange outage, simulated bad
  fill, simulated runaway strategy.

## Critical files / modules

Greenfield, so all paths are new. Anchors to design first because every
later module depends on them:

- [`crates/core/src/`](../crates/core/src/) — `Price`, `Qty`, `Market`,
  `Order`, `Fill`, `fees`. Get fixed-point math and serialization right
  once. (**Phase 0 — done.**)
- [`crates/book/src/lib.rs`](../crates/book/src/lib.rs) — order book
  snapshot/delta engine with sequence-gap detection. (**Phase 1 — done.**)
- [`crates/kalshi-rest/src/`](../crates/kalshi-rest/src/) — RSA-PSS auth +
  REST client. (**Phase 1 — done.**)
- `crates/kalshi-md/src/` — WS feed handler. (**Phase 1 — pending.**)
- `crates/oms/src/state_machine.rs` — order lifecycle. Single most important
  file in the codebase for correctness.
- `crates/risk/src/checks.rs` — every limit. Two-person-review required.
- `crates/strategy/src/lib.rs` — `Strategy` trait + `Intent` enum. Defines
  the contract every bot speaks.
- `crates/sim/src/engine.rs` — replay + matching. Must use the exact same
  OMS code as live (no mocks).

## Verification

End-to-end correctness gates (each must pass before the next phase is
declared done):

1. `cargo test --workspace` + `cargo clippy --all-targets -- -D warnings`
   green in CI.
2. **Book reconciliation test**: replay 24h of recorded WS, assert
   reconstructed book == snapshot every hour, zero diffs.
3. **OMS property tests**: `proptest` invariants — no double-fill, no
   negative position, idempotent client IDs, kill switch flattens within
   1s.
4. **Sim ↔ live parity**: run a week live with $500 cap, replay through sim,
   PnL within 10%.
5. **Risk fuzz**: send adversarial intents (huge size, negative px,
   duplicate cid, post-cancel-modify) — risk module must reject every
   one.
6. **Game-day drill** before each new strategy goes live: kill switch,
   exchange disconnect, position-mismatch, runaway-loop scenarios. All must
   auto-mitigate within 5s.
7. **PnL attribution daily report**: per strategy, per market — realized,
   unrealized, fees, rebates, mark-out at 10s/1m/1h. Anything inexplicable
   triggers a halt.

## Open Questions

- Whether to apply for the Kalshi formal MM designation (extra rebates, but
  quoting obligations) — answered when account ≥ $25k.
- Sports-data feed budget (SportsRadar is the difference between competing
  and not competing in sports) — answered when account ≥ $250k.
- Whether to pursue CME co-location for true HFT macro alpha or stay
  AWS-only — answered when latency edge is measured > 10ms.
- Entity formation (LLC vs personal) before scaling capital — talk to
  trading attorney once account ≥ $25k.
