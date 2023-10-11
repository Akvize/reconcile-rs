use std::hint::black_box;
use std::time::{Duration, Instant};

use rand::{rngs::StdRng, seq::SliceRandom, Rng, SeedableRng};

use reconciliate::diff::Diffable;
use reconciliate::htree::HTree;
use reconciliate::hvec::HVec;

const NUMBER_OF_ELEMENTS: u32 = 1000000;
const ITERATIONS_BETWEEN_TIME_CHECKS: u32 = 100;
const RUNTIME_TARGET: Duration = Duration::from_secs(1);

fn bench<R, F: Fn() -> R>(name: &str, f: F) {
    let now = Instant::now();
    let mut iterations = 0u32;
    loop {
        for _ in 0..ITERATIONS_BETWEEN_TIME_CHECKS {
            black_box(f());
        }
        iterations += ITERATIONS_BETWEEN_TIME_CHECKS;
        let elapsed = Instant::now() - now;
        if elapsed >= RUNTIME_TARGET {
            println!("{name}: {:?}", elapsed / iterations);
            break;
        }
    }
}

fn bench_hvec(rng: &mut StdRng, key_values: &mut Vec<(u64, u64)>, key: u64, value: u64) {
    let now = Instant::now();
    key_values.shuffle(rng);
    let vec1 = HVec::from_iter(key_values.iter().copied());
    key_values.shuffle(rng);
    let mut vec2 = HVec::from_iter(key_values.iter().copied());
    let elapsed = (Instant::now() - now) / 2;
    println!(
        "hvec: {} elements inserted in {elapsed:?}",
        key_values.len()
    );

    assert_eq!(vec1, vec2);
    vec2.insert(key, value);
    assert_eq!(vec1.diff(&vec2).len(), 1);

    bench("hvec", || black_box(&vec1).diff(black_box(&vec2)));
}

fn bench_hvec_fast(rng: &mut StdRng, key_values: &mut Vec<(u64, u64)>, key: u64, value: u64) {
    let now = Instant::now();
    let vec1 = HVec::from_iter(key_values.iter().copied());
    key_values.shuffle(rng);
    let mut vec2 = HVec::from_iter(key_values.iter().copied());
    let elapsed = (Instant::now() - now) / 2;
    println!(
        "hvec_fast: {} elements inserted in {elapsed:?}",
        key_values.len()
    );

    assert_eq!(vec1, vec2);
    vec2.insert(key, value);
    assert_eq!(vec1.fast_diff(&vec2).len(), 1);

    bench("hvec_fast", || black_box(&vec1).fast_diff(black_box(&vec2)));
}

fn bench_htree(rng: &mut StdRng, key_values: &mut Vec<(u64, u64)>, key: u64, value: u64) {
    let now = Instant::now();
    let tree1 = HTree::from_iter(key_values.iter().copied());
    key_values.shuffle(rng);
    let mut tree2 = HTree::from_iter(key_values.iter().copied());
    let elapsed = (Instant::now() - now) / 2;
    println!(
        "htree: {} elements inserted in {elapsed:?}",
        key_values.len()
    );

    assert_eq!(tree1, tree2);
    tree2.insert(key, value);
    assert_eq!(tree1.diff(&tree2).len(), 1);

    bench("htree", || black_box(&tree1).diff(black_box(&tree2)));
}

fn main() {
    let mut rng = StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..NUMBER_OF_ELEMENTS {
        let key: u64 = rng.gen();
        let value: u64 = rng.gen();
        key_values.push((key, value));
    }
    let key: u64 = rng.gen();
    let value: u64 = rng.gen();

    bench_hvec(&mut rng, &mut key_values, key, value);
    bench_hvec_fast(&mut rng, &mut key_values, key, value);
    bench_htree(&mut rng, &mut key_values, key, value);
}
