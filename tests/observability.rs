// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Smoke tests for the observability instrumentation (issue #94).
//!
//! These tests deliberately exercise only the **synchronous, same-thread** paths:
//! `tracing::subscriber::set_default` and `metrics::with_local_recorder` are both
//! thread-local, so events emitted on `tokio::spawn`-ed tasks (the `run()` loop) are out of
//! scope here. Running on a `current_thread` runtime keeps the `async` work on the test thread
//! so the lifecycle events of `ReconcileStore::new` are captured.

use std::sync::{Arc, Mutex};

use reconcile::{reconcile_store::Config, ReconcileStore};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::Registry;

fn local_config() -> Config {
    Config::default()
        .with_port(0) // ephemeral port: avoids clashing with the other integration tests
        .with_listen_addr("127.0.0.1".parse().unwrap())
        .with_local_region("127.0.0.1/8".parse().unwrap())
}

/// Keep every `tracing` callsite "hot" for the whole test binary.
///
/// `tracing` caches per-callsite interest **globally**, but these tests install
/// **thread-local** subscribers (`set_default`). If a store is ever constructed while the
/// current default is the no-op `NoSubscriber` — e.g. the metrics test below, or a parallel
/// test caught between guard transitions — the `info!("Listening on")` callsite is registered
/// as `Interest::never` and then silently vanishes for every other test, regardless of their
/// thread-local subscriber. Installing a permanently-interested **global** default once keeps
/// all callsites enabled; the per-test thread-local capturing subscribers still receive the
/// events (a thread-local default overrides the global one for dispatch).
fn keep_callsites_hot() {
    use std::sync::Once;
    use tracing_subscriber::filter::LevelFilter;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ =
            tracing::subscriber::set_global_default(Registry::default().with(LevelFilter::TRACE));
    });
}

/// A minimal `tracing` layer that records `(level, message)` for every event into a shared Vec.
#[derive(Clone, Default)]
struct CapturingLayer(Arc<Mutex<Vec<(Level, String)>>>);

struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: Subscriber> Layer<S> for CapturingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        self.0
            .lock()
            .unwrap()
            .push((*event.metadata().level(), visitor.0));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn startup_emits_info_and_security_warning_without_cluster_key() {
    keep_callsites_hot();
    let layer = CapturingLayer::default();
    let events = layer.0.clone();
    let subscriber = Registry::default().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    let _store = ReconcileStore::<String, String>::new(local_config()).await;

    let events = events.lock().unwrap();
    let has_info_listening = events
        .iter()
        .any(|(level, msg)| *level == Level::INFO && msg.contains("Listening on"));
    let has_security_warn = events.iter().any(|(level, _)| *level == Level::WARN);

    assert!(
        has_info_listening,
        "expected an INFO 'Listening on' lifecycle event, captured: {events:?}"
    );
    assert!(
        has_security_warn,
        "expected a WARN security event (no cluster key set), captured: {events:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cluster_key_suppresses_the_security_warning() {
    keep_callsites_hot();
    let layer = CapturingLayer::default();
    let events = layer.0.clone();
    let subscriber = Registry::default().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    let config = local_config().with_cluster_key([7u8; 32]);
    let _store = ReconcileStore::<String, String>::new(config).await;

    let events = events.lock().unwrap();
    let has_info_listening = events
        .iter()
        .any(|(level, msg)| *level == Level::INFO && msg.contains("Listening on"));
    let has_warn = events.iter().any(|(level, _)| *level == Level::WARN);

    // Same code path, different level outcome — proves level discrimination is wired correctly.
    assert!(
        has_info_listening,
        "expected the INFO lifecycle event regardless of authentication, captured: {events:?}"
    );
    assert!(
        !has_warn,
        "a cluster key is set, so no security WARN should fire, captured: {events:?}"
    );
}

#[cfg(feature = "metrics")]
#[tokio::test(flavor = "current_thread")]
async fn local_mutations_increment_metric_counters() {
    use metrics::with_local_recorder;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    // `new` binds a socket but emits no metrics, so build the store outside the recorder scope.
    // This store is built without a tracing subscriber; `keep_callsites_hot` prevents its
    // `info!("Listening on")` emission from poisoning the shared callsite cache for the other
    // tests (see that helper).
    keep_callsites_hot();
    let store = ReconcileStore::<i32, i32>::new(local_config()).await;

    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    // `just_insert` / `just_remove` are the purely local, synchronous variants (no broadcast),
    // so every metric is emitted on this thread, inside the local-recorder scope.
    with_local_recorder(&recorder, || {
        store.just_insert(1, 10);
        store.just_insert(2, 20);
        store.just_remove(&1);
    });

    let mut inserts = 0u64;
    let mut removes = 0u64;
    for (composite, _unit, _desc, value) in snapshotter.snapshot().into_vec() {
        if let DebugValue::Counter(v) = value {
            match composite.key().name() {
                "reconcile_inserts_total" => inserts = v,
                "reconcile_removes_total" => removes = v,
                _ => {}
            }
        }
    }

    assert_eq!(inserts, 2, "expected two inserts to be counted");
    assert_eq!(removes, 1, "expected one removal to be counted");
}
