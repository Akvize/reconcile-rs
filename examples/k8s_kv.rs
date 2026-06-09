// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A Kubernetes-native reconcile node, *plus a tiny HTTP key/value API* so you can actually
//! observe reconciliation between pods.
//!
//! It is identical to the `k8s_node` example (pod-IP binding, stable identity from the pod name,
//! DNS peer discovery, a `/metrics` endpoint for the kubelet probes) but additionally serves a
//! minimal HTTP API on a second port:
//!
//! | Method & path        | Effect                                                       |
//! | -------------------- | ------------------------------------------------------------ |
//! | `PUT /kv/<key>`      | Insert `<key>` with the request body as its value.           |
//! | `GET /kv/<key>`      | Return the current value of `<key>` (404 if absent).         |
//! | `DELETE /kv/<key>`   | Remove `<key>`.                                              |
//! | `GET /kv`            | List every key currently known to this replica, one per line.|
//!
//! Write to one pod, then read from another and watch the value appear — that is the gossip
//! reconciliation doing its job. The `GET /kv` listing is maintained from the store's pre-insert
//! hook, which fires on *peer-originated* updates too, so it reflects the replicated keyspace, not
//! just what this pod was written to directly.
//!
//! This is a DEMO surface: the HTTP API is unauthenticated and meant for the local `deploy/kind/`
//! playground, not production. (The gossip protocol itself is still authenticated by the cluster
//! key, exactly as in `k8s_node`.)
//!
//! Run it locally:
//!
//! ```sh
//! POD_IP=127.0.0.1 RECONCILE_DNS_NAME=localhost \
//!   cargo run --release --example k8s_kv --features metrics-prometheus
//! ```

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
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

/// The set of keys this replica currently holds, kept in sync by the store's pre-insert hook so
/// the `GET /kv` listing reflects peer-originated (reconciled) keys as well as local writes.
type KeySet = Arc<Mutex<BTreeSet<String>>>;

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
    // The demo HTTP key/value API. Bind to the pod IP by default so peers/port-forwards reach it.
    let http_addr: SocketAddr = optional("RECONCILE_HTTP_ADDR", "0.0.0.0:8081")
        .parse()
        .expect("RECONCILE_HTTP_ADDR must be host:port");

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

    // Track the live keyset for `GET /kv`. The hook fires for local inserts AND for updates merged
    // from peers, so the listing mirrors the reconciled keyspace. A `None` payload is a tombstone
    // (a delete), so drop the key from the set.
    let keys: KeySet = Arc::new(Mutex::new(BTreeSet::new()));
    {
        let keys = keys.clone();
        store.add_pre_insert(move |k: &String, v| {
            let mut set = keys.lock().unwrap();
            if v.1.is_some() {
                set.insert(k.clone());
            } else {
                set.remove(k);
            }
        });
    }

    // Spawn the HTTP API on a clone of the store handle (cheap; it's Arc-backed). `run` below
    // consumes its own handle for the gossip loop.
    let http_store = store.clone();
    tokio::spawn(async move {
        if let Err(e) = serve_http(http_addr, http_store, keys).await {
            warn!(error = %e, "HTTP key/value API stopped");
        }
    });

    info!(%pod_ip, %pod_name, port, %http_addr, "reconcile k8s node starting; HTTP KV API up; discovering peers over DNS");
    store.run().await;
}

/// A deliberately tiny HTTP/1.1 server (one request per connection, `Connection: close`). It is
/// just enough to drive the demo with `curl`/`kubectl port-forward`; it is not a general-purpose
/// or hardened HTTP stack.
async fn serve_http(
    addr: SocketAddr,
    store: ReconcileStore<String, String>,
    keys: KeySet,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "HTTP key/value API listening");
    loop {
        let (stream, _peer) = listener.accept().await?;
        let store = store.clone();
        let keys = keys.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, store, keys).await {
                warn!(error = %e, "error handling HTTP connection");
            }
        });
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    store: ReconcileStore<String, String>,
    keys: KeySet,
) -> std::io::Result<()> {
    // Read until the end of the headers, then read exactly `Content-Length` bytes of body.
    let mut buf = Vec::with_capacity(1024);
    let header_end = loop {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            // Connection closed before we saw a full request.
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return write_response(&mut stream, 431, "Request Header Fields Too Large", "").await;
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    // Find Content-Length (case-insensitive header name) and read the body.
    let content_length: usize = lines
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0);
    let body_start = header_end + 4;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    // Route on (method, path). Keys are taken verbatim from the path (no percent-decoding), so use
    // simple keys for the demo.
    match (method, path.strip_prefix("/kv/"), path) {
        // List every known key, one per line. Build the body in a tight scope so the (non-Send)
        // lock guard is dropped before the `.await`, keeping this future `Send` for `tokio::spawn`.
        (_, _, "/kv") if method == "GET" => {
            let body: String = {
                let set = keys.lock().unwrap();
                set.iter().map(|k| format!("{k}\n")).collect()
            };
            write_response(&mut stream, 200, "OK", &body).await
        }
        (_, Some(key), _) if !key.is_empty() => {
            let key = key.to_string();
            match method {
                "PUT" | "POST" => {
                    let value = String::from_utf8_lossy(&body).to_string();
                    store.insert(key.clone(), value);
                    write_response(&mut stream, 200, "OK", &format!("stored {key}\n")).await
                }
                // Clone the value out so the read guard (also non-Send) is released before awaiting.
                "GET" => match store.get(&key).map(|v| v.clone()) {
                    Some(v) => write_response(&mut stream, 200, "OK", &format!("{v}\n")).await,
                    None => write_response(&mut stream, 404, "Not Found", "no such key\n").await,
                },
                "DELETE" => match store.remove(&key) {
                    Some(_) => write_response(&mut stream, 200, "OK", &format!("removed {key}\n")).await,
                    None => write_response(&mut stream, 404, "Not Found", "no such key\n").await,
                },
                _ => write_response(&mut stream, 405, "Method Not Allowed", "").await,
            }
        }
        _ => {
            let help = "reconcile-rs demo KV API\n\
                        PUT    /kv/<key>   (body = value)\n\
                        GET    /kv/<key>\n\
                        DELETE /kv/<key>\n\
                        GET    /kv         (list keys)\n";
            write_response(&mut stream, 404, "Not Found", help).await
        }
    }
}

/// Write a minimal HTTP/1.1 response and close the connection.
async fn write_response(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// Index of the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
