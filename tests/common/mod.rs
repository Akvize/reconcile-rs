// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Shared test helpers for the integration test suite.

use std::sync::OnceLock;
use std::time::Duration;

/// Scaling factor for `wait_until` poll budgets.
///
/// Set `RECONCILE_TEST_TIME_MULTIPLIER` to an integer or float to stretch the
/// budget; the default is 1 (unscaled, same as the hard-coded original budget).
/// The coverage job sets it to 3 because `cargo llvm-cov` instrumentation
/// slows the integration suite by 2-5×, and a 1-second budget produces
/// spurious "convergence timed out" failures on shared CI runners.
fn time_multiplier() -> u32 {
    static MULTIPLIER: OnceLock<u32> = OnceLock::new();
    *MULTIPLIER.get_or_init(|| {
        std::env::var("RECONCILE_TEST_TIME_MULTIPLIER")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|f| f.max(1.0).round() as u32)
            .unwrap_or(1)
    })
}

/// Wait for a while until the provided predicate becomes true.
///
/// Returns `true` if the predicate became true within the budget, or `false`
/// if it timed out.  The budget is `base_iters` × 10 ms by default, scaled by
/// `RECONCILE_TEST_TIME_MULTIPLIER`.  Most callers pass 100; test files that
/// already used a larger base (e.g. 200 for discovery tests) pass their
/// original value so relative timing is preserved.
pub async fn wait_until_with_budget<F: FnMut() -> bool>(mut f: F, base_iters: u32) -> bool {
    let iters = base_iters.saturating_mul(time_multiplier());
    for _ in 0..iters {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if f() {
            return true;
        }
    }
    false
}

/// Wait for a while until the provided predicate becomes true.
///
/// Returns `true` if the predicate became true within the budget, or `false`
/// if it timed out.  The budget is 100 × 10 ms by default, scaled by
/// `RECONCILE_TEST_TIME_MULTIPLIER`.
#[allow(dead_code)] // used by service.rs; not every test file that includes this module uses it
pub async fn wait_until<F: FnMut() -> bool>(f: F) -> bool {
    wait_until_with_budget(f, 100).await
}
