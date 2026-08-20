// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`ReadReplicaSet`]: a read-only replica of a [`ReplicatedSet`](crate::ReplicatedSet), mirroring
//! [`ReadReplicaMap`] the way [`ReplicatedSet`](crate::ReplicatedSet) mirrors
//! [`ReplicatedMap`](crate::ReplicatedMap).
//!
//! A thin newtype over [`ReadReplicaMap<K, ()>`](crate::ReadReplicaMap), not a reimplementation:
//! same wire format and convergence semantics. It exposes membership (`contains`, `len`,
//! `is_empty`, `keys`) instead of the value-shaped read API (`get`, `values`, `to_vec`,
//! `first_key_value`/`last_key_value`, `for_each*`) — none of it has a meaningful reading when
//! every value is `()`. Reach for `ReadReplicaMap<K, ()>` directly if one of those is genuinely
//! needed.

use std::io;
use std::net::IpAddr;
use std::ops::RangeBounds;

use ipnet::IpNet;

use crate::bounds::Key;
use crate::entry::State;
use crate::read_replica_map::ReadReplicaMap;
use crate::replicated_map::Config;
use rsos::Fingerprint;

/// A read-only replica of a replicated set; see the
/// [module documentation](crate::read_replica_set).
///
/// ```
/// use reconcile::{replicated_map::Config, ReadReplicaSet};
///
/// # #[tokio::main]
/// # async fn main() -> std::io::Result<()> {
/// let set = ReadReplicaSet::<String>::new(Config::new(8083).with_insecure_no_key()).await?;
///
/// // Read-only: nothing arrives until it reconciles with a dated peer (module docs).
/// assert!(!set.contains(&"a".to_string()));
/// # Ok(())
/// # }
/// ```
pub struct ReadReplicaSet<K>(ReadReplicaMap<K, ()>);

impl<K> Clone for ReadReplicaSet<K> {
    /// Allows cloning the `ReadReplicaSet` handle for lightweight sharing in hooks or tests.
    fn clone(&self) -> Self {
        ReadReplicaSet(self.0.clone())
    }
}

impl<K: Key> ReadReplicaSet<K> {
    /// Create a read replica bound to the configured UDP socket. See [`ReadReplicaMap::new`].
    ///
    /// # Errors
    ///
    /// If the socket cannot be bound to `(config.listen_addr, config.port)`.
    pub async fn new(config: Config) -> io::Result<Self> {
        ReadReplicaMap::new(config).await.map(ReadReplicaSet)
    }

    /// Provide the address of a known dated peer. See [`ReadReplicaMap::with_seed`].
    #[must_use]
    pub fn with_seed(self, peer: IpAddr) -> Self {
        ReadReplicaSet(self.0.with_seed(peer))
    }

    /// (runtime) Retune the probed network. See [`ReadReplicaMap::set_net`].
    pub fn set_net(&self, net: IpNet) {
        self.0.set_net(net);
    }

    /// The network this read replica currently probes. See [`ReadReplicaMap::net`].
    #[must_use]
    pub fn net(&self) -> IpNet {
        self.0.net()
    }

    /// Set the hook invoked before each inbound membership change. See
    /// [`ReadReplicaMap::set_on_update`].
    pub fn set_on_update<F: Send + Sync + Fn(&K, &State<()>) + 'static>(&self, on_update: F) {
        self.0.set_on_update(on_update);
    }

    /// Whether `key` is currently a member, as observed from the dated peer.
    #[must_use]
    pub fn contains(&self, key: &K) -> bool {
        self.0.contains_key(key)
    }

    /// Number of members currently held. See [`ReadReplicaMap::len`].
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the read replica holds no member. See [`ReadReplicaMap::is_empty`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Fingerprint over a key range. See [`ReadReplicaMap::value_fingerprint`].
    #[must_use]
    pub fn value_fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.0.value_fingerprint(range)
    }

    /// Deprecated alias for [`value_fingerprint`](Self::value_fingerprint) (#294).
    #[deprecated(since = "1.0.0", note = "renamed to `value_fingerprint`")]
    #[must_use]
    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.value_fingerprint(range)
    }

    /// The current members. See [`ReadReplicaMap::keys`].
    #[must_use]
    pub fn keys(&self) -> Vec<K> {
        self.0.keys()
    }

    /// Start an out-of-cadence reconciliation round. See
    /// [`ReadReplicaMap::start_reconciliation`].
    pub async fn start_reconciliation(&self) {
        self.0.start_reconciliation().await;
    }

    /// Run the reconciliation loop. See [`ReadReplicaMap::run`].
    pub async fn run(self) {
        self.0.run().await;
    }
}

#[cfg(test)]
mod read_replica_set_tests {
    use std::time::Duration;

    use crate::read_replica_set::ReadReplicaSet;
    use crate::replicated_map::{Config, MAX_NETS};
    use rsos::Fingerprint;

    fn ephemeral_config() -> Config {
        Config {
            port: crate::replica::tests::next_ephemeral_test_port(),
            listen_addr: "127.0.0.1".parse().unwrap(),
            nets: [None; MAX_NETS],
            remote_interval: 6,
            remote_fanout: 2,
            cluster_key: None,
            insecure_no_key: true,
            node_id: None,
            encrypt: false,
            reconcile_interval: Duration::from_secs(1),
            bulk_send_rate: Some(32 * 1024 * 1024),
            recv_buffer_size: Some(8 * 1024 * 1024),
            send_buffer_size: Some(8 * 1024 * 1024),
            freshness_window: gossip::replay::FRESHNESS_WINDOW_DEFAULT,
            max_peers: 1024,
            max_concurrent_bulk_dumps: 4,
            snapshot_interval: Duration::from_secs(5),
            max_clock_drift: crate::clock::MAX_CLOCK_DRIFT,
        }
    }

    /// #377: a freshly constructed `ReadReplicaSet` holds no member, and `contains`/`len`/
    /// `is_empty`/`keys` agree on that.
    #[tokio::test]
    async fn fresh_replica_has_no_members() {
        let replica = ReadReplicaSet::<i32>::new(ephemeral_config())
            .await
            .unwrap();

        assert!(replica.is_empty());
        assert_eq!(replica.len(), 0);
        assert!(!replica.contains(&1));
        assert!(replica.keys().is_empty());
    }

    /// #294: after converging with a real dated peer, `value_fingerprint` must actually reflect
    /// the received members (not a default/zero `Fingerprint`), and the deprecated `fingerprint`
    /// alias must forward to it — a mutant that no-ops either would pass any test that never
    /// compares its result to the real, non-default value.
    #[tokio::test(flavor = "multi_thread")]
    #[allow(deprecated)]
    async fn value_fingerprint_and_its_deprecated_alias_reflect_converged_content() {
        use crate::replicated_set::ReplicatedSet;
        use tokio_util::sync::CancellationToken;

        let port = crate::replica::tests::next_ephemeral_test_port();
        let net: ipnet::IpNet = "127.0.6.0/24".parse().unwrap();
        let dated_addr: std::net::IpAddr = "127.0.6.10".parse().unwrap();
        let replica_addr: std::net::IpAddr = "127.0.6.11".parse().unwrap();

        let dated = ReplicatedSet::<i32>::new(
            Config::default()
                .with_port(port)
                .with_listen_addr(dated_addr)
                .with_net(net)
                .with_insecure_no_key(),
        )
        .await
        .expect("bind failed");
        assert!(!dated.insert(7), "key 7 must be newly inserted");

        let replica = ReadReplicaSet::<i32>::new(
            Config::default()
                .with_port(port)
                .with_listen_addr(replica_addr)
                .with_net(net)
                .with_insecure_no_key(),
        )
        .await
        .expect("bind failed")
        .with_seed(dated_addr);

        let dated_task = tokio::spawn(dated.clone().run(CancellationToken::new()));
        let replica_task = tokio::spawn(replica.clone().run());

        let mut converged = false;
        for _ in 0..300 {
            if replica.contains(&7) {
                converged = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        dated_task.abort();
        replica_task.abort();
        assert!(converged, "replica never observed the dated peer's member");

        let value_fingerprint = replica.value_fingerprint(..);
        assert_ne!(
            value_fingerprint,
            Fingerprint::default(),
            "a converged, non-empty set must not fingerprint as empty"
        );
        assert_eq!(replica.fingerprint(..), value_fingerprint);
    }

    /// #294: `start_reconciliation` (the public wrapper) must actually send a value-only
    /// comparison round to the seeded peer — a mutant that no-ops its body would leave the peer's
    /// socket silent forever, which this test's `recv_from` would then time out on. No `run()`
    /// loop is spawned on either side, so nothing but this call can produce the datagram.
    #[tokio::test]
    async fn start_reconciliation_wrapper_actually_transmits() {
        let port = crate::replica::tests::next_ephemeral_test_port();
        let net: ipnet::IpNet = "127.0.6.0/24".parse().unwrap();
        let replica_addr: std::net::IpAddr = "127.0.6.20".parse().unwrap();
        let peer_addr: std::net::IpAddr = "127.0.6.21".parse().unwrap();

        let peer_socket = tokio::net::UdpSocket::bind((peer_addr, port))
            .await
            .expect("peer bind failed");

        let replica = ReadReplicaSet::<i32>::new(
            Config::default()
                .with_port(port)
                .with_listen_addr(replica_addr)
                .with_net(net)
                .with_insecure_no_key(),
        )
        .await
        .expect("bind failed")
        .with_seed(peer_addr);

        replica.start_reconciliation().await;

        let mut buf = [0u8; 65536];
        let (size, from) =
            tokio::time::timeout(Duration::from_secs(5), peer_socket.recv_from(&mut buf))
                .await
                .expect("start_reconciliation never sent anything to the seeded peer")
                .expect("recv_from failed");
        assert!(size > 0, "the datagram sent to the peer was empty");
        assert_eq!(from.ip(), replica_addr);
    }
}
