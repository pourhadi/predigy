//! Cross-strategy event bus dispatcher.
//!
//! Owns one mpsc receiver fed by every strategy's
//! `StrategyState::publish_cross_strategy`. Per-event:
//!
//! 1. Read the topic tag via `payload.payload_topic()`.
//! 2. Look up subscribers for that topic from the registry built
//!    at engine boot (per-strategy `cross_strategy_subscriptions()`).
//! 3. Construct an `Event::CrossStrategy { source, payload }` and
//!    `try_send` to each subscriber's supervisor queue. Slow
//!    consumers don't backpressure the bus.
//!
//! The producer side is the `tx` returned from
//! `start()`; clone it into each `StrategyState` via
//! `with_cross_strategy_tx`. The receiver lives inside the bus
//! task and isn't visible elsewhere.

use predigy_engine_core::events::Event;
use predigy_engine_core::state::PublishedCrossStrategyEvent;
use predigy_engine_core::strategy::StrategyId;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const BUS_QUEUE_CAPACITY: usize = 1024;

/// Public handle. Drop or call `shutdown` to abort the task.
pub struct CrossStrategyBus {
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for CrossStrategyBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossStrategyBus").finish_non_exhaustive()
    }
}

/// Sender handle the engine clones into each `StrategyState`.
pub type BusTx = mpsc::Sender<PublishedCrossStrategyEvent>;

/// Per-supervisor entry the bus uses to deliver. We don't reuse
/// the discovery / external-feed Vec<(StrategyId, mpsc::Sender)>
/// shape because here the lookup is by topic, not by strategy.
#[derive(Debug, Clone)]
pub struct CrossStrategySubscriber {
    pub strategy: StrategyId,
    pub event_tx: mpsc::Sender<Event>,
}

impl CrossStrategyBus {
    /// Build the producer/consumer channel pair. Call this FIRST
    /// — supervisors need the producer-side `BusTx` cloned into
    /// their `StrategyState`s before they spawn, but the consumer
    /// side (`rx`) can't start dispatching until subscribers are
    /// registered. Pass the returned `rx` to
    /// `start_dispatching` once the subscriber map is built.
    pub fn channel() -> (BusTx, mpsc::Receiver<PublishedCrossStrategyEvent>) {
        mpsc::channel::<PublishedCrossStrategyEvent>(BUS_QUEUE_CAPACITY)
    }

    /// Start the dispatcher task. Consumes the `rx` returned from
    /// [`channel`] and the subscriber map built from each
    /// strategy's `cross_strategy_subscriptions()`.
    ///
    /// Returns `None` if there are no subscribers for any topic
    /// — saves spawning a task that can never deliver. Late
    /// subscription isn't supported (the dispatcher's map is
    /// frozen at start time).
    pub fn start_dispatching(
        rx: mpsc::Receiver<PublishedCrossStrategyEvent>,
        subscribers_by_topic: HashMap<&'static str, Vec<CrossStrategySubscriber>>,
    ) -> Option<Self> {
        if subscribers_by_topic.is_empty() {
            return None;
        }
        let task = tokio::spawn(bus_task(rx, subscribers_by_topic));
        Some(Self { task })
    }

    pub async fn shutdown(self, grace: Duration) {
        self.task.abort();
        let _ = tokio::time::timeout(grace, self.task).await;
    }
}

async fn bus_task(
    mut rx: mpsc::Receiver<PublishedCrossStrategyEvent>,
    subscribers_by_topic: HashMap<&'static str, Vec<CrossStrategySubscriber>>,
) {
    info!(
        n_topics = subscribers_by_topic.len(),
        "cross-strategy bus started"
    );
    while let Some(envelope) = rx.recv().await {
        let topic = envelope.payload.payload_topic();
        let Some(subs) = subscribers_by_topic.get(topic) else {
            debug!(topic, "cross-strategy: no subscribers; dropping");
            continue;
        };
        let ev = Event::CrossStrategy {
            source: envelope.source,
            payload: envelope.payload.clone(),
        };
        for sub in subs {
            // Don't deliver back to the producer — strategies
            // that subscribe to a topic they also produce don't
            // typically want their own emissions echoed. Cheap
            // self-filter; consumers can opt in by checking
            // source themselves if they ever want this.
            if sub.strategy == envelope.source {
                continue;
            }
            if let Err(e) = sub.event_tx.try_send(ev.clone()) {
                warn!(
                    topic,
                    consumer = sub.strategy.0,
                    error = %e,
                    "cross-strategy fan-out failed"
                );
            }
        }
    }
    info!("cross-strategy bus: producer channel closed; exiting");
}

/// Build the subscribers-by-topic registry from
/// (strategy, topic_list, event_tx) triples — one triple per
/// (supervisor, declared topic).
pub fn build_subscriber_map(
    pairs: Vec<(StrategyId, &'static str, mpsc::Sender<Event>)>,
) -> HashMap<&'static str, Vec<CrossStrategySubscriber>> {
    let mut by_topic: HashMap<&'static str, Vec<CrossStrategySubscriber>> = HashMap::new();
    for (strategy, topic, event_tx) in pairs {
        by_topic
            .entry(topic)
            .or_default()
            .push(CrossStrategySubscriber { strategy, event_tx });
    }
    by_topic
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_engine_core::cross_strategy::CrossStrategyEvent;

    #[test]
    fn build_subscriber_map_groups_by_topic() {
        let (tx_a, _rx_a) = mpsc::channel::<Event>(1);
        let (tx_b, _rx_b) = mpsc::channel::<Event>(1);
        let pairs = vec![
            (StrategyId("a"), "poly_mid", tx_a),
            (StrategyId("b"), "poly_mid", tx_b),
        ];
        let m = build_subscriber_map(pairs);
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("poly_mid").map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn bus_routes_to_subscribers_and_skips_producer() {
        let (consumer_tx, mut consumer_rx) = mpsc::channel::<Event>(8);
        let (producer_tx, mut producer_rx) = mpsc::channel::<Event>(8);
        let mut by_topic: HashMap<&'static str, Vec<CrossStrategySubscriber>> = HashMap::new();
        by_topic.insert(
            "poly_mid",
            vec![
                CrossStrategySubscriber {
                    strategy: StrategyId("consumer"),
                    event_tx: consumer_tx,
                },
                CrossStrategySubscriber {
                    strategy: StrategyId("producer"),
                    event_tx: producer_tx,
                },
            ],
        );
        let (tx, rx) = CrossStrategyBus::channel();
        let bus = CrossStrategyBus::start_dispatching(rx, by_topic).expect("subscribers present");

        // Emit one event from "producer". The bus should deliver
        // to "consumer" only.
        tx.send(PublishedCrossStrategyEvent {
            source: StrategyId("producer"),
            payload: CrossStrategyEvent::PolyMidUpdate {
                kalshi_ticker: predigy_core::market::MarketTicker::new("KX-A"),
                poly_mid_cents: 50,
            },
        })
        .await
        .unwrap();

        // consumer receives.
        let ev = tokio::time::timeout(Duration::from_secs(1), consumer_rx.recv())
            .await
            .expect("delivered within 1s")
            .expect("not closed");
        match ev {
            Event::CrossStrategy { source, payload } => {
                assert_eq!(source.0, "producer");
                assert!(matches!(payload, CrossStrategyEvent::PolyMidUpdate { .. }));
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // producer must NOT receive its own emission.
        let producer_recv =
            tokio::time::timeout(Duration::from_millis(100), producer_rx.recv()).await;
        assert!(
            producer_recv.is_err(),
            "producer should not receive its own emission"
        );

        bus.shutdown(Duration::from_secs(1)).await;
    }

    #[test]
    fn start_dispatching_returns_none_when_no_subscribers() {
        let (_tx, rx) = CrossStrategyBus::channel();
        let bus = CrossStrategyBus::start_dispatching(rx, HashMap::new());
        assert!(bus.is_none());
    }
}
