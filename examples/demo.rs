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

    tracing_subscriber::fmt().with_max_level(log_level).init();

    let tree = HRTree::new();
    let mut service = Service::new(tree, port, listen_addr, peer_net).await;

    // build collection
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..elements {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let time = chrono::offset::Utc::now();
        let value = Alphanumeric.sample_string(&mut rng, 1000);
        key_values.push((key, (time, value)));
    }
    key_values.sort();
    for (key, (time, value)) in key_values {
        service.insert(key, value, time);
    }
    info!("Global hash is {}", service.read().hash(&..));

    for seed in seed {
        service = service.with_seed(seed);
    }
    service.run(|_k, _v, _old_v| ()).await;
}
