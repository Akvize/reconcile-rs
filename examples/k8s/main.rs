// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A Kubernetes-native reconcile node — the example behind `examples/k8s/`.
//!
//! It binds to its pod IP, derives a stable node identity from the pod name, discovers its peers by
//! resolving a headless `Service` over DNS, exposes a `/metrics` endpoint (Prometheus scrape +
//! liveness probe), and a separate readiness endpoint that only reports ready once the store is
//! built and its gossip loop is running. Everything is taken from the environment (the Kubernetes
//! downward API + a ConfigMap / Secret), so the same image works for every replica. See
//! `examples/k8s/base/` for the manifests and `examples/k8s/kind/` for a local playground.
//!
//! To make reconciliation *observable*, it also runs a small **demo** behaviour (clearly fenced
//! below with `--- demo ---` markers — delete those two blocks for a bare production node):
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
//!   cargo run --release --example k8s --features metrics-prometheus
//! ```

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{info, warn};

use reconcile::{reconcile_store::Config, ReconcileStore};

/// Serve a minimal readiness endpoint that returns `200` only once `ready` has been set, and `503`
/// before that. Pointing the kubelet readiness probe here (rather than at `/metrics`) keeps a pod
/// out of the headless-Service DNS — and thus out of every peer's `DnsDiscovery` set — until its
/// store is actually built and its gossip loop is running. `/metrics` comes up too early (it is the
/// in-process Prometheus listener, live before `ReconcileStore::new` binds the UDP socket), so a
/// pod probing `/metrics` would advertise itself before it could serve a single reconciliation.
///
/// This complements the discovery decommission-floor behaviour: readiness gates a pod *into*
/// rotation only when it can serve, while the decommission floor governs how long a vanished member
/// is tolerated before its tombstone-GC gate is released. The two are independent — one is join-time
/// health, the other leave-time grace — and both keep DNS membership honest.
async fn serve_readiness(addr: SocketAddr, ready: Arc<AtomicBool>) {
    let listener = TcpListener::bind(addr)
        .await
        .expect("failed to bind the readiness endpoint");
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let ready = Arc::clone(&ready);
        tokio::spawn(async move {
            // Drain the request line(s); we serve a fixed response regardless of path.
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let body = if ready.load(Ordering::Relaxed) {
                "ready"
            } else {
                "not ready"
            };
            let status = if ready.load(Ordering::Relaxed) {
                "200 OK"
            } else {
                "503 Service Unavailable"
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

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
    let ready_addr: SocketAddr = optional("RECONCILE_READY_ADDR", "0.0.0.0:9001")
        .parse()
        .expect("RECONCILE_READY_ADDR must be host:port");
    // How often this pod refreshes its own heartbeat key.
    let heartbeat_interval = Duration::from_secs(
        optional("RECONCILE_HEARTBEAT_SECS", "5")
            .parse()
            .expect("RECONCILE_HEARTBEAT_SECS must be an integer"),
    );

    // Build the config. No `with_net` is declared: in Kubernetes, peers come purely from DNS, so
    // the engine's random per-network probing is left disabled.
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

    // Start the readiness endpoint immediately, reporting NOT ready: the kubelet readiness probe
    // targets it, so the pod stays out of the headless-Service DNS (and out of peers' DnsDiscovery)
    // until the store below is built and its gossip loop is running. We flip it to ready only after
    // `ReconcileStore::new` has bound the UDP socket and `run()` is about to start.
    let ready = Arc::new(AtomicBool::new(false));
    tokio::spawn(serve_readiness(ready_addr, Arc::clone(&ready)));

    // Expose /metrics for the kubelet liveness probe and for Prometheus scraping. SECURITY: this
    // endpoint is unauthenticated and leaks operational info — keep it cluster-internal (see the
    // ConfigMap caveat) and bind it to a trusted interface rather than 0.0.0.0 if a node-local
    // sidecar is the only scraper.
    reconcile::prometheus::serve(metrics_addr)
        .await
        .expect("failed to start the metrics endpoint");

    let store = ReconcileStore::<String, String>::new(config)
        .await
        .expect("failed to bind UDP socket")
        .with_dns_discovery(dns_name, port)
        .with_discovery_interval(discovery_interval)
        .with_discovery_miss_threshold(miss_threshold);

    // --- demo (remove for a bare production node) -------------------------------------------
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
    // --- end demo ---------------------------------------------------------------------------

    info!(%pod_ip, %pod_name, port, "reconcile k8s node starting; writing heartbeats; discovering peers over DNS");
    // The store is built and its UDP socket is bound; the gossip loop starts on the next line.
    // Flip readiness on so the kubelet adds the pod to the Service DNS now that it can serve peers.
    ready.store(true, Ordering::Relaxed);
    store.run().await;
}
