// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Optional Prometheus integration, enabled by the `metrics-prometheus` feature.
//!
//! The library emits metrics through the [`metrics`] facade but, just like a `tracing`
//! subscriber, it never installs a recorder on its own — that is the application's choice.
//! These helpers let an application wire up a Prometheus recorder and either serve a
//! `/metrics` HTTP endpoint or render the exposition text itself.
//!
//! The metrics themselves are only emitted when the `metrics` feature is enabled;
//! `metrics-prometheus` implies it.
//!
//! # Exposure caveat
//!
//! The `/metrics` endpoint is **unauthenticated** and exposes operational information — dataset
//! size and churn, byte/datagram counters, peer and reconciliation activity — that an observer can
//! use to fingerprint your deployment. The examples bind `0.0.0.0:9000` for convenience, which
//! listens on **every** interface. In production, bind it to `127.0.0.1` (or a trusted management
//! interface) and let your scrape agent reach it locally, and/or restrict it with a firewall or a
//! Kubernetes `NetworkPolicy`. Do not expose it on an untrusted network.
//!
//! # Serving a `/metrics` endpoint
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! // Installs the recorder and spawns a background HTTP server exposing `/metrics`.
//! // `0.0.0.0` listens on every interface — see the exposure caveat above; prefer a trusted
//! // interface (e.g. `127.0.0.1:9000`) in production.
//! reconcile::prometheus::serve("0.0.0.0:9000".parse()?).await?;
//! // ... then start your store: `store.run().await;`
//! # Ok(())
//! # }
//! ```
//!
//! # Rendering the exposition text yourself (configurable hook)
//!
//! ```no_run
//! # fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let handle = reconcile::prometheus::install_recorder()?;
//! // Serve `handle.render()` through your own HTTP stack whenever Prometheus scrapes.
//! let body: String = handle.render();
//! # let _ = body;
//! # Ok(())
//! # }
//! ```

use std::net::SocketAddr;

use metrics_exporter_prometheus::{BuildError, PrometheusBuilder, PrometheusHandle};

/// Install a global Prometheus recorder **without** any HTTP server and return a handle.
///
/// Call [`PrometheusHandle::render`] on the returned handle to obtain the text-format
/// `/metrics` body — the "configurable hook" integration path, for applications that
/// already run their own HTTP server.
///
/// Call this exactly once, early in `main`. Returns an error if a recorder is already
/// installed.
pub fn install_recorder() -> Result<PrometheusHandle, BuildError> {
    let handle = PrometheusBuilder::new().install_recorder()?;
    crate::observability::describe();
    Ok(handle)
}

/// Install the recorder **and** spawn a background HTTP server exposing `/metrics` at `addr`.
///
/// Requires a Tokio runtime (the listener is spawned onto the current runtime). Returns once
/// the listener is set up; the server then runs in the background. Call this exactly once,
/// early in `main`, before starting the reconciliation loop.
///
/// The endpoint is unauthenticated and leaks operational information; choose `addr` accordingly
/// (prefer `127.0.0.1`/a trusted interface over `0.0.0.0`). See the module-level exposure caveat.
pub async fn serve(addr: SocketAddr) -> Result<(), BuildError> {
    PrometheusBuilder::new()
        .with_http_listener(addr)
        .install()?;
    crate::observability::describe();
    Ok(())
}
