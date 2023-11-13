use std::net::IpAddr;

use clap::Parser;
use ipnet::IpNet;
use rand::{
    distributions::{Alphanumeric, DistString},
    SeedableRng,
};
use tracing::info;

use reconcile::{HRTree, HashRangeQueryable, Service};

#[derive(Parser)]
struct Args {
    port: u16,
    listen_addr: IpAddr,
    peer_net: IpNet,
    other_addr: IpAddr,
    elements: usize,
    #[arg(short, long, default_value_t = tracing::Level::INFO)]
    log_level: tracing::Level,
}

#[tokio::main]
async fn main() {
    let Args {
        port,
        listen_addr,
        peer_net,
        other_addr,
        elements,
        log_level,
    } = Args::parse();

    tracing_subscriber::fmt().with_max_level(log_level).init();

    // build collection
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..elements {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let time = chrono::offset::Utc::now();
        let value = Alphanumeric.sample_string(&mut rng, 1000);
        key_values.push((key, (time, value)));
    }
    let tree = HRTree::from_iter(key_values);
    info!("Global hash is {}", tree.hash(&..));

    let service = Service::new(tree, port, listen_addr, peer_net)
        .await
        .with_seed(other_addr);
    service.run(|_k, _v, _old_v| ()).await;
}
