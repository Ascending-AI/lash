//! Indexed task-wide counters, with one private bank for all active calls.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct CounterEdit {
    pub(super) index: u32,
    pub(super) advance: u64,
}
#[derive(Clone, Copy)]
pub(super) struct CounterRead {
    /// The counter whose most recently issued occurrence the step reads.
    pub(super) issued: u32,
}
pub(super) struct CounterPlan {
    edits: Box<[CounterEdit]>,
    occurrence: Option<CounterRead>,
}
pub(super) struct SourceCounters {
    values: Box<[(usize, u64)]>,
}
pub(super) struct ReadyCounters<'a> {
    counters: &'a mut SourceCounters,
    plan: &'a CounterPlan,
    occurrence: Option<u64>,
}
impl CounterPlan {
    pub(super) fn indexes(&self) -> impl Iterator<Item = usize> + '_ {
        self.edits
            .iter()
            .map(|edit| edit.index as usize)
            .chain(self.occurrence.map(|read| read.issued as usize))
    }
    pub(super) fn bind(
        edits: Box<[CounterEdit]>,
        occurrence: Option<CounterRead>,
        width: usize,
    ) -> Result<Self, RegionError> {
        let observed = occurrence.map(|read| read.issued);
        if edits.iter().any(|edit| edit.index as usize >= width)
            || edits.windows(2).any(|pair| pair[0].index >= pair[1].index)
            || observed.is_some_and(|index| index as usize >= width)
        {
            return Err(invalid(Refused::Facts));
        }
        Ok(Self { edits, occurrence })
    }
    pub(super) fn empty() -> Self {
        Self {
            edits: Box::new([]),
            occurrence: None,
        }
    }
}
impl SourceCounters {
    pub(super) fn bind(actual: &[(usize, u64)]) -> Result<Self, RegionError> {
        if actual.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(invalid(Refused::Facts));
        }
        let mut values = Vec::new();
        values
            .try_reserve_exact(actual.len())
            .map_err(|_| invalid(Refused::Scratch))?;
        values.extend_from_slice(actual);
        Ok(Self {
            values: values.into_boxed_slice(),
        })
    }
    pub(super) fn current(&self, index: u32) -> Result<u64, RegionError> {
        let position = self.position(index)?;
        Ok(self.values[position].1)
    }
    fn position(&self, index: u32) -> Result<usize, RegionError> {
        self.values
            .binary_search_by_key(&(index as usize), |&(index, _)| index)
            .map_err(|_| invalid(Refused::Facts))
    }
    pub(super) fn prepare<'a>(
        &'a mut self,
        plan: &'a CounterPlan,
    ) -> Result<ReadyCounters<'a>, RegionError> {
        for edit in &plan.edits {
            self.current(edit.index)?
                .checked_add(edit.advance)
                .ok_or_else(|| invalid(Refused::Facts))?;
        }
        let occurrence = plan
            .occurrence
            .map(|read| {
                let index = read.issued;
                let mut count = self.current(index)?;
                if let Some(edit) = plan.edits.iter().find(|edit| edit.index == index) {
                    count += edit.advance;
                }
                count.checked_sub(1).ok_or_else(|| invalid(Refused::Facts))
            })
            .transpose()?;
        Ok(ReadyCounters {
            counters: self,
            plan,
            occurrence,
        })
    }
    pub(super) fn freeze(self) -> Box<[(usize, u64)]> {
        self.values
    }
}
impl ReadyCounters<'_> {
    pub(super) fn accept(self) -> Result<Option<u64>, RegionError> {
        for edit in &self.plan.edits {
            let position = self.counters.position(edit.index)?;
            self.counters.values[position].1 += edit.advance;
        }
        Ok(self.occurrence)
    }
}
