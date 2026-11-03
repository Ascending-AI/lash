//! An ordinary generic entry consumes the shared source publication plan.
use super::*;
use crate::machine::islands::compiled::CollectionStep;

/// These indexes name immutable source descriptors. This entry never dispatches
/// a grouped collection opcode or enters its family-specific runtime adapter.
pub(in crate::machine::islands) enum SourcePublication {
    Append(u32),
    Reduce(u32),
    Place(u32),
    Visit(u32),
    ReadSlot(u32),
    Merge(u32),
    Tail(u32),
}
impl SourcePublication {
    pub(in crate::machine::islands) fn new(
        step: &CollectionStep,
        tail: bool,
        at: u32,
    ) -> Option<Self> {
        Some(match (step, tail) {
            (CollectionStep::Append(_), false) => Self::Append(at),
            (CollectionStep::Reduce(_), false) => Self::Reduce(at),
            (CollectionStep::Place(_), false) => Self::Place(at),
            (CollectionStep::Visit(_), false) => Self::Visit(at),
            (CollectionStep::ReadSlot(_), false) => Self::ReadSlot(at),
            (CollectionStep::Merge(_), false) => Self::Merge(at),
            (CollectionStep::Merge(_), true) => Self::Tail(at),
            _ => return None,
        })
    }
}
impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    pub(in crate::machine::islands::execute) fn generic_source_publication(
        &mut self,
        frame: &mut Values<'_, 'e>,
        entry: &SourcePublication,
    ) -> Option<Result<Next, Stop>> {
        let sites = &frame.state.callee.compiled.code.sites;
        let (_index, accepted) = match *entry {
            SourcePublication::Append(at) => (
                0,
                self.try_source_publication(frame, &sites.collection_append[at as usize]),
            ),
            SourcePublication::Reduce(at) => (
                1,
                self.try_source_publication(frame, &sites.collection_reduce[at as usize]),
            ),
            SourcePublication::Place(at) => (
                2,
                self.try_source_publication(frame, &sites.collection_placement[at as usize]),
            ),
            SourcePublication::Visit(at) => (
                3,
                self.try_source_publication(frame, &sites.collection_visit[at as usize]),
            ),
            SourcePublication::ReadSlot(at) => (
                4,
                self.try_source_publication(frame, &sites.collection_read[at as usize]),
            ),
            SourcePublication::Merge(at) => (
                5,
                self.try_source_publication(frame, &sites.collection_merge[at as usize]),
            ),
            SourcePublication::Tail(at) => (
                6,
                self.try_source_publication(frame, &sites.collection_tail[at as usize]),
            ),
        };
        match accepted {
            Ok(None) => None,
            Ok(Some(_)) => {
                #[cfg(test)]
                super::super::merge::source_publication_accepted(_index);
                Some(Ok(Next::Continue))
            }
            Err(error) => Some(Err(self.halt(error.into()))),
        }
    }
}
