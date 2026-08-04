// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! System-level, end-to-end benchmarks driving the **public** `ReconcileStore` API (point-read
//! latency vs `HashMap`/`BTreeMap`, per-entry memory footprint, bulk-load throughput, cold anti-
//! entropy convergence between two in-process nodes, and durable-snapshot reload). Unlike the
//! `bench` target, these reach no crate internals, so they need no feature gate.
//!
//! Reproduction and interpretation are documented in `benches/README.md`. Not run in CI (only
//! compile-checked); run locally with `cargo bench --bench system`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hint::black_box;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use criterion::{
    criterion_group, criterion_main, AxisScale, BenchmarkId, Criterion, PlotConfiguration,
    Throughput,
};
use tokio::runtime::Runtime;

use reconcile::{
    reconcile_store::Config, FileSnapshot, PersistedState, Persistence, ReconcileStore, Timestamp,
    ValueOnly,
};

/// Dataset sizes swept by the size-parameterised benchmarks (log scale).
const SIZES: &[usize] = &[10, 100, 1_000, 10_000, 100_000];

/// Deterministic `(key, value)` corpus: sequential keys keep every backend's layout comparable.
fn corpus(n: usize) -> Vec<(u32, u32)> {
    (0..n as u32)
        .map(|k| (k, k.wrapping_mul(2_654_435_761)))
        .collect()
}

fn log_group<'a>(
    c: &'a mut Criterion,
    name: &str,
) -> criterion::BenchmarkGroup<'a, criterion::measurement::WallTime> {
    let mut group = c.benchmark_group(name);
    group.plot_config(PlotConfiguration::default().summary_scale(AxisScale::Logarithmic));
    group
}

/// An in-process, peerless store loaded with `kvs` (ephemeral port, so many can coexist).
fn loaded_store(rt: &Runtime, kvs: &[(u32, u32)]) -> ReconcileStore<u32, u32> {
    rt.block_on(async {
        let store = ReconcileStore::<u32, u32>::new(
            Config::default()
                .with_port(0)
                .with_listen_addr("127.0.0.1".parse().unwrap())
                .with_net("127.0.0.1/8".parse().unwrap()),
        )
        .await
        .expect("bind failed");
        store.insert_bulk(kvs);
        store
    })
}

/// Point-read latency: `ReconcileStore::get` against std collections at the same sizes.
fn point_read(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = log_group(c, "point_read");
    for &size in SIZES {
        let kvs = corpus(size);
        let probe = kvs[size / 2].0;
        let store = loaded_store(&rt, &kvs);
        let hashmap: HashMap<u32, u32> = kvs.iter().copied().collect();
        let btreemap: BTreeMap<u32, u32> = kvs.iter().copied().collect();

        group.bench_with_input(BenchmarkId::new("ReconcileStore", size), &size, |b, _| {
            b.iter(|| black_box(store.get(black_box(&probe)).map(|g| *g)));
        });
        group.bench_with_input(BenchmarkId::new("HashMap", size), &size, |b, _| {
            b.iter(|| black_box(hashmap.get(black_box(&probe)).copied()));
        });
        group.bench_with_input(BenchmarkId::new("BTreeMap", size), &size, |b, _| {
            b.iter(|| black_box(btreemap.get(black_box(&probe)).copied()));
        });
    }
    group.finish();
}

/// Bulk-load throughput: `insert_bulk` of `N` fresh entries into an empty store.
fn bulk_load(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = log_group(c, "bulk_load");
    for &size in SIZES {
        let kvs = corpus(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            // `insert_bulk` spawns a (peerless, no-op) broadcast, so the measured op needs a runtime
            // context; `block_on` here runs sequentially after the setup's, never nested inside it.
            b.iter_batched(
                || loaded_store(&rt, &[]),
                |store| rt.block_on(async { store.insert_bulk(black_box(&kvs)) }),
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

/// One-shot report of the fixed per-entry footprint of the dated cell vs the value-only mirror
/// projection, across value payload sizes. Printed rather than timed (it is a compile-time fact).
fn memory_footprint(c: &mut Criterion) {
    fn report<const N: usize>() {
        let dated = std::mem::size_of::<(Timestamp, Option<[u8; N]>)>();
        let light = std::mem::size_of::<ValueOnly<[u8; N]>>();
        println!(
            "[memory] value=[u8; {N}]: dated (Timestamp, Option) = {dated} B/entry, \
             value-only = {light} B/entry, saved = {} B/entry",
            dated.saturating_sub(light)
        );
    }
    report::<8>();
    report::<64>();
    report::<256>();

    // A trivial timed anchor so the report participates in a normal `cargo bench` run.
    c.bench_function("memory_footprint::size_of", |b| {
        b.iter(|| black_box(std::mem::size_of::<(Timestamp, Option<[u8; 64]>)>()));
    });
}

/// A fresh, unique loopback address pair for one cold-sync iteration (avoids rebind collisions
/// across Criterion's repeated samples). Both peers share the port; only the address differs.
fn fresh_pair() -> (IpAddr, IpAddr) {
    static N: AtomicU32 = AtomicU32::new(0);
    let i = N.fetch_add(1, Ordering::Relaxed);
    let hi = ((i >> 7) & 0xff) as u8;
    let lo = ((i & 0x7f) as u8) * 2 + 1; // odd octet for peer A, +1 (even) for peer B
    (
        format!("127.1.{hi}.{lo}").parse().unwrap(),
        format!("127.1.{hi}.{}", lo + 1).parse().unwrap(),
    )
}

/// Cold-sync: how long an empty node takes to converge with a full one purely via anti-entropy.
///
/// Peer A is pre-loaded **before it has any peer**, so nothing is broadcast eagerly; peer B (empty)
/// seeds A and pulls the whole dataset through the range-diff protocol. We time from spawning the
/// run loops until B's fingerprint matches A's.
fn cold_sync(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let port = 9_500;
    let mut group = log_group(c, "cold_sync");
    // Convergence dominates wall time, so keep the dataset and sample count modest.
    for &size in &[1_000usize, 10_000, 100_000] {
        let kvs = corpus(size);
        group.throughput(Throughput::Elements(size as u64));
        group.sample_size(10);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let (addr_a, addr_b) = fresh_pair();
                        let cfg = |addr: IpAddr| {
                            Config::default()
                                .with_port(port)
                                .with_listen_addr(addr)
                                .with_net("127.0.0.1/8".parse().unwrap())
                        };
                        // A is loaded with no peer declared, so `insert_bulk` broadcasts to nobody.
                        let a = ReconcileStore::<u32, u32>::new(cfg(addr_a))
                            .await
                            .expect("bind A");
                        a.insert_bulk(&kvs);
                        let target = a.fingerprint(..);
                        let b_store = ReconcileStore::<u32, u32>::new(cfg(addr_b))
                            .await
                            .expect("bind B")
                            .with_seed(addr_a);

                        let start = Instant::now();
                        let ta = tokio::spawn(a.clone().run());
                        let tb = tokio::spawn(b_store.clone().run());
                        while b_store.fingerprint(..) != target {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        total += start.elapsed();
                        ta.abort();
                        tb.abort();
                    }
                    total
                })
            });
        });
    }
    group.finish();
}

/// Durable rejoin: time to reload a persisted snapshot of `N` entries from disk (the cost a
/// restarting node pays before rejoining the cluster). The snapshot is written once in setup.
fn durable_rejoin(c: &mut Criterion) {
    let mut group = log_group(c, "durable_rejoin");
    for &size in SIZES {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = FileSnapshot::new(dir.path().join("reconcile.snapshot"));
        let state: PersistedState<u32, u32> = PersistedState {
            entries: (0..size as u32)
                .map(|k| (k, (Timestamp::new(k as u64, 0, 0), Some(k))))
                .collect(),
            members: HashSet::new(),
            tombstone_acks: HashMap::new(),
        };
        Persistence::<u32, u32>::save(&snapshot, &state).expect("save");

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| black_box(Persistence::<u32, u32>::load(&snapshot).expect("load")));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    point_read,
    bulk_load,
    memory_footprint,
    cold_sync,
    durable_rejoin
);
criterion_main!(benches);
