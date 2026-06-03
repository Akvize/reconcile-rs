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

/// One random probe address per region CIDR.
///
/// Used for multi-region peer auto-discovery (issue #53): a node probes one random address inside
/// each configured geographical region every reconciliation round, so discovery spans every region
/// rather than a single flat CIDR.
pub fn probe_targets<R: Rng>(rng: &mut R, regions: &[IpNet]) -> Vec<IpAddr> {
    regions.iter().map(|&net| gen_ip(rng, net)).collect()
}

/// Return the first region in `regions` whose CIDR contains `addr`, if any.
///
/// A peer's geographical region is derived purely from its IP address, so the wire format carries
/// no region tag (issue #53).
pub fn region_of(regions: &[IpNet], addr: IpAddr) -> Option<IpNet> {
    regions.iter().copied().find(|net| net.contains(&addr))
}

/// The region a node belongs to, given the address it listens on.
///
/// It is the first configured region whose CIDR contains `listen_addr`; if none does (a
/// misconfiguration), it falls back to the first region. `regions` must be non-empty (the local
/// region is always its first element).
pub fn local_region(regions: &[IpNet], listen_addr: IpAddr) -> IpNet {
    region_of(regions, listen_addr).unwrap_or(regions[0])
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
    fn probe_targets_one_per_region() {
        use super::probe_targets;
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let regions = [
            "127.0.0.0/30".parse().unwrap(),
            "127.0.1.0/30".parse().unwrap(),
            "10.0.0.0/8".parse().unwrap(),
        ];
        let targets = probe_targets(&mut rng, &regions);
        assert_eq!(targets.len(), regions.len());
        for (target, region) in targets.iter().zip(regions.iter()) {
            assert!(region.contains(target), "{region} should contain {target}");
        }
    }

    #[test]
    fn region_classification() {
        use super::{local_region, region_of};
        let regions: Vec<_> = ["127.0.0.0/30", "127.0.1.0/30"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(
            region_of(&regions, "127.0.0.1".parse().unwrap()),
            Some(regions[0])
        );
        assert_eq!(
            region_of(&regions, "127.0.1.1".parse().unwrap()),
            Some(regions[1])
        );
        assert_eq!(region_of(&regions, "10.0.0.1".parse().unwrap()), None);

        // The local region is the one containing the listen address.
        assert_eq!(
            local_region(&regions, "127.0.1.2".parse().unwrap()),
            regions[1]
        );
        // Fallback to the first region when no region contains the listen address.
        assert_eq!(
            local_region(&regions, "10.0.0.1".parse().unwrap()),
            regions[0]
        );
    }
}
