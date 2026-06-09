// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A Kubernetes-native reconcile node that shows convergence in its logs.
//!
//! It is the `k8s_node` example (pod-IP binding, stable identity from the pod name, DNS peer
//! discovery, a `/metrics` endpoint for the kubelet probes) plus two tiny additions that make
//! reconciliation *observable* without any extra port, API, or dependency:
//!
//!   * every few seconds the pod writes one key for itself — `heartbeat/<pod-name>` — with the
//!     current time as its value;
//!   * a pre-insert hook logs each key the first time it appears in the local store.
//!
//! Because the hook fires for updates merged *from peers* too, `kubectl logs reconcile-2` shows the
//! node learning about `heartbeat/reconcile-0`, `heartbeat/reconcile-1`, … as gossip reconciles the
//! cluster — i.e. you watch the replicas converge in real time, straight from the logs.
//!
//! Run it locally:
//!
//! ```sh
//! POD_IP=127.0.0.1 RECONCILE_DNS_NAME=localhost \
//!   cargo run --release --example k8s_heartbeat --features metrics-prometheus
//! ```

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

use reconcile::{reconcile_store::Config, ReconcileStore};

/// Read a required environment variable or exit with a clear message.
fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing required environment variable {name}"))
}

/// Read an optional environment variable, falling back to `default`.
fn optional(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Decode a 32-byte cluster key from a hex string (64 hex chars).
fn parse_cluster_key(hex: &str) -> [u8; 32] {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .expect("RECONCILE_CLUSTER_KEY must be valid hexadecimal")
        })
        .collect();
    bytes
        .try_into()
        .expect("RECONCILE_CLUSTER_KEY must decode to exactly 32 bytes (64 hex chars)")
}

/// Derive a stable, per-pod node id from the (stable) StatefulSet pod name.
fn node_id_from(pod_name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    pod_name.hash(&mut hasher);
    hasher.finish()
}

#[tokio::main]
async fn main() {
    let log_level: tracing::Level = optional("RECONCILE_LOG_LEVEL", "info")
        .parse()
        .unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt().with_max_level(log_level).init();

    // Identity & networking from the downward API.
    let pod_ip: IpAddr = required("POD_IP")
        .parse()
        .expect("POD_IP must be a valid IP address");
    let pod_name = optional("POD_NAME", "reconcile-node");
    let port: u16 = optional("RECONCILE_PORT", "8080")
        .parse()
        .expect("RECONCILE_PORT must be a valid port number");

    // Discovery & operational knobs from the ConfigMap.
    let dns_name = required("RECONCILE_DNS_NAME");
    let discovery_interval = Duration::from_secs(
        optional("RECONCILE_DISCOVERY_INTERVAL_SECS", "5")
            .parse()
            .expect("RECONCILE_DISCOVERY_INTERVAL_SECS must be an integer"),
    );
    let miss_threshold: u32 = optional("RECONCILE_DISCOVERY_MISS_THRESHOLD", "3")
        .parse()
        .expect("RECONCILE_DISCOVERY_MISS_THRESHOLD must be an integer");
    let metrics_addr: SocketAddr = optional("RECONCILE_METRICS_ADDR", "0.0.0.0:9000")
        .parse()
        .expect("RECONCILE_METRICS_ADDR must be host:port");
    // How often this pod refreshes its own heartbeat key.
    let heartbeat_interval = Duration::from_secs(
        optional("RECONCILE_HEARTBEAT_SECS", "5")
            .parse()
            .expect("RECONCILE_HEARTBEAT_SECS must be an integer"),
    );

    // Build the config. As in `k8s_node`, no `with_net` is declared: peers come from DNS.
    let mut config = Config::default()
        .with_port(port)
        .with_listen_addr(pod_ip)
        .with_node_id(node_id_from(&pod_name));

    match std::env::var("RECONCILE_CLUSTER_KEY") {
        Ok(hex) if !hex.is_empty() => config = config.with_cluster_key(parse_cluster_key(&hex)),
        _ => warn!(
            "RECONCILE_CLUSTER_KEY is unset: the cluster runs UNAUTHENTICATED. Set it (from a \
             Kubernetes Secret) on every pod in production."
        ),
    }

    // Expose /metrics for the kubelet readiness/liveness probes.
    reconcile::prometheus::serve(metrics_addr)
        .await
        .expect("failed to start the metrics endpoint");

    let store = ReconcileStore::<String, String>::new(config)
        .await
        .with_dns_discovery(dns_name, port)
        .with_discovery_interval(discovery_interval)
        .with_discovery_miss_threshold(miss_threshold);

    // Log each key the first time it lands in the local store. The pre-insert hook fires for local
    // writes AND for updates merged from peers, so this is exactly where reconciliation becomes
    // visible: the lines for OTHER pods' heartbeats only appear once gossip has delivered them.
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    {
        let seen = seen.clone();
        store.add_pre_insert(move |key: &String, value| {
            // `value.1` is `None` for a tombstone (a delete); only announce live keys.
            if value.1.is_some() && seen.lock().unwrap().insert(key.clone()) {
                info!(%key, "store now holds this key (local write or reconciled from a peer)");
            }
        });
    }

    // Refresh this pod's own heartbeat key on a fixed cadence, on a clone of the (Arc-backed) store
    // handle. `run` below owns its own handle for the gossip loop.
    let heartbeat_store = store.clone();
    let heartbeat_key = format!("heartbeat/{pod_name}");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(heartbeat_interval);
        loop {
            ticker.tick().await;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            heartbeat_store.insert(heartbeat_key.clone(), now.to_string());
        }
    });

    info!(%pod_ip, %pod_name, port, "reconcile k8s node starting; writing heartbeats; discovering peers over DNS");
    store.run().await;
}
