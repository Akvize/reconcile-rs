use std::net::SocketAddr;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use clap::Parser;
use diff::HashRangeQueryable;
use htree::HTree;
use rand::{
    distributions::{Alphanumeric, DistString},
    SeedableRng,
};

use reconcilable::rhtree::RHTree;
use reconcile_service;
use tokio::net::UdpSocket;
use tracing::{debug, info};

#[derive(Parser)]
struct Args {
    listen_addr: SocketAddr,
    other_addr: SocketAddr,
    elements: usize,
    #[arg(short, long, default_value_t = tracing::Level::INFO)]
    log_level: tracing::Level,
}

#[tokio::main]
async fn main() {
    let Args {
        listen_addr,
        other_addr,
        elements,
        log_level,
    } = Args::parse();

    tracing_subscriber::fmt().with_max_level(log_level).init();

    let socket = UdpSocket::bind(listen_addr).await.unwrap();
    info!("Listening on: {}", socket.local_addr().unwrap());

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..elements {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value = chrono::offset::Utc::now().to_string();
        key_values.push((key, value));
    }
    let tree = HTree::from_iter(key_values.into_iter());

    info!("Global hash is {}", tree.hash(&..));
    let conflict_handler = |k: &String, local_v: &String, v: String| -> Option<String> {
        if DateTime::<Utc>::from_str(local_v).unwrap() > DateTime::<Utc>::from_str(&v).unwrap() {
            debug!("Key {k} - Keeping local value {local_v}, dropping remote value {v}");
            return None;
        }
        debug!("Key {k} - Replacing local value {local_v} with remote value {v}");
        Some(v)
    }; // Should the user be able to choose between
       //  * providing a conflict handler or
       //  * using a "standard" handler based on timestamping?
    let rhtree = RHTree::new(tree).with_conflict_handler(conflict_handler);

    reconcile_service::run(socket, other_addr, rhtree)
        .await
        .unwrap();
}
