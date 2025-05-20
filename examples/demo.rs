use std::net::IpAddr;

use chrono::Utc;
use clap::Parser;
use ipnet::IpNet;
use rand::{
    distributions::{Alphanumeric, DistString},
    SeedableRng,
};
use tracing::info;

use reconcile::{service::ServiceConfig, DatedMaybeTombstone, HRTree, HashRangeQueryable, Service};

#[derive(Parser)]
struct Args {
    port: u16,
    listen_addr: IpAddr,
    peer_net: IpNet,
    elements: usize,
    #[arg(short, long)]
    seed: Vec<IpAddr>,
    #[arg(short, long, default_value_t = tracing::Level::INFO)]
    log_level: tracing::Level,
}

#[tokio::main]
async fn main() {
    let Args {
        port,
        listen_addr,
        peer_net,
        seed,
        elements,
        log_level,
    } = Args::parse();
    let config = ServiceConfig::default()
        .with_port(port)
        .with_listen_addr(listen_addr)
        .with_peer_net(peer_net);
    tracing_subscriber::fmt().with_max_level(log_level).init();

    // build collection
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..elements {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: DatedMaybeTombstone<String> =
            (Utc::now(), Some(Alphanumeric.sample_string(&mut rng, 100)));
        key_values.push((key, value));
    }
    let tree = HRTree::from_iter(key_values.into_iter());
    info!("Global hash is {}", tree.hash(&..));

    let mut service = Service::new(tree, config).await;

    for seed in seed {
        service = service.with_seed(seed);
    }
    service.run().await;
}
