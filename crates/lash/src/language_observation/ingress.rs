//! Count and byte admission, including the single publication in flight.
//! A queue that cannot take a draft loses every draft it holds, and retires
//! the whole class's provisional continuity: it can no longer name what it
//! lost.

use std::collections::VecDeque;

pub(super) const MAX_EVENTS: usize = 256;
pub(super) const MAX_BYTES: usize = 4 * 1024 * 1024;

pub(super) enum Work<T> {
    Invalidate,
    Publish { draft: T, charge: usize },
}

pub(super) struct Ingress<T> {
    pending: VecDeque<(T, usize)>,
    admitted: usize,
    bytes: usize,
    dirty: bool,
    max_events: usize,
    max_bytes: usize,
}

impl<T> Ingress<T> {
    pub(super) fn new(max_events: usize, max_bytes: usize) -> Self {
        Self {
            pending: VecDeque::new(),
            admitted: 0,
            bytes: 0,
            dirty: false,
            max_events,
            max_bytes,
        }
    }

    /// The closure clones a draft only after count and byte admission. A
    /// refusal answers the pending drafts it lost with the refused one.
    pub(super) fn enqueue(
        &mut self,
        charge: Option<usize>,
        draft: impl FnOnce() -> T,
    ) -> Result<(), Vec<T>> {
        let accepted = charge.filter(|charge| {
            self.admitted < self.max_events && *charge <= self.max_bytes.saturating_sub(self.bytes)
        });
        match accepted {
            Some(charge) => {
                self.pending.push_back((draft(), charge));
                self.admitted += 1;
                self.bytes += charge;
                Ok(())
            }
            None => Err(self.invalidate()),
        }
    }

    /// Lose every pending draft and owe the class's invalidation.
    pub(super) fn invalidate(&mut self) -> Vec<T> {
        self.dirty = true;
        self.pending
            .drain(..)
            .map(|(draft, charge)| {
                self.admitted -= 1;
                self.bytes -= charge;
                draft
            })
            .collect()
    }

    pub(super) fn next(&mut self) -> Option<Work<T>> {
        if std::mem::take(&mut self.dirty) {
            return Some(Work::Invalidate);
        }
        self.pending
            .pop_front()
            .map(|(draft, charge)| Work::Publish { draft, charge })
    }

    pub(super) fn has_work(&self) -> bool {
        self.dirty || self.admitted != 0
    }

    /// Drafts admitted and not yet finished: pending or in flight.
    #[cfg(test)]
    pub(super) fn admitted(&self) -> usize {
        self.admitted
    }

    pub(super) fn finish(&mut self, charge: usize) {
        self.admitted -= 1;
        self.bytes -= charge;
    }
}
