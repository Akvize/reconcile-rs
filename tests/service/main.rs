// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Top-level end-to-end oracle suite: two-or-more-node [`ReplicatedMap`](reconcile::ReplicatedMap)
//! clusters exercised over real UDP sockets, split by concern (AGENTS.md §10/#427). Cargo discovers
//! this directory as the `service` integration-test binary via `main.rs`, same as a `[[bin]]`
//! target -- every module below shares one process and one `support` helper module.

mod basic;
mod security;
mod support;
mod tombstones;
mod topology;
