//! One source resource owner stages an admitted loop prefix. No iteration
//! changes the shared meter or counters; its final boundary settles them once.
use super::{Accepted, OccurrenceDebt, SourceAccepted, SourceResources, Window};
use crate::compile::room::charges::ChargeDebt;
use lash_kernel_doc::heap::HeapFault;
use std::collections::BTreeMap;

pub(crate) struct RepeatResources<'a> {
    resources: SourceResources<'a>,
    window: Window,
    units: u64,
    counts: BTreeMap<usize, u64>,
}
impl<'a> SourceResources<'a> {
    pub(crate) fn repeat(self) -> Option<RepeatResources<'a>> {
        if self.safe_point != *self.charged {
            return None;
        }
        let window = Window::new(*self.charged, self.bound, self.slice);
        Some(RepeatResources {
            resources: self,
            window,
            units: 0,
            counts: BTreeMap::new(),
        })
    }
}
impl RepeatResources<'_> {
    pub(crate) fn admit(
        &mut self,
        debts: &[ChargeDebt<'_>],
        depth: u64,
        occurrences: &[usize],
    ) -> bool {
        let Some(units) = debts
            .iter()
            .try_fold(0u64, |sum, debt| sum.checked_add(debt.units()))
        else {
            return false;
        };
        if self
            .resources
            .depth
            .checked_add(depth)
            .is_none_or(|depth| depth > self.resources.limit)
            || self
                .window
                .admit((), self.units, units, None, true)
                .is_none()
        {
            return false;
        }
        let mut counts = self.counts.clone();
        for index in occurrences {
            let count = counts.entry(*index).or_default();
            let Some(next) = count.checked_add(1) else {
                return false;
            };
            if self
                .resources
                .counters
                .current(*index)
                .and_then(|current| current.checked_add(next))
                .is_none()
            {
                return false;
            }
            *count = next;
        }
        self.counts = counts;
        // Window::admit proved this addition while preserving strict slice room.
        self.units += units;
        true
    }
}
impl<'a> RepeatResources<'a> {
    pub(crate) fn accept<C>(self, request: C) -> Result<SourceAccepted<'a, C>, HeapFault> {
        let sites: Vec<_> = self.counts.keys().copied().collect();
        if !self.resources.counters.prepare_publication(&sites)? {
            return Err(HeapFault::Malformed(
                "admitted loop counter staging changed".into(),
            ));
        }
        let accepted = self
            .window
            .admit(request, 0, self.units, None, true)
            .ok_or_else(|| HeapFault::Malformed("admitted loop charge window changed".into()))?;
        Ok(SourceAccepted {
            resources: self.resources,
            accepted: Accepted {
                request: accepted.request,
                end: accepted.end,
            },
            occurrences: OccurrenceDebt::Repeated(self.counts.into_iter().collect()),
        })
    }
}
