use std::hint::black_box;
use std::time::Instant;

use rand::{seq::SliceRandom, Rng, SeedableRng};

use reconciliate::diff::Diffable;
use reconciliate::htree::HTree;
use reconciliate::hvec::HVec;

fn bench_hvec() {
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..100000 {
        let key: u64 = rng.gen();
        let value: u64 = rng.gen();
        key_values.push((key, value));
    }

    let vec1 = HVec::from_iter(key_values.iter().copied());
    key_values.shuffle(&mut rng);
    let mut vec2 = HVec::from_iter(key_values.iter().copied());
    assert_eq!(vec1, vec2);

    let key: u64 = rng.gen();
    let value: u64 = rng.gen();
    vec2.insert(key, value);
    assert_eq!(vec1.diff(&vec2).len(), 1);

    let now = Instant::now();
    const ITERATIONS: u32 = 10000;
    for _ in 0..ITERATIONS {
        black_box(black_box(&vec1).diff(black_box(&vec2)));
    }
    println!("{:?}", (Instant::now() - now) / ITERATIONS);
}

fn bench_htree() {
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..100000 {
        let key: u64 = rng.gen();
        let value: u64 = rng.gen();
        key_values.push((key, value));
    }

    let tree1 = HTree::from_iter(key_values.iter().copied());
    key_values.shuffle(&mut rng);
    let mut tree2 = HTree::from_iter(key_values.iter().copied());
    assert_eq!(tree1, tree2);

    let key: u64 = rng.gen();
    let value: u64 = rng.gen();
    tree2.insert(key, value);
    assert_eq!(tree1.diff(&tree2).len(), 1);

    let now = Instant::now();
    const ITERATIONS: u32 = 100000;
    for _ in 0..ITERATIONS {
        black_box(black_box(&tree1).diff(black_box(&tree2)));
    }
    println!("{:?}", (Instant::now() - now) / ITERATIONS);
}

fn main() {
    bench_hvec();
    bench_htree();
}
