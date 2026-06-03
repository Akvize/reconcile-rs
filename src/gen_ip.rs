// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides utility methods to generate IP addresses from a CIDR.

use ipnet::{IpBitAnd, IpBitOr, IpNet, Ipv4Net, Ipv6Net};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use rand::Rng;

/// Select a random IP address from the given network
/// ```
/// # use rand::SeedableRng;
/// # use reconcile::gen_ip::gen_ip;
/// #
/// let mut rng = rand::rngs::StdRng::seed_from_u64(42);
/// let net = "192.168.1.0/24".parse().unwrap();
/// assert_eq!(gen_ip(&mut rng, net).to_string(), "192.168.1.162");
/// ```
pub fn gen_ip<R: Rng>(rng: &mut R, network: IpNet) -> IpAddr {
    match network {
        IpNet::V4(network) => IpAddr::V4(gen_ipv4(rng, network)),
        IpNet::V6(network) => IpAddr::V6(gen_ipv6(rng, network)),
    }
}

/// Select a random IPv4 address from the given network
/// ```
/// # use rand::SeedableRng;
/// # use reconcile::gen_ip::gen_ipv4;
/// #
/// let mut rng = rand::rngs::StdRng::seed_from_u64(42);
/// let net = "192.168.42.0/24".parse().unwrap();
/// assert_eq!(gen_ipv4(&mut rng, net).to_string(), "192.168.42.162");
/// ```
pub fn gen_ipv4<R: Rng>(rng: &mut R, network: Ipv4Net) -> Ipv4Addr {
    let random: Ipv4Addr = rng.gen::<u32>().into();
    network.network().bitor(random.bitand(network.hostmask()))
}

/// Select a random IPv6 address from the given network
/// ```
/// # use rand::SeedableRng;
/// # use reconcile::gen_ip::gen_ipv6;
/// #
/// let mut rng = rand::rngs::StdRng::seed_from_u64(42);
/// let net = "2001:db8::/32".parse().unwrap();
/// assert_eq!(gen_ipv6(&mut rng, net).to_string(), "2001:db8:3fad:517d:86cc:7763:2227:24a2");
/// ```
pub fn gen_ipv6<R: Rng>(rng: &mut R, network: Ipv6Net) -> Ipv6Addr {
    let random: Ipv6Addr = rng.gen::<u128>().into();
    network.network().bitor(random.bitand(network.hostmask()))
}

/// One random probe address per network CIDR.
///
/// Used for multi-network peer auto-discovery (issue #53): a node probes one random address inside
/// each configured geographical network every reconciliation round, so discovery spans every
/// network rather than a single flat CIDR.
pub fn probe_targets<R: Rng>(rng: &mut R, nets: &[IpNet]) -> Vec<IpAddr> {
    nets.iter().map(|&net| gen_ip(rng, net)).collect()
}

/// Return the first network in `nets` whose CIDR contains `addr`, if any.
///
/// A peer's geographical network is derived purely from its IP address, so the wire format carries
/// no network tag (issue #53).
pub fn net_of(nets: &[IpNet], addr: IpAddr) -> Option<IpNet> {
    nets.iter().copied().find(|net| net.contains(&addr))
}

/// The host route (`/32` for IPv4, `/128` for IPv6) of a single address.
///
/// Used as the local network of last resort: when no configured network contains a node's listen
/// address (a misconfiguration), the node treats only itself as local (issue #53).
pub fn host_net(addr: IpAddr) -> IpNet {
    let prefix_len = match addr {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    IpNet::new(addr, prefix_len).expect("host prefix length is always valid")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use rand::SeedableRng;

    use super::gen_ip;

    const N_ADDR: usize = 1000;

    #[test]
    fn rand_ipv4() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let net = "127.0.0.0/8".parse().unwrap();
        let addrs: HashSet<_> = (0..N_ADDR).map(|_| gen_ip(&mut rng, net)).collect();
        assert_eq!(addrs.len(), N_ADDR);
        for addr in addrs {
            assert!(net.contains(&addr), "{net} should contain {addr}");
        }
    }

    #[test]
    fn rand_ipv6() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let net = "2001:db8::/32".parse().unwrap();
        let addrs: HashSet<_> = (0..N_ADDR).map(|_| gen_ip(&mut rng, net)).collect();
        assert_eq!(addrs.len(), N_ADDR);
        for addr in addrs {
            assert!(net.contains(&addr), "{net} should contain {addr}");
        }
    }

    #[test]
    fn probe_targets_one_per_net() {
        use super::probe_targets;
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let nets = [
            "127.0.0.0/30".parse().unwrap(),
            "127.0.1.0/30".parse().unwrap(),
            "10.0.0.0/8".parse().unwrap(),
        ];
        let targets = probe_targets(&mut rng, &nets);
        assert_eq!(targets.len(), nets.len());
        for (target, net) in targets.iter().zip(nets.iter()) {
            assert!(net.contains(target), "{net} should contain {target}");
        }
    }

    #[test]
    fn net_classification() {
        use super::{host_net, net_of};
        let nets: Vec<_> = ["127.0.0.0/30", "127.0.1.0/30"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        // A peer is qualified by the first network whose CIDR contains it.
        assert_eq!(net_of(&nets, "127.0.0.1".parse().unwrap()), Some(nets[0]));
        assert_eq!(net_of(&nets, "127.0.1.1".parse().unwrap()), Some(nets[1]));
        // An address in no declared network is unqualified.
        assert_eq!(net_of(&nets, "10.0.0.1".parse().unwrap()), None);

        // The local-net-of-last-resort is the address' own host route.
        assert_eq!(
            host_net("10.0.0.1".parse().unwrap()),
            "10.0.0.1/32".parse().unwrap()
        );
        assert_eq!(host_net("::1".parse().unwrap()), "::1/128".parse().unwrap());
    }
}
