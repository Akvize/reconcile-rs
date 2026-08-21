// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Property 3: what `rsos::encoding` owes the protocol, over a rich generated value type —
//! injectivity, and iteration-order independence for unordered containers.

use proptest::prelude::*;
use rsos::lift;

/// A value shape reaching most arms of the encoding at once: an enum with several variant kinds, a
/// length-prefixed string, an optional, and a nested sequence.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
enum Shape {
    Unit,
    Num(u32),
    Pair(u16, u16),
    Text { s: String, tail: Option<u8> },
    Nested(Vec<Vec<u8>>),
}

fn shape_strategy() -> impl Strategy<Value = Shape> {
    prop_oneof![
        Just(Shape::Unit),
        any::<u32>().prop_map(Shape::Num),
        (any::<u16>(), any::<u16>()).prop_map(|(a, b)| Shape::Pair(a, b)),
        ("[a-c]{0,8}", any::<Option<u8>>()).prop_map(|(s, tail)| Shape::Text { s, tail }),
        prop::collection::vec(prop::collection::vec(any::<u8>(), 0..4), 0..4)
            .prop_map(Shape::Nested),
    ]
}

proptest! {
    /// Injectivity: two elements collide in fingerprint only if they *are* the same element. Not a
    /// claim about BLAKE3 — this checks the encoding does not erase a difference before BLAKE3
    /// ever sees it, which is precisely the framing bug an unprefixed encoding would have.
    #[test]
    fn lift_is_injective_over_rich_values(
        ka in any::<u32>(),
        kb in any::<u32>(),
        va in shape_strategy(),
        vb in shape_strategy(),
    ) {
        let same_element = ka == kb && va == vb;
        prop_assert_eq!(lift(&ka, &va) == lift(&kb, &vb), same_element);
    }

    /// An unordered map summarizes by content, not by iteration order — and agrees with the
    /// ordered map holding the same entries. Neither was expressible under the old `Hash` bound,
    /// which `HashMap` does not satisfy at all.
    #[test]
    fn unordered_maps_lift_independently_of_iteration_order(
        entries in prop::collection::vec((any::<u16>(), shape_strategy()), 0..24),
    ) {
        use std::collections::{BTreeMap, HashMap};

        // Deduplicate first, so the three collections below genuinely hold the same content
        // regardless of the direction they are built in.
        let ordered: BTreeMap<u16, Shape> = entries.into_iter().collect();
        let forward: HashMap<u16, Shape> =
            ordered.iter().map(|(k, v)| (*k, v.clone())).collect();
        let backward: HashMap<u16, Shape> =
            ordered.iter().rev().map(|(k, v)| (*k, v.clone())).collect();

        prop_assert_eq!(lift(&0u8, &forward), lift(&0u8, &backward));
        prop_assert_eq!(lift(&0u8, &forward), lift(&0u8, &ordered));
    }
}
