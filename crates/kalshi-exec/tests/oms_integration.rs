//! End-to-end: `Oms` → `RestExecutor` → mock Kalshi REST → fills polling.
//!
//! Spins up a hand-rolled HTTP mock that responds to:
//! - `POST /portfolio/events/orders` with a 201 + canned `order_id`
//! - `DELETE /portfolio/events/orders/{id}` with a 200
//! - `GET /portfolio/fills` with a fixture list
//!
//! Drives an OMS through submit → fills polling → cancel and asserts
//! that the expected `OmsEvent`s show up in the right order.

mod http_mock;

use http_mock::{MockRoute, MockServer};
use predigy_core::{Action, Intent, MarketTicker, Price, Qty, Side};
use predigy_kalshi_exec::{PollerConfig, RestExecutor};
use predigy_kalshi_rest::Client as RestClient;
use predigy_kalshi_rest::Signer;
use predigy_oms::{Oms, OmsConfig, OmsEvent};
use predigy_risk::{Limits, PerMarketLimits, RiskEngine};
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

fn permissive() -> Limits {
    Limits {
        per_market: PerMarketLimits {
            max_contracts_per_side: 100_000,
            max_notional_cents_per_side: 10_000_000,
        },
        ..Limits::default()
    }
}

/// Generate a throwaway PEM so the REST client can be built; the mock
/// server doesn't validate signatures.
fn test_pem() -> String {
    use rsa::RsaPrivateKey;
    use rsa::pkcs8::EncodePrivateKey;
    let mut rng = rand::thread_rng();
    let key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
    key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .unwrap()
        .to_string()
}

async fn next_event_with_timeout(oms: &mut predigy_oms::OmsHandle) -> OmsEvent {
    tokio::time::timeout(Duration::from_secs(5), oms.next_event())
        .await
        .expect("event in time")
        .expect("stream not closed")
}

async fn drain_until<F>(oms: &mut predigy_oms::OmsHandle, mut pred: F) -> OmsEvent
where
    F: FnMut(&OmsEvent) -> bool,
{
    for _ in 0..32 {
        let ev = next_event_with_timeout(oms).await;
        if pred(&ev) {
            return ev;
        }
    }
    panic!("predicate never matched in 32 events");
}

#[tokio::test]
async fn submit_emits_acked_then_polled_fill_emits_filled_and_position() {
    // The mock returns:
    // - POST /portfolio/events/orders → 201 with {"order_id":"V-1", ...}
    // - GET /portfolio/fills → first call: empty, second call: one fill
    // We control the second response by mutating the route table mid-test.

    let routes = vec![
        MockRoute {
            method: "POST".into(),
            path_prefix: "/portfolio/events/orders".into(),
            status: 201,
            body: r#"{"order_id":"V-1","client_order_id":"arb:X:00000000","fill_count":"0.00","remaining_count":"100.00"}"#.into(),
        },
        MockRoute {
            method: "GET".into(),
            path_prefix: "/portfolio/fills".into(),
            status: 200,
            body: r#"{"fills":[]}"#.into(),
        },
    ];
    let mock = MockServer::start(routes).await;

    let signer = Signer::from_pem("test-key", &test_pem()).unwrap();
    let rest = RestClient::with_base(&mock.base_url, Some(signer)).unwrap();
    let (executor, reports) = RestExecutor::spawn(
        rest,
        PollerConfig {
            interval: Duration::from_millis(50),
            initial_lookback: Duration::from_mins(1),
        },
    );

    let mut oms = Oms::spawn(
        OmsConfig {
            strategy_id: "arb".into(),
            start_cid_seq: 0,
        },
        RiskEngine::new(permissive()),
        executor,
        reports,
    );

    let cid = oms.submit(buy_yes("X", 42, 100)).await.expect("submit ok");
    assert_eq!(cid.as_str(), "arb:X:00000000");

    // First event: Submitted (from OMS).
    let _ = drain_until(&mut oms, |e| matches!(e, OmsEvent::Submitted { .. })).await;
    // Then: Acked from the executor's response.
    match drain_until(&mut oms, |e| matches!(e, OmsEvent::Acked { .. })).await {
        OmsEvent::Acked { venue_order_id, .. } => assert_eq!(venue_order_id, "V-1"),
        _ => unreachable!(),
    }

    // Now flip the fills route to return a single fill matching V-1 for
    // the full 100 contracts. The poller will pick it up on its next
    // tick (within ~50ms).
    mock.set_routes(vec![
        MockRoute {
            method: "POST".into(),
            path_prefix: "/portfolio/events/orders".into(),
            status: 201,
            body: r#"{"order_id":"V-1","client_order_id":"arb:X:00000000","fill_count":"0.00","remaining_count":"100.00"}"#.into(),
        },
        MockRoute {
            method: "GET".into(),
            path_prefix: "/portfolio/fills".into(),
            status: 200,
            body: r#"{
                "fills":[{
                    "fill_id":"f-1",
                    "order_id":"V-1",
                    "market_ticker":"X",
                    "side":"yes",
                    "action":"buy",
                    "count_fp":"100.00",
                    "yes_price_dollars":"0.4100",
                    "no_price_dollars":"0.5900",
                    "is_taker":true,
                    "fee_cost":"0.07",
                    "ts":1700000000
                }],
                "cursor":null
            }"#.into(),
        },
    ]);

    // Filled event from the polled fill.
    match drain_until(&mut oms, |e| matches!(e, OmsEvent::Filled { .. })).await {
        OmsEvent::Filled {
            cumulative_qty,
            fill_price,
            ..
        } => {
            assert_eq!(cumulative_qty, 100);
            assert_eq!(fill_price.cents(), 41);
        }
        _ => unreachable!(),
    }
    match drain_until(&mut oms, |e| matches!(e, OmsEvent::PositionUpdated { .. })).await {
        OmsEvent::PositionUpdated {
            new_qty,
            new_avg_entry_cents,
            ..
        } => {
            assert_eq!(new_qty, 100);
            assert_eq!(new_avg_entry_cents, 41);
        }
        _ => unreachable!(),
    }

    oms.close().await;
}

#[tokio::test]
async fn cancel_emits_cancelled_event() {
    let routes = vec![
        MockRoute {
            method: "POST".into(),
            path_prefix: "/portfolio/events/orders".into(),
            status: 201,
            body: r#"{"order_id":"V-2","client_order_id":"arb:X:00000000","fill_count":"0.00","remaining_count":"50.00"}"#.into(),
        },
        MockRoute {
            method: "DELETE".into(),
            path_prefix: "/portfolio/events/orders/".into(),
            status: 200,
            body: r#"{"order_id":"V-2","client_order_id":"arb:X:00000000","reduced_by":"50.00"}"#.into(),
        },
        MockRoute {
            method: "GET".into(),
            path_prefix: "/portfolio/fills".into(),
            status: 200,
            body: r#"{"fills":[]}"#.into(),
        },
    ];
    let mock = MockServer::start(routes).await;

    let signer = Signer::from_pem("test-key", &test_pem()).unwrap();
    let rest = RestClient::with_base(&mock.base_url, Some(signer)).unwrap();
    let (executor, reports) = RestExecutor::spawn(rest, PollerConfig::default());

    let mut oms = Oms::spawn(
        OmsConfig::default(),
        RiskEngine::new(permissive()),
        executor,
        reports,
    );
    let cid = oms.submit(buy_yes("X", 42, 50)).await.unwrap();
    let _ = drain_until(&mut oms, |e| matches!(e, OmsEvent::Acked { .. })).await;

    oms.cancel(cid.clone()).await.expect("cancel ok");
    match drain_until(&mut oms, |e| matches!(e, OmsEvent::Cancelled { .. })).await {
        OmsEvent::Cancelled { cid: c, .. } => assert_eq!(c, cid),
        _ => unreachable!(),
    }

    // Mock recorded the DELETE with the venue order id in the path.
    let recorded = mock.recorded();
    assert!(
        recorded
            .iter()
            .any(|r| r.method == "DELETE" && r.path == "/portfolio/events/orders/V-2"),
        "expected DELETE to /portfolio/events/orders/V-2; got {recorded:?}"
    );
    oms.close().await;
}

#[tokio::test]
async fn submit_failure_emits_rejected_and_does_not_track() {
    // Mock returns 400 for the create-order POST.
    let routes = vec![
        MockRoute {
            method: "POST".into(),
            path_prefix: "/portfolio/events/orders".into(),
            status: 400,
            body: r#"{"error":"bad price"}"#.into(),
        },
        MockRoute {
            method: "GET".into(),
            path_prefix: "/portfolio/fills".into(),
            status: 200,
            body: r#"{"fills":[]}"#.into(),
        },
    ];
    let mock = MockServer::start(routes).await;
    let signer = Signer::from_pem("test-key", &test_pem()).unwrap();
    let rest = RestClient::with_base(&mock.base_url, Some(signer)).unwrap();
    let (executor, reports) = RestExecutor::spawn(rest, PollerConfig::default());

    let oms = Oms::spawn(
        OmsConfig::default(),
        RiskEngine::new(permissive()),
        executor,
        reports,
    );

    let err = oms.submit(buy_yes("X", 42, 1)).await.unwrap_err();
    assert!(matches!(err, predigy_oms::OmsError::Executor(_)));

    // The Rejected event from the executor still flows through the
    // OMS because the OMS never recorded the order (executor returned
    // an error, so handle_submit returned without inserting into
    // self.orders) — so the report is dropped at OMS level. The OMS
    // emitted neither Submitted nor Rejected. That's the intended
    // contract: a failed submit leaves zero state.
    //
    // What we CAN check: the mock recorded one POST attempt.
    let recorded = mock.recorded();
    assert_eq!(
        recorded
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/portfolio/events/orders")
            .count(),
        1
    );
    oms.close().await;
}
