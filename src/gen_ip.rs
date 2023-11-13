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

pub fn gen_ip<R: Rng>(rng: &mut R, network: IpNet) -> IpAddr {
    match network {
        IpNet::V4(network) => IpAddr::V4(gen_ipv4(rng, network)),
        IpNet::V6(network) => IpAddr::V6(gen_ipv6(rng, network)),
    }
}

pub fn gen_ipv4<R: Rng>(rng: &mut R, network: Ipv4Net) -> Ipv4Addr {
    let random: Ipv4Addr = rng.gen::<u32>().into();
    network.network().bitor(random.bitand(network.hostmask()))
}

pub fn gen_ipv6<R: Rng>(rng: &mut R, network: Ipv6Net) -> Ipv6Addr {
    let random: Ipv6Addr = rng.gen::<u128>().into();
    network.network().bitor(random.bitand(network.hostmask()))
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
}
