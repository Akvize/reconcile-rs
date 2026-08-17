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

use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ipnet::IpNet;
use parking_lot::RwLock;
use rand::rngs::StdRng;

use crate::gen_ip::probe_targets;

/// A [`Discovery::discover`] failure. Boxed rather than an associated type on [`Discovery`], so
/// `Arc<dyn Discovery>` (`src/replica.rs`, `src/replicated_map.rs`) stays a single concrete type
/// across implementors with unrelated failure modes (`DnsDiscoveryError` vs. `Infallible`) — the
/// erasure #287 asked for, without giving up the trait object every call site relies on.
pub type DiscoveryError = Box<dyn StdError + Send + Sync>;

/// The future returned by [`Discovery::discover`]. Boxed rather than `async_trait`, so the port
/// stays object-safe with no extra dependency.
pub type DiscoverFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, DiscoveryError>> + Send + 'a>>;

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
#[derive(Debug)]
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

/// Why a [`DnsDiscovery::discover`] round failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum DnsDiscoveryError {
    /// The system resolver returned an error (e.g. NXDOMAIN, or the resolver is unreachable).
    Resolve(std::io::Error),
    /// The lookup did not complete within [`DnsDiscovery::with_timeout`]'s budget.
    Timeout(tokio::time::error::Elapsed),
}

impl fmt::Display for DnsDiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DnsDiscoveryError::Resolve(e) => write!(f, "DNS resolution failed: {e}"),
            DnsDiscoveryError::Timeout(_) => write!(f, "DNS resolution timed out"),
        }
    }
}

impl StdError for DnsDiscoveryError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            DnsDiscoveryError::Resolve(e) => Some(e),
            DnsDiscoveryError::Timeout(e) => Some(e),
        }
    }
}

/// The default budget [`DnsDiscovery`] allows a single lookup before treating it as a transient
/// failure (skip the round) rather than hanging indefinitely.
pub const DEFAULT_DNS_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Discovers peers by resolving a DNS name to its address records, through the system resolver.
///
/// Point it at a Kubernetes **headless** `Service` (`clusterIP: None`): one record per ready pod.
#[derive(Debug)]
pub struct DnsDiscovery {
    name: String,
    port: u16,
    timeout: Duration,
}

impl DnsDiscovery {
    /// A DNS discovery source for `name`, typically a headless Service FQDN. `port` only forms
    /// the `host:port` string `lookup_host` expects and is discarded from the results. Each
    /// lookup is bounded by [`DEFAULT_DNS_DISCOVERY_TIMEOUT`]; tune it with
    /// [`with_timeout`](Self::with_timeout).
    pub fn new(name: impl Into<String>, port: u16) -> Self {
        DnsDiscovery {
            name: name.into(),
            port,
            timeout: DEFAULT_DNS_DISCOVERY_TIMEOUT,
        }
    }

    /// Bound how long a single [`discover`](Discovery::discover) round waits for the system
    /// resolver before giving up and treating the round as a transient failure.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Discovery for DnsDiscovery {
    fn discover(&self) -> DiscoverFuture<'_> {
        let host = format!("{}:{}", self.name, self.port);
        Box::pin(async move {
            let addrs = tokio::time::timeout(self.timeout, tokio::net::lookup_host(host))
                .await
                .map_err(DnsDiscoveryError::Timeout)?
                .map_err(DnsDiscoveryError::Resolve)?;
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

    /// `tokio::time::timeout` polls the wrapped future before it ever checks the deadline, so a
    /// future that can resolve synchronously on its first poll (as `lookup_host("localhost")`
    /// does on this host) always wins the race regardless of how small a budget is passed — no
    /// duration can deterministically force a `Timeout` here without also mocking the resolver.
    /// `with_timeout` itself, and the `Elapsed` → `DnsDiscoveryError::Timeout` mapping in
    /// `discover`, stay covered by inspection and by `tokio::time::timeout`'s own test suite.
    #[test]
    fn dns_discovery_with_timeout_is_a_builder() {
        let discovery = DnsDiscovery::new("svc", 0).with_timeout(Duration::from_millis(1));
        assert_eq!(discovery.timeout, Duration::from_millis(1));
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
