use std::net::IpAddr;

use clap::Parser;
use ipnet::IpNet;
use rand::{
    distributions::{Alphanumeric, DistString},
    SeedableRng,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

use reconcile::{replicated_map::Config, ReplicatedMap};

#[derive(Parser)]
struct Args {
    port: u16,
    listen_addr: IpAddr,
    net: IpNet,
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
        net,
        seed,
        elements,
        log_level,
    } = Args::parse();
    let config = Config::default()
        .with_port(port)
        .with_listen_addr(listen_addr)
        .with_net(net)
        // Demo only: no cluster key flag here, so this loopback demo opts in explicitly. A real
        // deployment must set Config::with_cluster_key instead — see README "Security model".
        .with_insecure_no_key();
    tracing_subscriber::fmt().with_max_level(log_level).init();

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values: Vec<(String, String)> = vec![];
    for _ in 0..elements {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        key_values.push((key, value));
    }
    let key_values = key_values.as_slice();
    let mut service = ReplicatedMap::new(config)
        .await
        .expect("failed to bind UDP socket");
    service.insert_bulk(key_values);
    info!("Global fingerprint is {}", service.fingerprint(..));

    for seed in seed {
        service = service.with_seed(seed);
    }
    service.run(CancellationToken::new()).await;
}
