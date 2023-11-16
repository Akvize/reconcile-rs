use std::collections::BTreeMap;

use rand::{Rng, SeedableRng};

use criterion::{
    criterion_group, criterion_main, AxisScale, BenchmarkId, Criterion, PlotConfiguration,
    SamplingMode, Throughput,
};

use reconcile::HRTree;

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

criterion_group!(benches, hrtree_new, hrtree_insert);
criterion_main!(benches);
