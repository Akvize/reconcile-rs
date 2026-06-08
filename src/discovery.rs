// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Dynamic peer discovery for cloud-native deployments.
//!
//! By default a [`ReconcileStore`](crate::ReconcileStore) discovers peers by probing random
//! addresses out of the declared networks ([`Config::with_net`](crate::reconcile_store::Config::with_net)).
//! That works for flat or geographically partitioned CIDRs, but not for Kubernetes, where pod IPs
//! are ephemeral and drawn from a large cluster CIDR — a random probe almost never hits a live pod.
//!
//! This module adds an **optional, additive** discovery source: an implementation of the
//! [`Discovery`] port returns the *current* set of peer addresses, which the store injects into its
//! known-peer set every round (via [`ReconcileStore::seed_peer`](crate::ReconcileStore::seed_peer)).
//! The built-in [`DnsDiscovery`] adapter resolves a **headless Service** DNS name (one address
//! record per ready pod) — the canonical Kubernetes StatefulSet peer-discovery pattern, requiring
//! no API client and no RBAC.
//!
//! Discovery only ever feeds the gossip-target set; it never grants causal-stability *membership*,
//! which a peer must still earn through a genuine authenticated, dated datagram. See
//! [`ReconcileStore::with_dns_discovery`](crate::ReconcileStore::with_dns_discovery) for the
//! grace-period decommissioning of pods that disappear from DNS.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;

/// The future returned by [`Discovery::discover`].
///
/// A boxed future is used (rather than the `async_trait` crate) so the port stays object-safe —
/// it is always consumed behind `Arc<dyn Discovery>` — without pulling in an extra dependency.
pub type DiscoverFuture<'a> =
    Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send + 'a>>;

/// A source of candidate peer addresses for the reconciliation engine.
///
/// [`discover`](Self::discover) is called once per discovery round. The result is interpreted as
/// follows:
///
/// - `Ok(addrs)` is an **authoritative snapshot** of the peers that should exist right now. Any
///   address present refreshes the corresponding known peer; a previously-seen member that is
///   *absent* from the snapshot is counted toward grace-period decommissioning.
/// - `Err(_)` is a **transient failure** (e.g. a DNS blip). It MUST NOT be read as "no peers": the
///   store skips the round entirely, so a momentary resolver hiccup never decommissions anyone.
pub trait Discovery: Send + Sync + 'static {
    /// Resolve the current candidate peer set.
    fn discover(&self) -> DiscoverFuture<'_>;
}

/// Discovers peers by resolving a DNS name to its set of address records.
///
/// Point this at a Kubernetes **headless** `Service` (`clusterIP: None`): its DNS name resolves to
/// one A/AAAA record per ready pod, so a single lookup yields every peer. Resolution uses
/// [`tokio::net::lookup_host`], i.e. the system resolver (`getaddrinfo`) — no extra dependency, and
/// it honours the in-cluster DNS configuration.
pub struct DnsDiscovery {
    name: String,
    port: u16,
}

impl DnsDiscovery {
    /// Create a DNS-based discovery source for `name` (a resolvable host, typically a headless
    /// Service FQDN such as `reconcile-headless.default.svc.cluster.local`). `port` is the
    /// reconciliation UDP port; it is only used to form the `host:port` string `lookup_host`
    /// expects and is discarded from the returned addresses.
    pub fn new(name: impl Into<String>, port: u16) -> Self {
        DnsDiscovery {
            name: name.into(),
            port,
        }
    }
}

impl Discovery for DnsDiscovery {
    fn discover(&self) -> DiscoverFuture<'_> {
        let host = format!("{}:{}", self.name, self.port);
        Box::pin(async move {
            let addrs = tokio::net::lookup_host(host).await?;
            Ok(addrs.map(|sock_addr| sock_addr.ip()).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dns_discovery_resolves_loopback() {
        // `localhost` resolves to a loopback address through the system resolver; this is a smoke
        // test that the adapter wires `lookup_host` up correctly and strips the port.
        let discovery = DnsDiscovery::new("localhost", 0);
        let addrs = discovery
            .discover()
            .await
            .expect("localhost should resolve");
        assert!(
            addrs.iter().any(|ip| ip.is_loopback()),
            "expected a loopback address, got {addrs:?}"
        );
    }

    #[tokio::test]
    async fn dns_discovery_errors_on_unresolvable_name() {
        // A syntactically valid but unresolvable name yields an `Err`, which the store treats as a
        // transient blip rather than an empty peer set.
        let discovery = DnsDiscovery::new("this-name-should-not-resolve.invalid", 0);
        assert!(discovery.discover().await.is_err());
    }
}
