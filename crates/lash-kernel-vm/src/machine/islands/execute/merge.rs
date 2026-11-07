//! Typed collection packets share one affine source publication.
use super::super::compiled::{
    CollectionAppend, CollectionPacket, Merge, Placement, ReadSlot, Reduction, Visit,
};
use super::*;
#[path = "merge/region.rs"]
pub(super) mod region;

#[cfg(test)]
thread_local! { static ACCEPTED: std::cell::Cell<[u64; 7]> = const { std::cell::Cell::new([0; 7]) }; }
#[cfg(test)]
pub(super) fn source_publication_accepted(index: usize) {
    ACCEPTED.with(|counts| {
        let mut n = counts.get();
        n[index] += 1;
        counts.set(n);
    });
}
#[cfg(test)]
impl crate::machine::KernelMachine {
    pub(crate) fn take_collection_acceptances(&self) -> [u64; 7] {
        ACCEPTED.with(|counts| counts.replace([0; 7]))
    }
}
#[cfg(test)]
macro_rules! collection_count {
    (Visit) => {
        #[cfg(test)]
        super::super::COLLECTION_VISITS.with(|n| n.set(n.get() + 1));
    };
    ($other:ident) => {};
}
macro_rules! collection_entry {
    ($method:ident, $shape:ty, $family:ident, $index:tt) => {
        impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
            pub(super) fn $method(
                &mut self,
                frame: &mut Values<'_, 'e>,
                packet: &CollectionPacket<$shape>,
            ) -> Option<Result<Next, Stop>> {
                let answer = self.publish_collection(frame, packet);
                #[cfg(test)]
                if matches!(&answer, Some(Ok(_))) {
                    source_publication_accepted($index);
                    collection_count!($family);
                }
                #[cfg(any(test, feature = "testing"))]
                {
                    use super::super::witness::{self, Exit, Family};
                    witness::record(
                        Family::$family,
                        match &answer {
                            None => Exit::Guard,
                            Some(Err(_)) => Exit::Boundary,
                            Some(Ok((_, region::CollectionContinuation::Callback))) => {
                                Exit::Callback
                            }
                            Some(Ok((_, region::CollectionContinuation::Commit))) => Exit::Commit,
                        },
                    );
                }
                answer.map(|answer| answer.map(|(next, _)| next))
            }
        }
    };
}
collection_entry!(append_collection, CollectionAppend, CollectionAppend, 0);
collection_entry!(reduce_collection, Reduction, Reduction, 1);
collection_entry!(place_collection, Placement, Placement, 2);
collection_entry!(visit_collection, Visit, Visit, 3);
collection_entry!(read_collection, ReadSlot, ReadSlot, 4);
collection_entry!(merge_collection, Merge, Merge, 5);
collection_entry!(merge_collection_tail, MergeTail, Merge, 6);
