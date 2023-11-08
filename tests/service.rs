use std::net::SocketAddr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rand::{
    distributions::{Alphanumeric, DistString},
    Rng, SeedableRng,
};
use tokio::net::UdpSocket;

use reconcile::diff::HashRangeQueryable;
use reconcile::hrtree::HRTree;
use reconcile::Service;

#[tokio::test(flavor = "multi_thread")]
async fn test() {
    let addr1: SocketAddr = "127.0.0.42:8080".parse().unwrap();
    let addr2: SocketAddr = "127.0.0.43:8080".parse().unwrap();
    let socket1 = UdpSocket::bind(addr1).await.unwrap();
    let socket2 = UdpSocket::bind(addr2).await.unwrap();

    // create tree1 with many values
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..1000 {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let time = Utc::now();
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        key_values.push((key, (time, value)));
    }
    let tree1 = HRTree::from_iter(key_values.into_iter());
    let start_hash = tree1.hash(&..);

    // empty tree2
    let tree2: HRTree<String, (DateTime<Utc>, String)> = HRTree::new();

    // start reconciliation services for tree1 and tree2
    let service1 = Service::new(tree1);
    let service2 = Service::new(tree2);
    let task2 = tokio::spawn(service2.clone().run(socket2, addr1, |_, _, _| {}, |_| {}));
    assert_eq!(service2.read().hash(&..), 0);
    let task1 = tokio::spawn(service1.clone().run(socket1, addr2, |_, _, _| {}, |_| {}));
    assert_eq!(service1.read().hash(&..), start_hash);

    // check that tree2 is filled with the values from tree1
    for _ in 0..1000 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if service2.read().hash(&..) == start_hash {
            break;
        }
    }
    assert_eq!(service1.read().hash(&..), start_hash);
    assert_eq!(service2.read().hash(&..), start_hash);

    // add value to tree2, and check that it is transferred to tree1
    let key = "42".to_string();
    let value = (Utc::now(), "Hello, World!".to_string());
    service2.insert(key.clone(), value.clone());
    let new_hash = service2.read().hash(&..);
    assert_ne!(new_hash, start_hash);
    assert_eq!(service1.read().hash(&..), start_hash);
    for _ in 0..1000 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if service1.read().hash(&..) == new_hash {
            break;
        }
    }
    assert_eq!(service1.read().hash(&..), new_hash);
    assert_eq!(service2.read().hash(&..), new_hash);
    assert_eq!(service2.read().get(&key), Some(&value));

    // check that the more recent value always win
    for _ in 0..10 {
        // add value to tree2, and check that it is transferred to tree1
        let key = "42".to_string();
        let value1 = (Utc::now(), "Hello, World!".to_string());
        let value2 = (Utc::now(), "Hello, World!".to_string());
        if rng.gen() {
            service1.insert(key.clone(), value1.clone());
            service2.insert(key.clone(), value2.clone());
        } else {
            service1.insert(key.clone(), value2.clone());
            service2.insert(key.clone(), value1.clone());
        }
        for _ in 0..1000 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if service2.read().get(&key) == Some(&value2) {
                break;
            }
        }
        assert_eq!(service2.read().get(&key), Some(&value2));
    }

    task2.abort();
    task1.abort();
}
