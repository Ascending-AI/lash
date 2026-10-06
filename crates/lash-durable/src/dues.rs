//! Due sources: the durable deadlines an owner waits on, and the earliest
//! of them, which a release as `waiting` records as the actor's due time.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use crate::ids::DurableInstant;

/// What a due time is for. Each lane registers its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DueSource {
    /// L3: a model call's `model_total` deadline.
    ModelDeadline,
    /// L4: an admitted execution's `ExecutionLimit`.
    ToolLimit,
    /// L4: a `Repeatable` retry's due time.
    RetryDue,
    /// L5: a wait's deadline.
    WaitDeadline,
    /// L5: a durable timer.
    Timer,
    /// L6: a process's `Sleep`.
    ProcessSleep,
    /// L6: a cascade cursor's next batch.
    CascadeCursor,
}

/// The due times an owner noted, the earliest per source.
#[derive(Debug, Default)]
pub struct Dues {
    noted: Mutex<BTreeMap<DueSource, DurableInstant>>,
}

impl Dues {
    /// No due times.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that `source` is due at `at`; a source keeps its earliest.
    pub fn note(&self, source: DueSource, at: DurableInstant) {
        let mut noted = self.noted.lock().unwrap_or_else(PoisonError::into_inner);
        let due = noted.entry(source).or_insert(at);
        *due = (*due).min(at);
    }

    /// Forget `source`'s due time: what it waited for happened.
    pub fn clear(&self, source: DueSource) {
        self.noted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&source);
    }

    /// The earliest due time of any source: what a release as `waiting`
    /// records.
    #[must_use]
    pub fn next(&self) -> Option<DurableInstant> {
        self.noted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .min()
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_waits_for_the_earliest_due_of_any_source() {
        let dues = Dues::new();
        assert_eq!(dues.next(), None);
        dues.note(DueSource::WaitDeadline, DurableInstant(50));
        dues.note(DueSource::RetryDue, DurableInstant(30));
        dues.note(DueSource::WaitDeadline, DurableInstant(70));
        assert_eq!(dues.next(), Some(DurableInstant(30)));
        dues.clear(DueSource::RetryDue);
        assert_eq!(dues.next(), Some(DurableInstant(50)));
    }
}
