use std::net::IpAddr;

use clap::Parser;
use ipnet::IpNet;
use rand::{
    distributions::{Alphanumeric, DistString},
    SeedableRng,
};
use tracing::info;

use reconcile::{service::ServiceConfig, Service};

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
    let mut key_values: Vec<(String, String)> = vec![];
    for _ in 0..elements {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        key_values.push((key, value));
    }
    let key_values = key_values.as_slice();
    let mut service = Service::new(config).await;
    service.insert_bulk(key_values);
    info!("Global fingerprint is {}", service.fingerprint(..));

    for seed in seed {
        service = service.with_seed(seed);
    }
    service.run().await;
}
