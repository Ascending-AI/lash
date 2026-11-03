//! Admission precedes publication. Storage owners retain their own discard,
//! snapshot and moved-storage rollback implementations.

use super::*;

use crate::admission::{Accepted, Window};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    #[inline(always)]
    pub(super) fn admission_window(&self) -> Window {
        Window::new(self.charged, self.bound, self.slice_end)
    }

    #[inline(always)]
    pub(super) fn source_admission(
        &self,
        occurrence: Option<usize>,
        before: u64,
        units: u64,
        close: Option<Close>,
        strict: bool,
    ) -> Option<Accepted> {
        let occurrences = occurrence.map(|at| [at]);
        self.admission_window().admit_counted(
            &*self.counters,
            (),
            &crate::admission::Requirements {
                occurrences: occurrences.as_ref().map_or(&[], |one| one.as_slice()),
                before,
                units,
                close,
                strict,
            },
        )
    }
}
