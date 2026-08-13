// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Optional Prometheus integration, enabled by the `metrics-prometheus` feature.
//!
//! The library emits through the [`metrics`] facade and never installs a recorder itself. These
//! helpers install one and either serve `/metrics` or hand back the exposition text.
//!
//! # Serving a `/metrics` endpoint
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! // Installs the recorder and spawns a background HTTP server exposing `/metrics`.
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

/// Install a global Prometheus recorder with no HTTP server; [`PrometheusHandle::render`] gives
/// the `/metrics` body.
///
/// # Errors
///
/// If a recorder is already installed — call this exactly once, early in `main`.
pub fn install_recorder() -> Result<PrometheusHandle, BuildError> {
    let handle = PrometheusBuilder::new().install_recorder()?;
    crate::observability::describe();
    Ok(handle)
}

/// Install the recorder and spawn a background HTTP server exposing `/metrics` at `addr`.
///
/// Requires a Tokio runtime, and returns once the listener is up. Call exactly once.
pub async fn serve(addr: SocketAddr) -> Result<(), BuildError> {
    PrometheusBuilder::new()
        .with_http_listener(addr)
        .install()?;
    crate::observability::describe();
    Ok(())
}
