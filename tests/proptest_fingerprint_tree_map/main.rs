// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Property-based / generative tests, split by concern (AGENTS.md §10/#427). Cargo discovers this
//! directory as the `proptest_fingerprint_tree_map` integration-test binary via `main.rs`, same as
//! a `[[bin]]` target -- every module below runs independently, so (unlike `tests/service/`) there
//! is no shared `support` module.
//!
//! These exercise the two invariants that matter most for a reconciliation
//! library and that fixed-seed example tests cannot cover exhaustively:
//!
//! 1. The hand-rolled B-tree (`FingerprintTreeMap`) behaves like a `BTreeMap` oracle for
//!    every random `insert`/`remove`/`get`/`range` sequence, and its internal
//!    [`FingerprintTreeMap::check_invariants`] holds after *every* mutation (this is where
//!    the `TODO` rebalancing edge cases in `rsos/src/fingerprint_tree_map.rs` would surface) --
//!    [`btreemap_oracle`].
//! 2. Any two stores converge to identical state after running the full diff
//!    loop, the returned diff ranges equal the true symmetric difference of the
//!    key sets, and convergence survives reordered, duplicated and dropped
//!    messages — modelling the lossy UDP transport. Convergence is also checked
//!    under every shipped `RefinementPolicy` and under *mixed* pairs of them,
//!    which is the property that makes the policy swappable without a protocol
//!    break -- [`diff_convergence`].
//!
//! Plus two narrower oracles: encoding injectivity/order-independence
//! ([`encoding_injectivity`]) and driving the diff protocol with an adversarial `RsosView`
//! backend ([`adversarial_rsos`]).

mod adversarial_rsos;
mod btreemap_oracle;
mod diff_convergence;
mod encoding_injectivity;
