//! Integration tests for the DB-backed OMS against a live
//! Postgres test DB.
//!
//! Connection: defaults to `postgresql:///predigy_test`. Override
//! via `TEST_DATABASE_URL`. The DB MUST already exist + have the
//! schema applied (one-time setup):
//!
//! ```bash
//! createdb predigy_test
//! psql -U dan -d predigy_test -f migrations/0001_initial.sql
//! ```
//!
//! Each test runs in its own transaction-ish isolation by:
//! truncating positions / intents / fills / intent_events at
//! the start. Tests within a single file run serially (cargo
//! test default for same-file integration tests is parallel,
//! but the truncations would race; we use a serial mutex via
//! the `serial_test`-like pattern: each test acquires a global
//! tokio::sync::Mutex before mutating).

use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine::oms_db::{DbBackedOms, EngineMode};
use predigy_engine_core::intent::{Intent, IntentAction, LegGroup, OrderType, Tif};
use predigy_engine_core::oms::{
    ExecutionStatus, ExecutionUpdate, KillSwitchView, Oms, RejectionReason, RiskCaps,
    SubmitGroupOutcome, SubmitOutcome,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

fn test_database_url() -> String {
    std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| "postgresql:///predigy_test".into())
}

// Global mutex to serialise tests against the shared DB.
async fn test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::OnceCell<AsyncMutex<()>> = tokio::sync::OnceCell::const_new();
    LOCK.get_or_init(|| async { AsyncMutex::new(()) })
        .await
        .lock()
        .await
}

async fn fresh_pool() -> PgPool {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_database_url())
        .await
        .expect("connect to test db");
    // Wipe state. Order matters for FK refs.
    for tbl in [
        "intent_events",
        "fills",
        "positions",
        "intents",
        "model_p_snapshots",
        "model_p_inputs",
        "rules",
        "calibration",
        "settlements",
        "book_snapshots",
        "kill_switches",
        "markets",
    ] {
        sqlx::query(&format!("TRUNCATE TABLE {tbl} RESTART IDENTITY CASCADE"))
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("truncate {tbl}: {e}"));
    }
    pool
}

async fn ensure_market(pool: &PgPool, ticker: &str) {
    sqlx::query(
        "INSERT INTO markets (ticker, venue, market_type)
         VALUES ($1, 'kalshi', 'binary') ON CONFLICT (ticker) DO NOTHING",
    )
    .bind(ticker)
    .execute(pool)
    .await
    .unwrap();
}

fn buy_yes(client_id: &str, ticker: &str, qty: i32, price_cents: i32) -> Intent {
    Intent {
        client_id: client_id.into(),
        strategy: "test",
        market: MarketTicker::new(ticker),
        side: Side::Yes,
        action: IntentAction::Buy,
        price_cents: Some(price_cents),
        qty,
        order_type: OrderType::Limit,
        tif: Tif::Ioc,
        reason: Some("integration test".into()),
    }
}

fn sell_yes(client_id: &str, ticker: &str, qty: i32, price_cents: i32) -> Intent {
    Intent {
        client_id: client_id.into(),
        strategy: "test",
        market: MarketTicker::new(ticker),
        side: Side::Yes,
        action: IntentAction::Sell,
        price_cents: Some(price_cents),
        qty,
        order_type: OrderType::Limit,
        tif: Tif::Ioc,
        reason: Some("integration test".into()),
    }
}

fn permissive_caps() -> RiskCaps {
    RiskCaps {
        max_notional_cents: 1_000_000,
        // 0 disables; existing integration tests don't exercise
        // the global cap. The dedicated global-cap test below
        // overrides it explicitly.
        max_global_notional_cents: 0,
        max_daily_loss_cents: 1_000_000,
        max_contracts_per_side: 1000,
        max_in_flight: 1000,
        max_orders_per_window: 1000,
        rate_window_ms: 1000,
    }
}

#[tokio::test]
async fn submit_persists_intent_and_emits_event() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-A").await;

    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);
    let outcome = oms
        .submit(buy_yes("test:A:0001", "KX-INT-A", 1, 30))
        .await
        .unwrap();
    match outcome {
        SubmitOutcome::Submitted { client_id, .. } => assert_eq!(client_id, "test:A:0001"),
        other => panic!("expected Submitted, got {other:?}"),
    }
    let row: (String, i32, String) =
        sqlx::query_as("SELECT status, qty, ticker FROM intents WHERE client_id = $1")
            .bind("test:A:0001")
            .fetch_one(&pool)
            .await
            .unwrap();
    // Shadow mode is the default; engine never reaches the venue,
    // so the intent stays at 'shadow'. Tests that verify the live
    // venue path use new_with_mode(EngineMode::Live).
    assert_eq!(row.0, "shadow");
    assert_eq!(row.1, 1);
    assert_eq!(row.2, "KX-INT-A");

    let evs: (i64,) =
        sqlx::query_as("SELECT COUNT(*)::BIGINT FROM intent_events WHERE client_id = $1")
            .bind("test:A:0001")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(evs.0, 1);
}

#[tokio::test]
async fn duplicate_client_id_returns_idempotent() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-B").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool, permissive_caps(), ks);

    let intent = buy_yes("test:B:0001", "KX-INT-B", 1, 30);
    let first = oms.submit(intent.clone()).await.unwrap();
    matches!(first, SubmitOutcome::Submitted { .. });
    let second = oms.submit(intent).await.unwrap();
    match second {
        SubmitOutcome::Idempotent {
            client_id,
            current_status,
        } => {
            assert_eq!(client_id, "test:B:0001");
            // Shadow-mode default; the first submit landed at
            // status='shadow' so the second sees 'shadow'.
            assert_eq!(current_status, "shadow");
        }
        other => panic!("expected Idempotent, got {other:?}"),
    }
}

#[tokio::test]
async fn kill_switch_armed_rejects() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-C").await;
    let ks = Arc::new(KillSwitchView::new());
    ks.arm();
    let oms = DbBackedOms::new(pool, permissive_caps(), ks);

    let outcome = oms
        .submit(buy_yes("test:C:0001", "KX-INT-C", 1, 30))
        .await
        .unwrap();
    match outcome {
        SubmitOutcome::Rejected {
            reason: RejectionReason::KillSwitchArmed { .. },
        } => {}
        other => panic!("expected KillSwitchArmed, got {other:?}"),
    }
}

#[tokio::test]
async fn contract_cap_rejects() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-D").await;
    let ks = Arc::new(KillSwitchView::new());
    let mut caps = permissive_caps();
    caps.max_contracts_per_side = 2;
    let oms = DbBackedOms::new(pool, caps, ks);

    // 3 contracts on a side capped at 2.
    let outcome = oms
        .submit(buy_yes("test:D:0001", "KX-INT-D", 3, 30))
        .await
        .unwrap();
    match outcome {
        SubmitOutcome::Rejected {
            reason: RejectionReason::ContractCapExceeded { current, limit, .. },
        } => {
            assert_eq!(current, 3);
            assert_eq!(limit, 2);
        }
        other => panic!("expected ContractCapExceeded, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_client_id_rejects() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-E").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool, permissive_caps(), ks);

    let outcome = oms.submit(buy_yes("", "KX-INT-E", 1, 30)).await.unwrap();
    match outcome {
        SubmitOutcome::Rejected {
            reason: RejectionReason::InvalidIntent { .. },
        } => {}
        other => panic!("expected InvalidIntent, got {other:?}"),
    }
}

#[tokio::test]
async fn negative_qty_rejects() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-F").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool, permissive_caps(), ks);

    let outcome = oms
        .submit(buy_yes("test:F:0001", "KX-INT-F", 0, 30))
        .await
        .unwrap();
    match outcome {
        SubmitOutcome::Rejected {
            reason: RejectionReason::InvalidIntent { .. },
        } => {}
        other => panic!("expected InvalidIntent, got {other:?}"),
    }
}

#[tokio::test]
async fn execution_filled_creates_position_and_fill() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-G").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    let cid = "test:G:0001";
    oms.submit(buy_yes(cid, "KX-INT-G", 5, 30)).await.unwrap();
    oms.apply_execution(ExecutionUpdate {
        client_id: cid.into(),
        venue_order_id: Some("venue-G".into()),
        venue_fill_id: Some("unique-fill-0001".into()),
        status: ExecutionStatus::Filled,
        cumulative_qty: 5,
        avg_fill_price_cents: Some(28),
        last_fill_qty: Some(5),
        last_fill_price_cents: Some(28),
        last_fill_fee_cents: Some(2),
        venue_payload: serde_json::json!({"raw": "test"}),
    })
    .await
    .unwrap();

    // Position created.
    let pos: (i32, i32, i64) = sqlx::query_as(
        "SELECT current_qty, avg_entry_cents, fees_paid_cents
           FROM positions
          WHERE strategy = 'test' AND ticker = 'KX-INT-G' AND closed_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pos.0, 5);
    assert_eq!(pos.1, 28);
    assert_eq!(pos.2, 2);

    // Fill row written.
    let fill: (i32, i32, i32) =
        sqlx::query_as("SELECT qty, price_cents, fee_cents FROM fills WHERE client_id = $1")
            .bind(cid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(fill.0, 5);
    assert_eq!(fill.1, 28);
    assert_eq!(fill.2, 2);

    // Intent updated to filled.
    let intent_status: (String,) =
        sqlx::query_as("SELECT status FROM intents WHERE client_id = $1")
            .bind(cid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(intent_status.0, "filled");
}

#[tokio::test]
async fn short_position_close_realised_pnl_correct_sign() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-S").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    // Open SHORT at 30¢ (sell to open).
    let open = Intent {
        client_id: "test:S:open".into(),
        strategy: "test",
        market: MarketTicker::new("KX-INT-S"),
        side: Side::Yes,
        action: IntentAction::Sell,
        price_cents: Some(30),
        qty: 5,
        order_type: OrderType::Limit,
        tif: Tif::Ioc,
        reason: None,
    };
    oms.submit(open).await.unwrap();
    oms.apply_execution(ExecutionUpdate {
        client_id: "test:S:open".into(),
        venue_order_id: Some("v-S1".into()),
        venue_fill_id: Some("unique-fill-0002".into()),
        status: ExecutionStatus::Filled,
        cumulative_qty: 5,
        avg_fill_price_cents: Some(30),
        last_fill_qty: Some(5),
        last_fill_price_cents: Some(30),
        last_fill_fee_cents: Some(0),
        venue_payload: serde_json::json!({}),
    })
    .await
    .unwrap();

    // Buy back at 20¢ (a profit on a short — bought lower than sold).
    oms.submit(buy_yes("test:S:close", "KX-INT-S", 5, 20))
        .await
        .unwrap();
    oms.apply_execution(ExecutionUpdate {
        client_id: "test:S:close".into(),
        venue_order_id: Some("v-S2".into()),
        venue_fill_id: Some("unique-fill-0003".into()),
        status: ExecutionStatus::Filled,
        cumulative_qty: 5,
        avg_fill_price_cents: Some(20),
        last_fill_qty: Some(5),
        last_fill_price_cents: Some(20),
        last_fill_fee_cents: Some(0),
        venue_payload: serde_json::json!({}),
    })
    .await
    .unwrap();

    // Short opened at 30, closed at 20 → profit per share 10¢ × 5 = 50¢.
    let pnl: (i64,) = sqlx::query_as(
        "SELECT realized_pnl_cents FROM positions
          WHERE strategy = 'test' AND ticker = 'KX-INT-S'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pnl.0, 50, "short close at lower price should be a profit");
}

#[tokio::test]
async fn partial_fill_then_full_fill_accumulates_position() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-P").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    oms.submit(buy_yes("test:P:0001", "KX-INT-P", 10, 30))
        .await
        .unwrap();
    // Partial fill: 4 contracts at 28¢.
    oms.apply_execution(ExecutionUpdate {
        client_id: "test:P:0001".into(),
        venue_order_id: Some("v-P".into()),
        venue_fill_id: Some("v-P-fill1".into()),
        status: ExecutionStatus::PartialFill,
        cumulative_qty: 4,
        avg_fill_price_cents: Some(28),
        last_fill_qty: Some(4),
        last_fill_price_cents: Some(28),
        last_fill_fee_cents: Some(0),
        venue_payload: serde_json::json!({}),
    })
    .await
    .unwrap();

    // Then 6 more at 32¢ (different price).
    oms.apply_execution(ExecutionUpdate {
        client_id: "test:P:0001".into(),
        venue_order_id: Some("v-P".into()),
        venue_fill_id: Some("v-P-fill2".into()),
        status: ExecutionStatus::Filled,
        cumulative_qty: 10,
        avg_fill_price_cents: Some(30),
        last_fill_qty: Some(6),
        last_fill_price_cents: Some(32),
        last_fill_fee_cents: Some(0),
        venue_payload: serde_json::json!({}),
    })
    .await
    .unwrap();

    let pos: (i32, i32) = sqlx::query_as(
        "SELECT current_qty, avg_entry_cents
           FROM positions
          WHERE strategy = 'test' AND ticker = 'KX-INT-P' AND closed_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pos.0, 10);
    // Weighted avg of 4@28 and 6@32 = (4*28 + 6*32)/10 = (112+192)/10 = 30.4 → 30 (integer).
    assert_eq!(pos.1, 30);

    // Two fill rows.
    let n_fills: (i64,) = sqlx::query_as("SELECT COUNT(*)::BIGINT FROM fills WHERE client_id = $1")
        .bind("test:P:0001")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n_fills.0, 2);
}

#[tokio::test]
async fn live_mode_submits_at_status_submitted() {
    // Explicit EngineMode::Live writes status='submitted' so the
    // venue-router worker can pick it up. Shadow mode (default)
    // writes 'shadow' instead.
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-LIVE").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new_with_mode(pool.clone(), permissive_caps(), ks, EngineMode::Live);

    oms.submit(buy_yes("test:LIVE:0001", "KX-INT-LIVE", 1, 30))
        .await
        .unwrap();
    let row: (String,) = sqlx::query_as("SELECT status FROM intents WHERE client_id = $1")
        .bind("test:LIVE:0001")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "submitted");

    // intent_events also reflects 'submitted'.
    let ev: (String,) = sqlx::query_as(
        "SELECT status FROM intent_events WHERE client_id = $1 ORDER BY ts DESC LIMIT 1",
    )
    .bind("test:LIVE:0001")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ev.0, "submitted");
}

#[tokio::test]
async fn cancel_marks_intent_and_appends_event() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-X").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    oms.submit(buy_yes("test:X:0001", "KX-INT-X", 1, 30))
        .await
        .unwrap();
    oms.cancel("test:X:0001").await.unwrap();
    let row: (String,) = sqlx::query_as("SELECT status FROM intents WHERE client_id = $1")
        .bind("test:X:0001")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "cancel_requested");
    let n: (i64,) =
        sqlx::query_as("SELECT COUNT(*)::BIGINT FROM intent_events WHERE client_id = $1")
            .bind("test:X:0001")
            .fetch_one(&pool)
            .await
            .unwrap();
    // submitted + cancel_requested = 2 events.
    assert_eq!(n.0, 2);
}

#[tokio::test]
async fn fill_then_close_settles_realised_pnl() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-H").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    // Open at 30¢.
    oms.submit(buy_yes("test:H:open", "KX-INT-H", 5, 30))
        .await
        .unwrap();
    oms.apply_execution(ExecutionUpdate {
        client_id: "test:H:open".into(),
        venue_order_id: Some("venue-H1".into()),
        venue_fill_id: Some("unique-fill-0004".into()),
        status: ExecutionStatus::Filled,
        cumulative_qty: 5,
        avg_fill_price_cents: Some(30),
        last_fill_qty: Some(5),
        last_fill_price_cents: Some(30),
        last_fill_fee_cents: Some(0),
        venue_payload: serde_json::json!({}),
    })
    .await
    .unwrap();

    // Close at 50¢ via a sell.
    let close = Intent {
        client_id: "test:H:close".into(),
        strategy: "test",
        market: MarketTicker::new("KX-INT-H"),
        side: Side::Yes,
        action: IntentAction::Sell,
        price_cents: Some(50),
        qty: 5,
        order_type: OrderType::Limit,
        tif: Tif::Ioc,
        reason: None,
    };
    oms.submit(close).await.unwrap();
    oms.apply_execution(ExecutionUpdate {
        client_id: "test:H:close".into(),
        venue_order_id: Some("venue-H2".into()),
        venue_fill_id: Some("unique-fill-0005".into()),
        status: ExecutionStatus::Filled,
        cumulative_qty: 5,
        avg_fill_price_cents: Some(50),
        last_fill_qty: Some(5),
        last_fill_price_cents: Some(50),
        last_fill_fee_cents: Some(0),
        venue_payload: serde_json::json!({}),
    })
    .await
    .unwrap();

    // Position closed; realised pnl = (50 - 30) * 5 = 100¢.
    let pos: (Option<chrono::DateTime<chrono::Utc>>, i64, i32) = sqlx::query_as(
        "SELECT closed_at, realized_pnl_cents, current_qty
           FROM positions
          WHERE strategy = 'test' AND ticker = 'KX-INT-H'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(pos.0.is_some(), "position should be closed");
    assert_eq!(pos.2, 0);
    assert_eq!(pos.1, 100);
}

#[tokio::test]
async fn duplicate_venue_fill_id_is_idempotent() {
    // The WS-push exec-data path (Phase 4a) can replay fills
    // across reconnects; the OMS must dedupe on venue_fill_id so
    // a replayed fill doesn't double-credit the position. Same
    // venue_fill_id arriving twice should leave exactly one row
    // in `fills` and one position update.
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-DUP").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    oms.submit(buy_yes("test:DUP:0001", "KX-INT-DUP", 3, 30))
        .await
        .unwrap();

    let fill = ExecutionUpdate {
        client_id: "test:DUP:0001".into(),
        venue_order_id: Some("venue-dup-1".into()),
        venue_fill_id: Some("trade-dup-A".into()),
        status: ExecutionStatus::Filled,
        cumulative_qty: 3,
        avg_fill_price_cents: Some(30),
        last_fill_qty: Some(3),
        last_fill_price_cents: Some(30),
        last_fill_fee_cents: Some(0),
        venue_payload: serde_json::json!({"source": "ws_push"}),
    };

    oms.apply_execution(fill.clone()).await.unwrap();
    // Replay the same fill (e.g. from a WS reconnect or a REST
    // belt-and-suspenders poller landing on the same trade_id).
    oms.apply_execution(fill).await.unwrap();

    // Exactly one row in `fills`.
    let fill_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*)::BIGINT FROM fills WHERE venue_fill_id = 'trade-dup-A'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        fill_count.0, 1,
        "duplicate fill must collapse to a single row"
    );

    // Position reflects a single 3-contract fill (not 6).
    let pos: (i32, i32) = sqlx::query_as(
        "SELECT current_qty, avg_entry_cents
           FROM positions
          WHERE strategy = 'test' AND ticker = 'KX-INT-DUP'
            AND closed_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pos.0, 3, "duplicate fill must not double the position");
    assert_eq!(pos.1, 30);

    let events: (i64,) = sqlx::query_as(
        "SELECT COUNT(*)::BIGINT FROM intent_events WHERE client_id = 'test:DUP:0001'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        events.0, 2,
        "duplicate fill must not append a lifecycle event"
    );
}

#[tokio::test]
async fn long_at_contract_cap_can_sell_to_close() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-CLOSE-LONG").await;
    let ks = Arc::new(KillSwitchView::new());
    let mut caps = permissive_caps();
    caps.max_contracts_per_side = 3;
    caps.max_notional_cents = 90;
    let oms = DbBackedOms::new(pool.clone(), caps, ks);

    sqlx::query(
        "INSERT INTO positions
            (strategy, ticker, side, current_qty, avg_entry_cents,
             fees_paid_cents, opened_at, last_fill_at)
         VALUES ('test', 'KX-INT-CLOSE-LONG', 'yes', 3, 30, 0, now(), now())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let outcome = oms
        .submit(sell_yes("test:CLOSE-LONG:0001", "KX-INT-CLOSE-LONG", 3, 50))
        .await
        .unwrap();
    assert!(
        matches!(outcome, SubmitOutcome::Submitted { .. }),
        "sell-to-close at cap should pass; got {outcome:?}"
    );
}

#[tokio::test]
async fn short_at_contract_cap_can_buy_to_close() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-CLOSE-SHORT").await;
    let ks = Arc::new(KillSwitchView::new());
    let mut caps = permissive_caps();
    caps.max_contracts_per_side = 3;
    caps.max_notional_cents = 90;
    let oms = DbBackedOms::new(pool.clone(), caps, ks);

    sqlx::query(
        "INSERT INTO positions
            (strategy, ticker, side, current_qty, avg_entry_cents,
             fees_paid_cents, opened_at, last_fill_at)
         VALUES ('test', 'KX-INT-CLOSE-SHORT', 'yes', -3, 30, 0, now(), now())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let outcome = oms
        .submit(buy_yes(
            "test:CLOSE-SHORT:0001",
            "KX-INT-CLOSE-SHORT",
            3,
            20,
        ))
        .await
        .unwrap();
    assert!(
        matches!(outcome, SubmitOutcome::Submitted { .. }),
        "buy-to-close at cap should pass; got {outcome:?}"
    );
}

#[tokio::test]
async fn naked_sell_is_modeled_as_short_and_respects_caps() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-NAKED").await;
    let ks = Arc::new(KillSwitchView::new());
    let mut caps = permissive_caps();
    caps.max_contracts_per_side = 2;
    let oms = DbBackedOms::new(pool.clone(), caps, ks);

    let rejected = oms
        .submit(sell_yes("test:NAKED:0001", "KX-INT-NAKED", 3, 40))
        .await
        .unwrap();
    assert!(matches!(
        rejected,
        SubmitOutcome::Rejected {
            reason: RejectionReason::ContractCapExceeded { .. }
        }
    ));

    let accepted = oms
        .submit(sell_yes("test:NAKED:0002", "KX-INT-NAKED", 2, 40))
        .await
        .unwrap();
    assert!(matches!(accepted, SubmitOutcome::Submitted { .. }));
}

#[tokio::test]
async fn global_notional_cap_blocks_cross_strategy_concentration() {
    // Phase 6.2 — the global notional cap should bind even when
    // each per-strategy cap individually has headroom. Setup:
    //   - per-strategy max_notional_cents: 1_000_000 (no bind)
    //   - global max_global_notional_cents: 200 (binds)
    //   - "stat" already holds $2 of open notional; engine
    //     attempts a second $1 trade on a different strategy.
    //   - Expected: rejected with NotionalExceeded scope='global'.
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-GLOBAL").await;
    let ks = Arc::new(KillSwitchView::new());
    let caps = RiskCaps {
        max_notional_cents: 1_000_000,
        max_global_notional_cents: 200, // $2 global cap
        max_daily_loss_cents: 1_000_000,
        max_contracts_per_side: 1000,
        max_in_flight: 1000,
        max_orders_per_window: 1000,
        rate_window_ms: 1000,
    };
    let oms = DbBackedOms::new(pool.clone(), caps, ks);

    // Pre-seed a $2 ($200 cents) open position on a different
    // strategy via direct DB insert. The OMS reads from
    // `positions`, which is what the global query sums.
    sqlx::query(
        "INSERT INTO positions
            (strategy, ticker, side, current_qty, avg_entry_cents,
             fees_paid_cents, opened_at, last_fill_at)
         VALUES ('settlement', 'KX-INT-GLOBAL', 'yes', 4, 50, 0, now(), now())",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Now stat tries to add $0.30 more (3 × 10¢) — total would
    // be $2.30 > $2 global cap → reject.
    let outcome = oms
        .submit(buy_yes("test:G:0001", "KX-INT-GLOBAL", 3, 10))
        .await
        .unwrap();
    match outcome {
        SubmitOutcome::Rejected {
            reason:
                RejectionReason::NotionalExceeded {
                    scope,
                    current_cents,
                    limit_cents,
                },
        } => {
            assert_eq!(scope, "global");
            assert_eq!(current_cents, 200);
            assert_eq!(limit_cents, 200);
        }
        other => panic!("expected NotionalExceeded(global); got {other:?}"),
    }
}

#[tokio::test]
async fn global_notional_cap_disabled_when_zero() {
    // max_global_notional_cents=0 disables the global gate.
    // Per-strategy caps still apply.
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-GZ").await;
    let ks = Arc::new(KillSwitchView::new());
    let caps = RiskCaps {
        max_notional_cents: 1_000_000,
        max_global_notional_cents: 0, // disabled
        max_daily_loss_cents: 1_000_000,
        max_contracts_per_side: 1000,
        max_in_flight: 1000,
        max_orders_per_window: 1000,
        rate_window_ms: 1000,
    };
    let oms = DbBackedOms::new(pool.clone(), caps, ks);

    // Pre-seed a large position; with the global cap off the
    // submit should pass.
    sqlx::query(
        "INSERT INTO positions
            (strategy, ticker, side, current_qty, avg_entry_cents,
             fees_paid_cents, opened_at, last_fill_at)
         VALUES ('settlement', 'KX-INT-GZ', 'yes', 100, 90, 0, now(), now())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let outcome = oms
        .submit(buy_yes("test:GZ:0001", "KX-INT-GZ", 1, 50))
        .await
        .unwrap();
    assert!(
        matches!(outcome, SubmitOutcome::Submitted { .. }),
        "global cap disabled should permit; got {outcome:?}"
    );
}

// ─── I7 — atomic multi-leg submit ──────────────────────────

fn buy_no(client_id: &str, ticker: &str, qty: i32, price_cents: i32) -> Intent {
    Intent {
        client_id: client_id.into(),
        strategy: "test",
        market: MarketTicker::new(ticker),
        side: Side::No,
        action: IntentAction::Buy,
        price_cents: Some(price_cents),
        qty,
        order_type: OrderType::Limit,
        tif: Tif::Ioc,
        reason: Some("integration test (no leg)".into()),
    }
}

#[tokio::test]
async fn submit_group_persists_all_legs_with_shared_id() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-LG-A").await;
    ensure_market(&pool, "KX-LG-B").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    let group = LegGroup::new(vec![
        buy_yes("test:LGA:0001", "KX-LG-A", 1, 30),
        buy_no("test:LGB:0001", "KX-LG-B", 1, 40),
    ])
    .unwrap();
    let group_id = group.group_id;

    let outcome = oms.submit_group(group).await.unwrap();
    match outcome {
        SubmitGroupOutcome::Submitted {
            group_id: gid,
            client_ids,
            ..
        } => {
            assert_eq!(gid, group_id);
            assert_eq!(client_ids.len(), 2);
        }
        other => panic!("expected Submitted; got {other:?}"),
    }

    let rows: Vec<(String, Option<uuid::Uuid>)> =
        sqlx::query_as("SELECT client_id, leg_group_id FROM intents ORDER BY client_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    for (_cid, gid) in &rows {
        assert_eq!(*gid, Some(group_id));
    }

    // Two intent_events rows, one per leg.
    let n_events: (i64,) = sqlx::query_as("SELECT COUNT(*)::BIGINT FROM intent_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n_events.0, 2);
}

#[tokio::test]
async fn submit_group_rejects_whole_group_on_kill_switch() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-LG-K1").await;
    ensure_market(&pool, "KX-LG-K2").await;
    let ks = Arc::new(KillSwitchView::new());
    ks.arm();
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    let group = LegGroup::new(vec![
        buy_yes("test:K1:0001", "KX-LG-K1", 1, 30),
        buy_no("test:K2:0001", "KX-LG-K2", 1, 40),
    ])
    .unwrap();

    let outcome = oms.submit_group(group).await.unwrap();
    assert!(
        matches!(
            outcome,
            SubmitGroupOutcome::Rejected {
                reason: RejectionReason::KillSwitchArmed { .. },
                ..
            }
        ),
        "expected KillSwitchArmed rejection, got {outcome:?}"
    );

    // No rows should have been inserted.
    let n: (i64,) = sqlx::query_as("SELECT COUNT(*)::BIGINT FROM intents")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n.0, 0, "rejected group must not persist");
}

#[tokio::test]
async fn submit_group_rejects_on_combined_notional_cap() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-LG-N1").await;
    ensure_market(&pool, "KX-LG-N2").await;
    let ks = Arc::new(KillSwitchView::new());
    let mut caps = permissive_caps();
    // 2 legs × 50 × 5¢ = 500¢ projected. Cap at 400 forces a
    // group-level reject even though each leg alone (250¢) would
    // pass.
    caps.max_notional_cents = 400;
    let oms = DbBackedOms::new(pool.clone(), caps, ks);

    let group = LegGroup::new(vec![
        buy_yes("test:N1:0001", "KX-LG-N1", 50, 5),
        buy_no("test:N2:0001", "KX-LG-N2", 50, 5),
    ])
    .unwrap();
    let outcome = oms.submit_group(group).await.unwrap();
    assert!(
        matches!(
            outcome,
            SubmitGroupOutcome::Rejected {
                reason: RejectionReason::NotionalExceeded { .. },
                ..
            }
        ),
        "expected combined-notional rejection; got {outcome:?}"
    );
    let n: (i64,) = sqlx::query_as("SELECT COUNT(*)::BIGINT FROM intents")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n.0, 0);
}

#[tokio::test]
async fn submit_group_idempotent_on_replay() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-LG-I1").await;
    ensure_market(&pool, "KX-LG-I2").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    let group_id = uuid::Uuid::new_v4();
    let intents = vec![
        buy_yes("test:I1:0001", "KX-LG-I1", 1, 30),
        buy_no("test:I2:0001", "KX-LG-I2", 1, 40),
    ];
    let group = LegGroup::with_id(group_id, intents.clone()).unwrap();
    let _ = oms.submit_group(group).await.unwrap();

    // Replay with the same group_id and same client_ids.
    let group2 = LegGroup::with_id(group_id, intents).unwrap();
    let outcome = oms.submit_group(group2).await.unwrap();
    match outcome {
        SubmitGroupOutcome::Idempotent { group_id: gid, .. } => assert_eq!(gid, group_id),
        other => panic!("expected Idempotent, got {other:?}"),
    }

    // Still exactly 2 intents in DB.
    let n: (i64,) = sqlx::query_as("SELECT COUNT(*)::BIGINT FROM intents")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n.0, 2);
}

#[tokio::test]
async fn submit_group_partial_collision_when_cid_reused_under_different_group() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-LG-P1").await;
    ensure_market(&pool, "KX-LG-P2").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    // First: a single-leg submit (no group). Same client_id.
    let lone = buy_yes("test:P1:0001", "KX-LG-P1", 1, 30);
    let _ = oms.submit(lone).await.unwrap();

    // Second: a group that includes that same client_id.
    let group = LegGroup::new(vec![
        buy_yes("test:P1:0001", "KX-LG-P1", 1, 30),
        buy_no("test:P2:0001", "KX-LG-P2", 1, 40),
    ])
    .unwrap();
    let outcome = oms.submit_group(group).await.unwrap();
    match outcome {
        SubmitGroupOutcome::PartialCollision { existing } => {
            assert_eq!(existing.len(), 1);
            assert_eq!(existing[0].0, "test:P1:0001");
            assert_eq!(existing[0].1, None);
        }
        other => panic!("expected PartialCollision, got {other:?}"),
    }
}

#[tokio::test]
async fn partial_close_preserves_avg_entry_cents() {
    // Regression for the upsert_position bug where a partial
    // close (sell against a long position that doesn't cross
    // zero) was treated as "adding to position" because both
    // cur_qty.signum() and new_qty.signum() were positive. The
    // weighted-avg formula then divided by `new_qty.abs()`,
    // corrupting `avg_entry_cents` by amplifying it (live
    // observation: a position bought at ~62¢ ended up with
    // avg_entry_cents=982 after a partial-close cycle).
    //
    // Correct behavior: a partial close leaves avg_entry_cents
    // unchanged; only the realized PnL accumulates.
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-PC").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    // Submit + fill 3 buys @ 60¢.
    oms.submit(buy_yes("test:PC:0001", "KX-INT-PC", 3, 60))
        .await
        .unwrap();
    oms.apply_execution(ExecutionUpdate {
        client_id: "test:PC:0001".into(),
        venue_order_id: Some("v-PC1".into()),
        venue_fill_id: Some("v-PC1-fill".into()),
        status: ExecutionStatus::Filled,
        cumulative_qty: 3,
        avg_fill_price_cents: Some(60),
        last_fill_qty: Some(3),
        last_fill_price_cents: Some(60),
        last_fill_fee_cents: Some(0),
        venue_payload: serde_json::json!({}),
    })
    .await
    .unwrap();

    // Partial close: sell 2 @ 65¢. Same side ("yes"), opposite
    // action ("sell"). Position goes from +3 long to +1 long.
    let exit = Intent {
        client_id: "test:PC:exit-0001".into(),
        strategy: "test",
        market: MarketTicker::new("KX-INT-PC"),
        side: Side::Yes,
        action: IntentAction::Sell,
        price_cents: Some(65),
        qty: 2,
        order_type: OrderType::Limit,
        tif: Tif::Ioc,
        reason: Some("partial close".into()),
    };
    oms.submit(exit).await.unwrap();
    oms.apply_execution(ExecutionUpdate {
        client_id: "test:PC:exit-0001".into(),
        venue_order_id: Some("v-PC2".into()),
        venue_fill_id: Some("v-PC2-fill".into()),
        status: ExecutionStatus::Filled,
        cumulative_qty: 2,
        avg_fill_price_cents: Some(65),
        last_fill_qty: Some(2),
        last_fill_price_cents: Some(65),
        last_fill_fee_cents: Some(0),
        venue_payload: serde_json::json!({}),
    })
    .await
    .unwrap();

    let pos: (i32, i32, i64) = sqlx::query_as(
        "SELECT current_qty, avg_entry_cents, realized_pnl_cents
           FROM positions
          WHERE strategy = 'test' AND ticker = 'KX-INT-PC' AND closed_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pos.0, 1, "partial close should leave 1 contract");
    assert_eq!(
        pos.1, 60,
        "partial close MUST NOT change avg_entry_cents (bug: was being recomputed via weighted-avg formula)"
    );
    assert_eq!(
        pos.2, 10,
        "realized PnL = (65-60) * 2 contracts closed = 10c"
    );
}

#[tokio::test]
async fn position_reversal_resets_avg_to_fill_price() {
    // When a fill flips the position to the opposite side
    // (cur=+3 long @ 60, sell 5 @ 65), the OMS should:
    //   1. Realize PnL on the closed portion (3 contracts).
    //   2. Reset avg_entry_cents to the fill price for the new
    //      opposing portion (the remaining 2 contracts on the
    //      short side, opened at 65¢).
    //
    // Pre-fix behavior: the "partial close" branch ran (because
    // cur and new had different signs) and avg_entry was left
    // unchanged at 60 — wrong, since we're now SHORT @ 65.
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-INT-RV").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new(pool.clone(), permissive_caps(), ks);

    // Build cur=+3 long @ 60.
    oms.submit(buy_yes("test:RV:0001", "KX-INT-RV", 3, 60))
        .await
        .unwrap();
    oms.apply_execution(ExecutionUpdate {
        client_id: "test:RV:0001".into(),
        venue_order_id: Some("v-RV1".into()),
        venue_fill_id: Some("v-RV1-f".into()),
        status: ExecutionStatus::Filled,
        cumulative_qty: 3,
        avg_fill_price_cents: Some(60),
        last_fill_qty: Some(3),
        last_fill_price_cents: Some(60),
        last_fill_fee_cents: Some(0),
        venue_payload: serde_json::json!({}),
    })
    .await
    .unwrap();

    // Reversal sell 5 @ 65: closes 3, opens new short of 2.
    let exit = Intent {
        client_id: "test:RV:exit-0001".into(),
        strategy: "test",
        market: MarketTicker::new("KX-INT-RV"),
        side: Side::Yes,
        action: IntentAction::Sell,
        price_cents: Some(65),
        qty: 5,
        order_type: OrderType::Limit,
        tif: Tif::Ioc,
        reason: Some("reversal".into()),
    };
    oms.submit(exit).await.unwrap();
    oms.apply_execution(ExecutionUpdate {
        client_id: "test:RV:exit-0001".into(),
        venue_order_id: Some("v-RV2".into()),
        venue_fill_id: Some("v-RV2-f".into()),
        status: ExecutionStatus::Filled,
        cumulative_qty: 5,
        avg_fill_price_cents: Some(65),
        last_fill_qty: Some(5),
        last_fill_price_cents: Some(65),
        last_fill_fee_cents: Some(0),
        venue_payload: serde_json::json!({}),
    })
    .await
    .unwrap();

    let pos: (i32, i32, i64) = sqlx::query_as(
        "SELECT current_qty, avg_entry_cents, realized_pnl_cents
           FROM positions
          WHERE strategy = 'test' AND ticker = 'KX-INT-RV' AND closed_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pos.0, -2, "should be short 2 after reversal");
    assert_eq!(
        pos.1, 65,
        "avg_entry should reset to fill price for the new side"
    );
    assert_eq!(pos.2, 15, "realized = (65-60) * 3 closed = 15c");
}

#[tokio::test]
async fn rejection_cascades_cancel_request_to_sibling_legs() {
    let _g = test_lock().await;
    let pool = fresh_pool().await;
    ensure_market(&pool, "KX-LG-C1").await;
    ensure_market(&pool, "KX-LG-C2").await;
    let ks = Arc::new(KillSwitchView::new());
    let oms = DbBackedOms::new_with_mode(pool.clone(), permissive_caps(), ks, EngineMode::Live);

    let group = LegGroup::new(vec![
        buy_yes("test:C1:0001", "KX-LG-C1", 1, 30),
        buy_no("test:C2:0001", "KX-LG-C2", 1, 40),
    ])
    .unwrap();
    let _ = oms.submit_group(group).await.unwrap();

    // Simulate venue-side rejection of leg 1.
    oms.apply_execution(ExecutionUpdate {
        client_id: "test:C1:0001".into(),
        venue_order_id: None,
        venue_fill_id: None,
        status: ExecutionStatus::Rejected,
        cumulative_qty: 0,
        avg_fill_price_cents: None,
        last_fill_qty: None,
        last_fill_price_cents: None,
        last_fill_fee_cents: None,
        venue_payload: serde_json::json!({"reason": "venue rejected"}),
    })
    .await
    .unwrap();

    // Sibling leg (C2) must be marked cancel_requested by the
    // cascade.
    let row: (String,) = sqlx::query_as("SELECT status FROM intents WHERE client_id = $1")
        .bind("test:C2:0001")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, "cancel_requested",
        "sibling leg should be cascade-cancelled"
    );

    // The cascade event is recorded in intent_events.
    let cascade_evs: (i64,) = sqlx::query_as(
        "SELECT COUNT(*)::BIGINT FROM intent_events
          WHERE client_id = $1 AND status = 'cancel_requested'",
    )
    .bind("test:C2:0001")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cascade_evs.0, 1);
}
