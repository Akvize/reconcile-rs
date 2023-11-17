use std::collections::BTreeMap;

use rand::{Rng, SeedableRng};

use criterion::{
    criterion_group, criterion_main, AxisScale, BenchmarkId, Criterion, PlotConfiguration,
    SamplingMode, Throughput,
};

use reconcile::{HRTree, HashRangeQueryable};

fn hrtree_new(c: &mut Criterion) {
    let mut group = c.benchmark_group("HRTree::new");
    group.bench_function("BTreeMap::new()", |b| {
        b.iter(|| BTreeMap::<u32, u32>::new())
    });
    group.bench_function("HRTree::new()", |b| b.iter(|| HRTree::<u32, u32>::new()));
}

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
                b.iter(|| {
                    let mut tree = BTreeMap::<u32, u32>::new();
                    for (k, v) in key_values[..size].iter().copied() {
                        tree.insert(k, v);
                    }
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new("HRTree::insert", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let mut tree = HRTree::<u32, u32>::new();
                    for (k, v) in key_values[..size].iter().copied() {
                        tree.insert(k, v);
                    }
                })
            },
        );
        size *= 10;
    }
}

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
                let k = rng.gen();
                let v = rng.gen();
                b.iter(|| {
                    tree.insert(k, v);
                    tree.remove(&k);
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
                let k = rng.gen();
                let v = rng.gen();
                b.iter(|| {
                    tree.insert(k, v);
                    tree.remove(&k);
                })
            },
        );
        size *= 10;
    }
}

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

criterion_group!(
    benches,
    hrtree_new,
    hrtree_insert,
    hrtree_remove,
    hrtree_hash
);
criterion_main!(benches);
