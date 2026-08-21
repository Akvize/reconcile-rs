// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Blanket forwarding, so a policy behind a smart pointer is itself a policy.

use super::{Comparison, Decision, RefinementPolicy};

impl<P: RefinementPolicy + ?Sized> RefinementPolicy for &P {
    fn decide(&self, comparison: Comparison) -> Decision {
        (**self).decide(comparison)
    }
}
