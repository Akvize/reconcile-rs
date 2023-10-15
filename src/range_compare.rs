use std::ops::{Bound, RangeBounds};

#[derive(Debug, Eq, PartialEq)]
pub enum RangeOrdering {
    Less,
    Inside,
    Greater,
}

pub fn range_compare<T: Ord, R: RangeBounds<T>>(item: &T, range: &R) -> RangeOrdering {
    if match range.start_bound() {
        Bound::Included(key) => item < key,
        Bound::Excluded(key) => item <= key,
        _ => false,
    } {
        return RangeOrdering::Less;
    }
    if match range.end_bound() {
        Bound::Included(key) => item > key,
        Bound::Excluded(key) => item >= key,
        _ => false,
    } {
        return RangeOrdering::Greater;
    }
    RangeOrdering::Inside
}
