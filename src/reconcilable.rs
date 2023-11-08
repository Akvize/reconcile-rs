use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ReconciliationResult {
    KeepSelf,
    KeepOther,
}

pub trait Reconcilable {
    fn reconcile(&self, other: &Self) -> ReconciliationResult;
}

impl<V> Reconcilable for (DateTime<Utc>, V) {
    fn reconcile(&self, other: &Self) -> ReconciliationResult {
        if other.0 > self.0 {
            ReconciliationResult::KeepOther
        } else {
            ReconciliationResult::KeepSelf
        }
    }
}
