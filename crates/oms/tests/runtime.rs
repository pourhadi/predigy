//! End-to-end runtime tests for the OMS.
//!
//! Each test spins up the [`Oms`] task with a [`StubExecutor`] +
//! paired report channel, drives a scenario, and asserts on the
//! resulting events / position state.

use predigy_core::fill::Fill;
use predigy_core::intent::Intent;
use predigy_core::market::MarketTicker;
use predigy_core::order::OrderId;
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};
use predigy_oms::{
    ExecutionReport, ExecutionReportKind, Oms, OmsConfig, OmsError, OmsEvent, StubCall,
    stub_channel,
};
use predigy_risk::{Limits, PerMarketLimits, RiskEngine};
use std::collections::HashMap;
use std::time::Duration;

fn p(c: u8) -> Price {
    Price::from_cents(c).unwrap()
}

fn q(n: u32) -> Qty {
    Qty::new(n).unwrap()
}

fn buy_yes(market: &str, price: u8, qty: u32) -> Intent {
    Intent::limit(
        MarketTicker::new(market),
        Side::Yes,
        Action::Buy,
        p(price),
        q(qty),
    )
}

fn sell_yes(market: &str, price: u8, qty: u32) -> Intent {
    Intent::limit(
        MarketTicker::new(market),
        Side::Yes,
        Action::Sell,
        p(price),
        q(qty),
    )
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

async fn next_event(oms: &mut predigy_oms::OmsHandle) -> OmsEvent {
    tokio::time::timeout(Duration::from_secs(5), oms.next_event())
        .await
        .expect("event in time")
        .expect("stream not closed")
}

fn fill(price: u8, qty: u32, maker: bool) -> Fill {
    Fill {
        order_id: OrderId::new("ignored-by-oms"),
        market: MarketTicker::new("ignored"),
        side: Side::Yes,
        action: Action::Buy,
        price: p(price),
        qty: q(qty),
        ts_ms: 0,
        is_maker: maker,
        fee_cents: 0,
    }
}

#[tokio::test]
async fn submit_ack_fill_updates_position() {
    let (executor, report_tx, report_rx) = stub_channel(64);
    let mut oms = Oms::spawn(
        OmsConfig {
            strategy_id: "arb".into(),
            cid_backing: predigy_oms::CidBacking::InMemory { start_seq: 0 },
        },
        RiskEngine::new(permissive_limits()),
        executor.clone(),
        report_rx,
    );

    // Submit returns the cid synchronously.
    let cid = oms.submit(buy_yes("X", 42, 100)).await.expect("submitted");
    assert_eq!(cid.as_str(), "arb:X:00000000");
    assert_eq!(executor.calls().len(), 1);
    assert!(matches!(executor.calls()[0], StubCall::Submit(_)));

    // First event: Submitted.
    match next_event(&mut oms).await {
        OmsEvent::Submitted { cid: c, .. } => assert_eq!(c, cid),
        other => panic!("expected Submitted, got {other:?}"),
    }

    // Push an Acked report.
    report_tx
        .send(ExecutionReport {
            cid: cid.clone(),
            ts_ms: 1,
            kind: ExecutionReportKind::Acked {
                venue_order_id: "V-1".into(),
            },
        })
        .await
        .unwrap();
    match next_event(&mut oms).await {
        OmsEvent::Acked {
            cid: c,
            venue_order_id,
        } => {
            assert_eq!(c, cid);
            assert_eq!(venue_order_id, "V-1");
        }
        other => panic!("expected Acked, got {other:?}"),
    }

    // Push a Filled report — full fill at 41¢ (better than the 42 limit).
    report_tx
        .send(ExecutionReport {
            cid: cid.clone(),
            ts_ms: 2,
            kind: ExecutionReportKind::Filled {
                fill: fill(41, 100, true),
                cumulative_qty: 100,
            },
        })
        .await
        .unwrap();
    match next_event(&mut oms).await {
        OmsEvent::Filled {
            cid: c,
            delta_qty,
            cumulative_qty,
            fill_price,
        } => {
            assert_eq!(c, cid);
            assert_eq!(delta_qty, 100);
            assert_eq!(cumulative_qty, 100);
            assert_eq!(fill_price.cents(), 41);
        }
        other => panic!("expected Filled, got {other:?}"),
    }
    match next_event(&mut oms).await {
        OmsEvent::PositionUpdated {
            market,
            side,
            new_qty,
            new_avg_entry_cents,
            realized_pnl_delta_cents,
        } => {
            assert_eq!(market, MarketTicker::new("X"));
            assert_eq!(side, Side::Yes);
            assert_eq!(new_qty, 100);
            assert_eq!(new_avg_entry_cents, 41);
            assert_eq!(realized_pnl_delta_cents, 0);
        }
        other => panic!("expected PositionUpdated, got {other:?}"),
    }

    oms.close().await;
}

#[tokio::test]
async fn risk_rejection_does_not_submit() {
    let (executor, _report_tx, report_rx) = stub_channel(8);
    // Tighten position cap to 50.
    let limits = Limits {
        per_market: PerMarketLimits {
            max_contracts_per_side: 50,
            max_notional_cents_per_side: 1_000_000,
        },
        ..Limits::default()
    };
    let oms = Oms::spawn(
        OmsConfig::default(),
        RiskEngine::new(limits),
        executor.clone(),
        report_rx,
    );

    // 100 > 50 → risk rejects.
    let err = oms.submit(buy_yes("X", 42, 100)).await.unwrap_err();
    assert!(matches!(err, OmsError::RiskRejected(_)), "got {err:?}");
    // Executor never saw the order.
    assert!(executor.calls().is_empty());
    oms.close().await;
}

#[tokio::test]
async fn partial_fill_followed_by_terminal_fill_blends_vwap() {
    let (executor, report_tx, report_rx) = stub_channel(64);
    let mut oms = Oms::spawn(
        OmsConfig::default(),
        RiskEngine::new(permissive_limits()),
        executor,
        report_rx,
    );
    let cid = oms.submit(buy_yes("X", 50, 100)).await.unwrap();
    let _ = next_event(&mut oms).await; // Submitted

    // 40 @ 41¢, then 60 more @ 42¢ → terminal cum 100. VWAP =
    // (40*41 + 60*42)/100 = 4160/100 = 41.6 → 42 (round).
    report_tx
        .send(ExecutionReport {
            cid: cid.clone(),
            ts_ms: 1,
            kind: ExecutionReportKind::PartiallyFilled {
                fill: fill(41, 40, true),
                cumulative_qty: 40,
            },
        })
        .await
        .unwrap();
    let _ = next_event(&mut oms).await; // PartiallyFilled
    let _ = next_event(&mut oms).await; // PositionUpdated (40 @ 41)

    report_tx
        .send(ExecutionReport {
            cid: cid.clone(),
            ts_ms: 2,
            kind: ExecutionReportKind::Filled {
                fill: fill(42, 60, true),
                cumulative_qty: 100,
            },
        })
        .await
        .unwrap();
    let _filled = next_event(&mut oms).await;
    match next_event(&mut oms).await {
        OmsEvent::PositionUpdated {
            new_qty,
            new_avg_entry_cents,
            ..
        } => {
            assert_eq!(new_qty, 100);
            // (40*41 + 60*42) / 100 = 41.6, rounded to 42.
            assert_eq!(new_avg_entry_cents, 42);
        }
        other => panic!("expected PositionUpdated, got {other:?}"),
    }
    oms.close().await;
}

#[tokio::test]
async fn sell_after_buy_realises_pnl() {
    let (executor, report_tx, report_rx) = stub_channel(64);
    let mut oms = Oms::spawn(
        OmsConfig::default(),
        RiskEngine::new(permissive_limits()),
        executor,
        report_rx,
    );

    // Buy 100 @ 40¢, fully filled.
    let cid_buy = oms.submit(buy_yes("X", 40, 100)).await.unwrap();
    let _ = next_event(&mut oms).await; // Submitted
    report_tx
        .send(ExecutionReport {
            cid: cid_buy,
            ts_ms: 1,
            kind: ExecutionReportKind::Filled {
                fill: fill(40, 100, true),
                cumulative_qty: 100,
            },
        })
        .await
        .unwrap();
    let _ = next_event(&mut oms).await; // Filled
    let _ = next_event(&mut oms).await; // PositionUpdated

    // Sell 30 @ 50¢ — realises +30 × (50−40) = +300¢.
    let cid_sell = oms.submit(sell_yes("X", 50, 30)).await.unwrap();
    let _ = next_event(&mut oms).await; // Submitted
    report_tx
        .send(ExecutionReport {
            cid: cid_sell,
            ts_ms: 2,
            kind: ExecutionReportKind::Filled {
                fill: fill(50, 30, true),
                cumulative_qty: 30,
            },
        })
        .await
        .unwrap();
    let _ = next_event(&mut oms).await; // Filled
    match next_event(&mut oms).await {
        OmsEvent::PositionUpdated {
            new_qty,
            new_avg_entry_cents,
            realized_pnl_delta_cents,
            ..
        } => {
            assert_eq!(new_qty, 70);
            assert_eq!(new_avg_entry_cents, 40); // Sells don't move VWAP.
            assert_eq!(realized_pnl_delta_cents, 300);
        }
        other => panic!("expected PositionUpdated, got {other:?}"),
    }
    oms.close().await;
}

#[tokio::test]
async fn cancel_marks_record_and_calls_executor() {
    let (executor, report_tx, report_rx) = stub_channel(64);
    let mut oms = Oms::spawn(
        OmsConfig::default(),
        RiskEngine::new(permissive_limits()),
        executor.clone(),
        report_rx,
    );
    let cid = oms.submit(buy_yes("X", 42, 100)).await.unwrap();
    let _ = next_event(&mut oms).await; // Submitted

    // Cancel.
    oms.cancel(cid.clone()).await.expect("cancel queued");
    // Stub recorded the cancel call.
    let calls = executor.calls();
    assert!(matches!(calls.last(), Some(StubCall::Cancel(c)) if *c == cid));

    // Server confirms with a Cancelled report.
    report_tx
        .send(ExecutionReport {
            cid: cid.clone(),
            ts_ms: 1,
            kind: ExecutionReportKind::Cancelled {
                reason: "user".into(),
            },
        })
        .await
        .unwrap();
    match next_event(&mut oms).await {
        OmsEvent::Cancelled { cid: c, reason } => {
            assert_eq!(c, cid);
            assert_eq!(reason, "user");
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
    oms.close().await;
}

#[tokio::test]
async fn kill_switch_blocks_new_submits() {
    let (executor, _report_tx, report_rx) = stub_channel(8);
    let mut oms = Oms::spawn(
        OmsConfig::default(),
        RiskEngine::new(permissive_limits()),
        executor.clone(),
        report_rx,
    );

    oms.arm_kill_switch().await.unwrap();
    // Drain the KillSwitchArmed event before re-entering submit.
    match next_event(&mut oms).await {
        OmsEvent::KillSwitchArmed => {}
        other => panic!("expected KillSwitchArmed, got {other:?}"),
    }

    let err = oms.submit(buy_yes("X", 42, 1)).await.unwrap_err();
    assert!(matches!(err, OmsError::KillSwitch));
    assert!(executor.calls().is_empty(), "executor should not be called");

    // Disarm and try again — succeeds.
    oms.disarm_kill_switch().await.unwrap();
    let _ = next_event(&mut oms).await; // KillSwitchDisarmed
    let cid = oms
        .submit(buy_yes("X", 42, 1))
        .await
        .expect("ok after disarm");
    assert!(!cid.as_str().is_empty());
    oms.close().await;
}

#[tokio::test]
async fn arm_kill_switch_cancels_live_orders() {
    use predigy_oms::StubCall;

    let (executor, report_tx, report_rx) = stub_channel(64);
    let mut oms = Oms::spawn(
        OmsConfig::default(),
        RiskEngine::new(permissive_limits()),
        executor.clone(),
        report_rx,
    );

    // Submit two orders and have the venue ack them so the OMS knows
    // about live working orders.
    let cid_a = oms.submit(buy_yes("X", 40, 10)).await.unwrap();
    let cid_b = oms.submit(buy_yes("X", 41, 10)).await.unwrap();
    let _ = next_event(&mut oms).await; // Submitted A
    let _ = next_event(&mut oms).await; // Submitted B
    report_tx
        .send(ExecutionReport {
            cid: cid_a.clone(),
            ts_ms: 1,
            kind: ExecutionReportKind::Acked {
                venue_order_id: "V-A".into(),
            },
        })
        .await
        .unwrap();
    report_tx
        .send(ExecutionReport {
            cid: cid_b.clone(),
            ts_ms: 2,
            kind: ExecutionReportKind::Acked {
                venue_order_id: "V-B".into(),
            },
        })
        .await
        .unwrap();
    let _ = next_event(&mut oms).await;
    let _ = next_event(&mut oms).await;

    // Arm the kill switch — should issue cancels for both live orders
    // before emitting KillSwitchArmed.
    oms.arm_kill_switch().await.unwrap();
    match next_event(&mut oms).await {
        OmsEvent::KillSwitchArmed => {}
        other => panic!("expected KillSwitchArmed, got {other:?}"),
    }

    // Stub recorded two Cancel calls — order is map-iteration so we
    // assert as a set.
    let cancelled: std::collections::HashSet<_> = executor
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            StubCall::Cancel(cid) => Some(cid),
            StubCall::Submit(_) => None,
        })
        .collect();
    assert_eq!(cancelled.len(), 2);
    assert!(cancelled.contains(&cid_a));
    assert!(cancelled.contains(&cid_b));
    oms.close().await;
}

#[tokio::test]
async fn reconcile_flags_position_mismatch() {
    let (executor, report_tx, report_rx) = stub_channel(64);
    let mut oms = Oms::spawn(
        OmsConfig::default(),
        RiskEngine::new(permissive_limits()),
        executor,
        report_rx,
    );

    // Buy 100 + fill so the OMS books a position.
    let cid = oms.submit(buy_yes("X", 40, 100)).await.unwrap();
    let _ = next_event(&mut oms).await; // Submitted
    report_tx
        .send(ExecutionReport {
            cid,
            ts_ms: 1,
            kind: ExecutionReportKind::Filled {
                fill: fill(40, 100, true),
                cumulative_qty: 100,
            },
        })
        .await
        .unwrap();
    let _ = next_event(&mut oms).await; // Filled
    let _ = next_event(&mut oms).await; // PositionUpdated

    // Venue says we hold 95 (5-contract drift).
    let mut venue: HashMap<(MarketTicker, Side), u32> = HashMap::new();
    venue.insert((MarketTicker::new("X"), Side::Yes), 95);
    oms.reconcile(venue).await.unwrap();
    match next_event(&mut oms).await {
        OmsEvent::Reconciled { mismatches } => {
            assert_eq!(mismatches.len(), 1);
            let mm = &mismatches[0];
            assert_eq!(mm.market, MarketTicker::new("X"));
            assert_eq!(mm.side, Side::Yes);
            assert_eq!(mm.oms_qty, 100);
            assert_eq!(mm.venue_qty, 95);
        }
        other => panic!("expected Reconciled, got {other:?}"),
    }

    // Reconcile when the venue agrees — no mismatches.
    let mut venue_agree: HashMap<(MarketTicker, Side), u32> = HashMap::new();
    venue_agree.insert((MarketTicker::new("X"), Side::Yes), 100);
    oms.reconcile(venue_agree).await.unwrap();
    match next_event(&mut oms).await {
        OmsEvent::Reconciled { mismatches } => assert!(mismatches.is_empty()),
        other => panic!("expected Reconciled (empty), got {other:?}"),
    }
    oms.close().await;
}

#[tokio::test]
async fn executor_submit_failure_does_not_record_order() {
    use predigy_oms::ExecutorError;

    let (executor, _report_tx, report_rx) = stub_channel(8);
    let oms = Oms::spawn(
        OmsConfig::default(),
        RiskEngine::new(permissive_limits()),
        executor.clone(),
        report_rx,
    );
    executor.fail_next_submit(ExecutorError::Transport("simulated".into()));

    let err = oms.submit(buy_yes("X", 42, 1)).await.unwrap_err();
    match err {
        OmsError::Executor(msg) => assert!(msg.contains("simulated"), "got: {msg}"),
        other => panic!("expected Executor err, got {other:?}"),
    }
    oms.close().await;
}
