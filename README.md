# predigy

Automated trading system for Kalshi prediction markets.

## Status

Phase 0 — workspace scaffold. Implementation roadmap lives in
`/root/.claude/plans/what-do-we-need-keen-church.md`.

## Layout

```
crates/
  core/        types: Px, Qty, Side, Market, Order, Fill, Position, fees
```

Subsequent phases will add: `kalshi-md`, `kalshi-rest`, `kalshi-exec`,
`poly-md`, `book`, `oms`, `risk`, `strategy`, `signals`, `sim`, `store`,
`ops`, `ext-feeds`.

## Build

```
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
