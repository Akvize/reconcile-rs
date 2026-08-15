// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Dynamic peer discovery behind the [`Discovery`] port (`ARCHITECTURE.md` §3.2).
//!
//! - [`RandomProbe`] — default, **speculative**: one random address per declared network per
//!   round, steering only that round's targets.
//! - [`DnsDiscovery`] — **authoritative**, for a Kubernetes headless Service: one address record
//!   per ready pod, seeded into the known-peer set, with absence decommissioning after a grace
//!   period.
//!
//! Either way discovery feeds the gossip-target set only, never causal-stability membership
//! (`ARCHITECTURE.md` §5 invariant 6).

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;

use ipnet::IpNet;
use parking_lot::RwLock;
use rand::rngs::StdRng;

use crate::gen_ip::probe_targets;

/// The future returned by [`Discovery::discover`]. Boxed rather than `async_trait`, so the port
/// stays object-safe with no extra dependency.
pub type DiscoverFuture<'a> =
    Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send + 'a>>;

/// Whether a [`Discovery`] source's result is the current truth or merely a hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryKind {
    /// A snapshot of the peers that should exist now: an absent member counts toward
    /// grace-period decommissioning.
    Authoritative,
    /// `discover`'s result is speculative — only the current round's targets, never seeded as
    /// known peers.
    Speculative,
}

/// A source of candidate peer addresses, called once per discovery round and read according to
/// [`kind`](Self::kind).
///
/// `Err(_)` is a **transient failure**, never "no peers": the store skips the round, so a resolver
/// hiccup decommissions nobody.
pub trait Discovery: Send + Sync + 'static {
    /// Resolve the current candidate peer set.
    fn discover(&self) -> DiscoverFuture<'_>;

    /// How [`discover`](Self::discover)'s result is read.
    ///
    /// No default: an implementor who does not think about this returns the dangerous choice by
    /// accident rather than the safe one. [`Authoritative`](DiscoveryKind::Authoritative) seeds
    /// every result as a permanent known peer and decommissions members on its absences —
    /// [`Speculative`](DiscoveryKind::Speculative) is the fail-safe default to reach for when in
    /// doubt.
    fn kind(&self) -> DiscoveryKind;
}

/// The default, **speculative** discovery: one random address per declared network each round,
/// never seeded as a known peer. Shares the engine's live `nets`/`rng`, so retuning the topology
/// changes what is probed.
pub struct RandomProbe {
    nets: Arc<RwLock<Vec<IpNet>>>,
    rng: Arc<RwLock<StdRng>>,
}

impl RandomProbe {
    /// Wrap the engine's live `nets`/`rng` handles. No copy is taken: retuning either through the
    /// shared lock changes what a subsequent [`discover`](Discovery::discover) probes.
    pub fn new(nets: Arc<RwLock<Vec<IpNet>>>, rng: Arc<RwLock<StdRng>>) -> Self {
        RandomProbe { nets, rng }
    }
}

impl Discovery for RandomProbe {
    fn discover(&self) -> DiscoverFuture<'_> {
        let nets = self.nets.read().clone();
        let targets = probe_targets(&mut *self.rng.write(), &nets);
        Box::pin(async move { Ok(targets) })
    }

    fn kind(&self) -> DiscoveryKind {
        DiscoveryKind::Speculative
    }
}

/// Discovers peers by resolving a DNS name to its address records, through the system resolver.
///
/// Point it at a Kubernetes **headless** `Service` (`clusterIP: None`): one record per ready pod.
pub struct DnsDiscovery {
    name: String,
    port: u16,
}

impl DnsDiscovery {
    /// A DNS discovery source for `name`, typically a headless Service FQDN. `port` only forms
    /// the `host:port` string `lookup_host` expects and is discarded from the results.
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

    fn kind(&self) -> DiscoveryKind {
        DiscoveryKind::Authoritative
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dns_discovery_resolves_loopback() {
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
        let discovery = DnsDiscovery::new("this-name-should-not-resolve.invalid", 0);
        assert!(discovery.discover().await.is_err());
    }

    #[test]
    fn dns_discovery_is_authoritative() {
        assert_eq!(
            DnsDiscovery::new("svc", 0).kind(),
            DiscoveryKind::Authoritative
        );
    }

    #[tokio::test]
    async fn random_probe_is_speculative_and_in_network() {
        use rand::SeedableRng;
        let net: IpNet = "127.0.0.0/8".parse().unwrap();
        let probe = RandomProbe::new(
            Arc::new(RwLock::new(vec![net])),
            Arc::new(RwLock::new(StdRng::seed_from_u64(42))),
        );
        assert_eq!(probe.kind(), DiscoveryKind::Speculative);
        let addrs = probe.discover().await.unwrap();
        assert_eq!(addrs.len(), 1);
        assert!(net.contains(&addrs[0]), "{net} should contain {}", addrs[0]);
    }
}
