// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A Kubernetes-native reconcile node.
//!
//! It binds to its pod IP, derives a stable node identity from the pod name, discovers its peers by
//! resolving a headless `Service` over DNS, and exposes a `/metrics` endpoint for the kubelet
//! probes. Everything is taken from the environment (the Kubernetes downward API + a ConfigMap /
//! Secret), so the same image works for every replica. See `deploy/k8s/` for the manifests.
//!
//! Run it with the metrics endpoint built in:
//!
//! ```sh
//! cargo run --release --example k8s_node --features metrics-prometheus
//! ```

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

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

    // Build the config. Note: NO `with_net` is declared — in Kubernetes, peers are found purely
    // through DNS-resolved pod IPs, so the engine's random per-network probing is left disabled.
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

    info!(%pod_ip, %pod_name, port, "reconcile k8s node starting; discovering peers over DNS");
    store.run().await;
}
