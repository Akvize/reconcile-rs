// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Dynamic peer discovery, behind a single port.
//!
//! Every way a [`ReconcileStore`](crate::ReconcileStore) learns about peers goes through the
//! [`Discovery`] port. Two built-in adapters implement it:
//!
//! - [`RandomProbe`] — the **default, speculative** source: one random address per declared network
//!   each round ([`Config::with_net`](crate::reconcile_store::Config::with_net)). The probed address
//!   might not be a live peer, so it is only used as a one-shot reconciliation target and is **not**
//!   added to the known-peer set. This is what auto-discovers peers on a flat or geographically
//!   partitioned CIDR. It is [non-authoritative](Discovery::is_authoritative).
//! - [`DnsDiscovery`] — an **authoritative** source for Kubernetes: it resolves a **headless
//!   Service** DNS name (one address record per ready pod), so a single lookup yields the real,
//!   current peer set, with no API client and no RBAC. Random probing does not fit Kubernetes, where
//!   pod IPs are ephemeral and drawn from a large cluster CIDR — a random probe almost never hits a
//!   live pod.
//!
//! An **authoritative** result is treated as the current truth: each returned address is seeded into
//! the known-peer set (via [`ReconcileStore::seed_peer`](crate::ReconcileStore::seed_peer)), and a
//! previously-seen member now absent is decommissioned after a grace period (see
//! [`ReconcileStore::with_dns_discovery`](crate::ReconcileStore::with_dns_discovery)). A
//! non-authoritative result is speculative and only steers the current round's targets. In **all**
//! cases discovery feeds the gossip-target set only; it never grants causal-stability *membership*,
//! which a peer must still earn through a genuine authenticated, dated datagram.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;

use ipnet::IpNet;
use parking_lot::RwLock;
use rand::rngs::StdRng;

use crate::gen_ip::probe_targets;

/// The future returned by [`Discovery::discover`].
///
/// A boxed future is used (rather than the `async_trait` crate) so the port stays object-safe —
/// it is always consumed behind `Arc<dyn Discovery>` — without pulling in an extra dependency.
pub type DiscoverFuture<'a> =
    Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send + 'a>>;

/// A source of candidate peer addresses for the reconciliation engine.
///
/// [`discover`](Self::discover) is called once per discovery round. The result is interpreted
/// according to [`is_authoritative`](Self::is_authoritative):
///
/// - `Ok(addrs)` from an **authoritative** source is a snapshot of the peers that should exist right
///   now: each address refreshes the corresponding known peer, and a previously-seen member that is
///   *absent* is counted toward grace-period decommissioning. From a **non-authoritative** source it
///   is speculative — only the current round's targets, never seeded as known peers.
/// - `Err(_)` is a **transient failure** (e.g. a DNS blip). It MUST NOT be read as "no peers": the
///   store skips the round entirely, so a momentary resolver hiccup never decommissions anyone.
pub trait Discovery: Send + Sync + 'static {
    /// Resolve the current candidate peer set.
    fn discover(&self) -> DiscoverFuture<'_>;

    /// Whether [`discover`](Self::discover) returns the *authoritative* current peer set (so an
    /// absence drives decommissioning), or merely *speculative* probe targets.
    ///
    /// Defaults to `true`; the speculative [`RandomProbe`] overrides it to `false`.
    fn is_authoritative(&self) -> bool {
        true
    }
}

/// The default, **speculative** discovery: one random address per declared network each round.
///
/// This is the historical auto-discovery for flat or geographically partitioned CIDRs (issue #53).
/// The probed addresses might not be live peers, so they are used only as one-shot reconciliation
/// targets and never seeded as known peers — if a peer exists there, it replies and is registered
/// then. It shares the engine's live `nets` and `rng`, so retuning the topology at runtime
/// immediately changes what is probed.
pub struct RandomProbe {
    nets: Arc<RwLock<Vec<IpNet>>>,
    rng: Arc<RwLock<StdRng>>,
}

impl RandomProbe {
    pub(crate) fn new(nets: Arc<RwLock<Vec<IpNet>>>, rng: Arc<RwLock<StdRng>>) -> Self {
        RandomProbe { nets, rng }
    }
}

impl Discovery for RandomProbe {
    fn discover(&self) -> DiscoverFuture<'_> {
        // Sample synchronously (no lock is held across the returned, already-ready future).
        let nets = self.nets.read().clone();
        let targets = probe_targets(&mut *self.rng.write(), &nets);
        Box::pin(async move { Ok(targets) })
    }

    fn is_authoritative(&self) -> bool {
        false
    }
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

    #[test]
    fn dns_discovery_is_authoritative() {
        assert!(DnsDiscovery::new("svc", 0).is_authoritative());
    }

    #[tokio::test]
    async fn random_probe_is_speculative_and_in_network() {
        use rand::SeedableRng;
        let net: IpNet = "127.0.0.0/8".parse().unwrap();
        let probe = RandomProbe::new(
            Arc::new(RwLock::new(vec![net])),
            Arc::new(RwLock::new(StdRng::seed_from_u64(42))),
        );
        // Speculative: an absence must never decommission a peer.
        assert!(!probe.is_authoritative());
        // One probe per declared network, each within that network.
        let addrs = probe.discover().await.unwrap();
        assert_eq!(addrs.len(), 1);
        assert!(net.contains(&addrs[0]), "{net} should contain {}", addrs[0]);
    }
}
