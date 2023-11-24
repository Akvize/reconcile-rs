use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use rand::{Rng, SeedableRng};

use criterion::{
    criterion_group, criterion_main, AxisScale, BenchmarkId, Criterion, PlotConfiguration,
    SamplingMode, Throughput,
};

use reconcile::{DatedMaybeTombstone, HRTree, HashRangeQueryable, Service};

fn hrtree_new(c: &mut Criterion) {
    let mut group = c.benchmark_group("HRTree::new");
    group.bench_function("BTreeMap::new()", |b| {
        b.iter(|| BTreeMap::<u32, u32>::new())
    });
    group.bench_function("HRTree::new()", |b| b.iter(|| HRTree::<u32, u32>::new()));
}

/// Measure the time to insert N elements in the tree
fn hrtree_fill(c: &mut Criterion) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);

    let mut key_values = Vec::new();
    for _ in 0..1_000_000 {
        let key: u32 = rng.gen();
        let value: u32 = rng.gen();
        key_values.push((key, value));
    }
    let key_values = &key_values;

    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("HRTree::fill");
    group.plot_config(plot_config);
    let mut size = 10;
    while size <= key_values.len() {
        group.throughput(Throughput::Elements(size as u64));
        group.sample_size(10.max(1_000_000 / size).min(100));
        group.sampling_mode(SamplingMode::Linear);
        group.bench_with_input(
            BenchmarkId::new("BTreeMap::fill", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let mut tree = BTreeMap::<u32, u32>::new();
                    for (k, v) in key_values[..size].iter().copied() {
                        tree.insert(k, v);
                    }
                })
            },
        );
        group.bench_with_input(BenchmarkId::new("HRTree::fill", size), &size, |b, &size| {
            b.iter(|| {
                let mut tree = HRTree::<u32, u32>::new();
                for (k, v) in key_values[..size].iter().copied() {
                    tree.insert(k, v);
                }
            })
        });
        size *= 10;
    }
}

/// Measure the time to insert (and remove) 1 element in a tree of size N
fn hrtree_insert(c: &mut Criterion) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);

    let mut key_values = Vec::new();
    for _ in 0..1_000_000 {
        let key: u32 = rng.gen();
        let value: u32 = rng.gen();
        key_values.push((key, value));
    }
    let key_values = &key_values;

    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("HRTree::insert");
    group.plot_config(plot_config);
    let mut size = 10;
    while size <= key_values.len() {
        group.throughput(Throughput::Elements(size as u64));
        group.sample_size(10.max(1_000_000 / size).min(100));
        group.sampling_mode(SamplingMode::Linear);
        group.bench_with_input(
            BenchmarkId::new("BTreeMap::insert", size),
            &size,
            |b, &size| {
                let mut tree = BTreeMap::<u32, u32>::new();
                for (k, v) in key_values[..size].iter().copied() {
                    tree.insert(k, v);
                }
                b.iter(|| {
                    // NOTE: do the insertion first because inserting a just-removed element is
                    // likely easier; do not reuse the same key, since it was just removed during
                    // the last iteration
                    let k = rng.gen();
                    let v = rng.gen();
                    tree.insert(k, v);
                    tree.remove(&k);
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new("HRTree::insert", size),
            &size,
            |b, &size| {
                let mut tree = HRTree::<u32, u32>::new();
                for (k, v) in key_values[..size].iter().copied() {
                    tree.insert(k, v);
                }
                b.iter(|| {
                    // NOTE: do the insertion first because inserting a just-removed element is
                    // likely easier; do not reuse the same key, since it was just removed during
                    // the last iteration
                    let k = rng.gen();
                    let v = rng.gen();
                    tree.insert(k, v);
                    tree.remove(&k);
                })
            },
        );
        size *= 10;
    }
}

/// Measure the time to remove (and restore) 1 element in a tree of size N
fn hrtree_remove(c: &mut Criterion) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);

    let mut key_values = Vec::new();
    for _ in 0..1_000_000 {
        let key: u32 = rng.gen();
        let value: u32 = rng.gen();
        key_values.push((key, value));
    }
    let key_values = &key_values;

    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("HRTree::remove");
    group.plot_config(plot_config);
    let mut size = 10;
    while size <= key_values.len() {
        group.throughput(Throughput::Elements(size as u64));
        group.sample_size(10.max(1_000_000 / size).min(100));
        group.sampling_mode(SamplingMode::Linear);
        group.bench_with_input(
            BenchmarkId::new("BTreeMap::remove", size),
            &size,
            |b, &size| {
                let mut tree = BTreeMap::<u32, u32>::new();
                for (k, v) in key_values[..size].iter().copied() {
                    tree.insert(k, v);
                }
                b.iter(|| {
                    // NOTE: do the removal first because removing a just-inserted element is
                    // likely easier; do not reuse the same key, since it was just reinserted
                    // during the last iteration
                    let idx = rng.gen_range(0..size);
                    let (k, v) = &key_values[idx];
                    tree.remove(k);
                    tree.insert(*k, *v);
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new("HRTree::remove", size),
            &size,
            |b, &size| {
                let mut tree = HRTree::<u32, u32>::new();
                for (k, v) in key_values[..size].iter().copied() {
                    tree.insert(k, v);
                }
                b.iter(|| {
                    // NOTE: do the removal first because removing a just-inserted element is
                    // likely easier; do not reuse the same key, since it was just reinserted
                    // during the last iteration
                    let idx = rng.gen_range(0..size);
                    let (k, v) = &key_values[idx];
                    tree.remove(k);
                    tree.insert(*k, *v);
                })
            },
        );
        size *= 10;
    }
}

/// Measure the time to compute the hash over a range in a HRTree of size N
fn hrtree_hash(c: &mut Criterion) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);

    let mut key_values = Vec::new();
    for _ in 0..1_000_000 {
        let key: u32 = rng.gen();
        let value: u32 = rng.gen();
        key_values.push((key, value));
    }
    let key_values = &key_values;

    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("HRTree::hash");
    group.plot_config(plot_config);
    let mut size = 10;
    while size <= key_values.len() {
        group.sample_size(10.max(1_000_000 / size).min(100));
        group.sampling_mode(SamplingMode::Linear);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            let mut tree = HRTree::<u32, u32>::new();
            for (k, v) in key_values[..size].iter().copied() {
                tree.insert(k, v);
            }
            b.iter(|| {
                let k1: u32 = rng.gen();
                let k2: u32 = rng.gen();
                let range = if k1 < k2 { k1..k2 } else { k2..k1 };
                tree.hash(&range);
            })
        });
        size *= 10;
    }
}

/// Measure the time to send 1 insertion, and 1 removal between 2 Service instances containing N items
fn service_send(c: &mut Criterion) {
    let port = 8080;
    let peer_net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.44".parse().unwrap();
    let addr2 = "127.0.0.45".parse().unwrap();

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);

    let mut key_values = Vec::new();
    for _ in 0..1_000_000 {
        let key: u32 = rng.gen();
        let value: DatedMaybeTombstone<u32> = (Utc::now(), rng.gen());
        key_values.push((key, value));
    }
    let key_values = &key_values;

    let rt = tokio::runtime::Runtime::new().unwrap();

    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("Service::send");
    group.plot_config(plot_config);
    let mut size = 10;
    while size <= key_values.len() {
        group.sample_size(10.max(1_000_000 / size).min(100));
        group.sampling_mode(SamplingMode::Linear);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            rt.block_on(async {
                // create trees with many values
                let tree1 = HRTree::from_iter(key_values[..size].iter().copied());
                let tree2 = HRTree::from_iter(key_values[..size].iter().copied());

                // start reconciliation services
                let service1 = Service::new(tree1, port, addr1, peer_net)
                    .await
                    .with_seed(addr2);
                let service2 = Service::new(tree2, port, addr2, peer_net)
                    .await
                    .with_seed(addr1);
                let task1 = tokio::spawn(service1.clone().run());
                let task2 = tokio::spawn(service2.clone().run());

                b.iter(|| {
                    let k: u32 = rng.gen();
                    let v: u32 = rng.gen();
                    service1.insert(k, v, Utc::now());
                    while service2.get(&k).is_none() {
                        std::thread::sleep(Duration::from_micros(1));
                    }
                    service1.remove(&k, Utc::now());
                    while service2.get(&k).is_some() {
                        std::thread::sleep(Duration::from_micros(1));
                    }
                });

                task2.abort();
                task1.abort();
                let _ = tokio::join!(task1, task2);
            })
        });
        size *= 10;
    }
}

/// Measure the time to reconcile 1 insertion/removal between Service instances containing N items
fn service_reconcile(c: &mut Criterion) {
    let port = 8080;
    let peer_net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.44".parse().unwrap();
    let addr2 = "127.0.0.45".parse().unwrap();

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);

    let mut key_values = Vec::new();
    for _ in 0..1_000_000 {
        let key: u32 = rng.gen();
        let value: DatedMaybeTombstone<u32> = (Utc::now(), rng.gen());
        key_values.push((key, value));
    }
    let key_values = &key_values;

    let rt = tokio::runtime::Runtime::new().unwrap();

    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("Service::reconcile");
    group.plot_config(plot_config);
    let mut size = 10;
    while size <= key_values.len() {
        group.sample_size(10.max(1_000_000 / size).min(100));
        group.sampling_mode(SamplingMode::Linear);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            rt.block_on(async {
                // create trees with many values
                let tree1 = HRTree::from_iter(key_values[..size].iter().copied());
                let tree2 = HRTree::from_iter(key_values[..size].iter().copied());

                // start reconciliation services
                let service1 = Service::new(tree1, port, addr1, peer_net)
                    .await
                    .with_seed(addr2);
                let service2 = Service::new(tree2, port, addr2, peer_net)
                    .await
                    .with_seed(addr1);
                let task1 = tokio::spawn(service1.clone().run());
                let task2 = tokio::spawn(service2.clone().run());

                b.iter(|| {
                    let k: u32 = rng.gen();
                    let v: u32 = rng.gen();
                    service1.just_insert(k, v, Utc::now());
                    let clone = service1.clone();
                    let task = tokio::spawn(async move { clone.start_reconciliation().await });
                    while service2.get(&k).is_none() {
                        std::thread::sleep(Duration::from_micros(1));
                    }
                    service1.just_remove(&k, Utc::now());
                    task.abort();
                    let clone = service1.clone();
                    let task = tokio::spawn(async move { clone.start_reconciliation().await });
                    while service2.get(&k).is_some() {
                        std::thread::sleep(Duration::from_micros(1));
                    }
                    task.abort();
                });

                task2.abort();
                task1.abort();
                let _ = tokio::join!(task1, task2);
            })
        });
        size *= 10;
    }
}

criterion_group!(
    benches,
    hrtree_new,
    hrtree_fill,
    hrtree_insert,
    hrtree_remove,
    hrtree_hash,
    service_send,
    service_reconcile,
);
criterion_main!(benches);
