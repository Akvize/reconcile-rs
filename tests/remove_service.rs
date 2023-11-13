use std::net::SocketAddr;
use std::time::Duration;

use chrono::Utc;
use rand::{
    distributions::{Alphanumeric, DistString},
    Rng, SeedableRng,
};
use tokio::net::UdpSocket;

use reconcile::{DatedMaybeTombstone, HRTree, HashRangeQueryable, RemoveService};

#[tokio::test(flavor = "multi_thread")]
async fn test() {
    let addr1: SocketAddr = "127.0.0.44:8080".parse().unwrap();
    let addr2: SocketAddr = "127.0.0.45:8080".parse().unwrap();
    let socket1 = UdpSocket::bind(addr1).await.unwrap();
    let socket2 = UdpSocket::bind(addr2).await.unwrap();

    // create tree1 with many values
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..1000 {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: DatedMaybeTombstone<String> =
            (Utc::now(), Some(Alphanumeric.sample_string(&mut rng, 100)));
        key_values.push((key, value));
    }
    let tree1 = HRTree::from_iter(key_values.into_iter());
    let start_hash = tree1.hash(&..);

    // empty tree2
    let tree2: HRTree<String, DatedMaybeTombstone<String>> = HRTree::new();

    // start reconciliation services for tree1 and tree2
    let service1 = RemoveService::new(tree1, socket1);
    let service2 = RemoveService::new(tree2, socket2);
    let task2 = tokio::spawn(service2.clone().run(addr1, |_, _, _| {}));
    assert_eq!(service2.read().hash(&..), 0);
    let task1 = tokio::spawn(service1.clone().run(addr2, |_, _, _| {}));
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
    let value = "Hello, World!".to_string();
    service2.insert(key.clone(), value.clone(), Utc::now());
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
    assert_eq!(service2.read().get(&key).unwrap().1, Some(value));

    // remove value from tree1, and check that the tombstone is transferred to tree2
    let key = "42".to_string();
    service1.remove(&key, Utc::now());
    let new_hash = service1.read().hash(&..);
    assert_ne!(new_hash, start_hash);
    for _ in 0..1000 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if service2.read().hash(&..) == new_hash {
            break;
        }
    }
    assert_eq!(service2.read().hash(&..), new_hash);
    assert_eq!(service1.read().hash(&..), new_hash);
    assert_eq!(service1.read().get(&key).unwrap().1, None);

    // check that the more recent value always wins
    for _ in 0..10 {
        // add value to tree2, and check that it is transferred to tree1
        let key = "42".to_string();
        let t1 = Utc::now();
        let value1 = "Hello, World!".to_string();
        let t2 = Utc::now();
        if rng.gen() {
            service1.insert(key.clone(), value1.clone(), t1);
            service2.remove(&key, t2);
            for _ in 0..1000 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if service2.read().get(&key).unwrap().1 == None {
                    break;
                }
            }
            assert_eq!(service2.read().get(&key).unwrap().1, None);
        } else {
            service1.remove(&key, t1);
            service2.insert(key.clone(), value1.clone(), t2);
            for _ in 0..1000 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if service2.read().get(&key).unwrap().1 == Some(value1.clone()) {
                    break;
                }
            }
            assert_eq!(service2.read().get(&key).unwrap().1, Some(value1));
        }
    }

    task2.abort();
    task1.abort();
}
