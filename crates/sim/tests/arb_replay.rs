//! End-to-end Phase 3 test: replay a synthetic NDJSON stream through
//! the full live-shaped pipeline (`Replay` → `OrderBook` →
//! `ArbStrategy` → `Oms` → `SimExecutor`) and assert on the
//! resulting OMS events + book state.
//!
//! This is the strongest "strategies run unchanged in sim and live"
//! claim we can make in unit-testable form: `ArbStrategy` is the
//! same code that `bin/arb-trader` runs in production; the
//! integration plumbing here only differs in which `Executor` impl
//! the OMS holds.

use arb_trader::strategy::{ArbConfig, ArbStrategy};
use md_recorder::{RecordedEvent, RecordedKind};
use predigy_book::Snapshot;
use predigy_core::market::MarketTicker;
use predigy_core::price::Price;
use predigy_core::side::Side;
use predigy_oms::{Oms, OmsConfig, OmsEvent};
use predigy_risk::{Limits, PerMarketLimits, RiskEngine};
use predigy_sim::{BookStore, Replay, ReplayUpdate, SimExecutor};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::BufReader;
use tokio::sync::Mutex;

fn p(c: u8) -> Price {
    Price::from_cents(c).unwrap()
}

fn permissive_limits() -> Limits {
    Limits {
        per_market: PerMarketLimits {
            max_contracts_per_side: 10_000,
            max_notional_cents_per_side: 1_000_000,
        },
        ..Limits::default()
    }
}

fn line(event: &RecordedEvent) -> String {
    let mut s = serde_json::to_string(event).unwrap();
    s.push('\n');
    s
}

/// Synthetic stream:
///   1. Snapshot — both sides empty → no arb possible.
///   2. Snapshot — YES bid 60×100, NO bid 50×100 → asks 50/40, sum 90
///      → 10¢/pair pre-fee, ~3¢/pair post-fee on size 25 → arb fires.
fn build_arb_payload() -> Vec<u8> {
    let snap_empty = RecordedEvent::new(
        1,
        "kalshi",
        RecordedKind::Snapshot {
            sid: 1,
            market: "X".into(),
            snapshot: Snapshot {
                seq: 1,
                yes_bids: vec![],
                no_bids: vec![],
            },
        },
    );
    let snap_arb = RecordedEvent::new(
        2,
        "kalshi",
        RecordedKind::Snapshot {
            sid: 1,
            market: "X".into(),
            snapshot: Snapshot {
                seq: 2,
                yes_bids: vec![(p(60), 100)],
                no_bids: vec![(p(50), 100)],
            },
        },
    );
    let mut payload = String::new();
    payload.push_str(&line(&snap_empty));
    payload.push_str(&line(&snap_arb));
    payload.into_bytes()
}

/// Build the OMS+SimExecutor stack and a strategy instance the test
/// can hand to the replay closure. Returns the `BookStore` (cloned
/// into the executor and the replay), the OMS in an `Arc<Mutex<>>`
/// (so the closure can call `.submit().await` while `&self` is
/// shared), and the strategy.
fn build_pipeline() -> (
    BookStore,
    Arc<Mutex<predigy_oms::OmsHandle>>,
    Arc<std::sync::Mutex<ArbStrategy>>,
) {
    let store = BookStore::new();
    let (executor, reports) = SimExecutor::spawn(store.clone());
    let oms = Oms::spawn(
        OmsConfig {
            strategy_id: "arb".into(),
            start_cid_seq: 0,
        },
        RiskEngine::new(permissive_limits()),
        executor,
        reports,
    );
    let strategy = Arc::new(std::sync::Mutex::new(ArbStrategy::new(ArbConfig {
        min_edge_cents: 1,
        max_size_per_pair: 25,
        cooldown: Duration::from_millis(1),
    })));
    (store, Arc::new(Mutex::new(oms)), strategy)
}

/// Drain OMS events until a `PositionUpdated` lands on each side of
/// "X" or the deadline expires. Returns the observed
/// `(yes_qty, yes_avg, no_qty, no_avg)`.
async fn drain_position_updates(
    oms: &mut predigy_oms::OmsHandle,
    deadline: tokio::time::Instant,
) -> (u32, u16, u32, u16) {
    let mut yes_qty = 0u32;
    let mut no_qty = 0u32;
    let mut yes_avg = 0u16;
    let mut no_avg = 0u16;
    while yes_qty == 0 || no_qty == 0 {
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        if timeout.is_zero() {
            break;
        }
        let Ok(Some(ev)) = tokio::time::timeout(timeout, oms.next_event()).await else {
            break;
        };
        if let OmsEvent::PositionUpdated {
            market,
            side,
            new_qty,
            new_avg_entry_cents,
            ..
        } = ev
        {
            assert_eq!(market, MarketTicker::new("X"));
            match side {
                Side::Yes => {
                    yes_qty = new_qty;
                    yes_avg = new_avg_entry_cents;
                }
                Side::No => {
                    no_qty = new_qty;
                    no_avg = new_avg_entry_cents;
                }
            }
        }
    }
    (yes_qty, yes_avg, no_qty, no_avg)
}

async fn drive_strategy_replay(
    payload: Vec<u8>,
    store: BookStore,
    oms_arc: Arc<Mutex<predigy_oms::OmsHandle>>,
    strategy: Arc<std::sync::Mutex<ArbStrategy>>,
) {
    let replay = Replay::new(store.clone());
    replay
        .drive_lines(
            BufReader::new(std::io::Cursor::new(payload)),
            move |update| {
                let strategy_cb = strategy.clone();
                let store_cb = store.clone();
                let oms_cb = oms_arc.clone();
                Box::pin(async move {
                    let ReplayUpdate::BookUpdated(market) = update else {
                        return;
                    };
                    let intents = {
                        let mut s = strategy_cb.lock().unwrap();
                        store_cb
                            .with_book(&market, |book| {
                                book.map(|b| s.evaluate(&market, b, Instant::now()))
                            })
                            .map(|ev| ev.intents)
                            .unwrap_or_default()
                    };
                    if intents.is_empty() {
                        return;
                    }
                    let oms_g = oms_cb.lock().await;
                    for intent in intents {
                        let _ = oms_g.submit(intent).await;
                    }
                }) as Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn arb_strategy_books_position_against_replayed_book() {
    let payload = build_arb_payload();
    let (store, oms_arc, strategy) = build_pipeline();

    drive_strategy_replay(payload, store.clone(), oms_arc.clone(), strategy).await;

    // Reclaim sole ownership of the OMS for the event drain.
    let mut oms = Arc::try_unwrap(oms_arc)
        .expect("only the test holds the OMS reference here")
        .into_inner();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let (yes_qty, yes_avg, no_qty, no_avg) = drain_position_updates(&mut oms, deadline).await;

    assert_eq!(yes_qty, 25, "YES leg should fill 25 contracts");
    assert_eq!(no_qty, 25, "NO leg should fill 25 contracts");
    assert_eq!(yes_avg, 50);
    assert_eq!(no_avg, 40);

    // Book consumed: 100 − 25 = 75 left at each touch level.
    store.with_book(&MarketTicker::new("X"), |b| {
        let b = b.expect("book exists");
        assert_eq!(b.best_yes_bid().unwrap().1, 75);
        assert_eq!(b.best_no_bid().unwrap().1, 75);
    });

    oms.close().await;
}
