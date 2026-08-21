// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::net::IpAddr;
use std::time::Duration;

use ipnet::IpNet;

use crate::bounds::{Key, Value};

use super::ReadReplicaMap;

const PEER_EXPIRATION: Duration = Duration::from_secs(60);

impl<K: Key, V: Value> ReadReplicaMap<K, V> {
    /// (runtime) Retune the network this read replica probes for discovery, visible to all clones.
    pub fn set_net(&self, net: IpNet) {
        *self.net.write() = net;
    }

    /// The network this read replica currently probes for discovery.
    pub fn net(&self) -> IpNet {
        *self.net.read()
    }

    pub(super) fn get_peers(&self) -> Vec<IpAddr> {
        let mut guard = self.peers.write();
        guard.retain(|_, instant| instant.elapsed() < PEER_EXPIRATION);
        guard.keys().cloned().collect()
    }
}
