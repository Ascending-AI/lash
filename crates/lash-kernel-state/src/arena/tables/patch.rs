//! Incremental index capacity and one-member edits for accepted publications.
use super::*;
use lash_kernel_doc::heap::{HeapFault, HostResource};

pub(crate) struct PreparedIndexGrowth {
    replacement: Option<Index>,
    shared_append: bool,
    sequences: Option<Vec<Vec<u64>>>,
    join: bool,
}
fn exhausted() -> HeapFault {
    HeapFault::Exhausted {
        resource: HostResource::StagingBytes,
    }
}
impl Index {
    pub(crate) fn set_region_next(&mut self, next: u64) {
        match &mut self.0 {
            Repr::Own(own) => own.next = next,
            Repr::Shared(layout) => assert_eq!(u64::from(layout.len()), next),
        }
    }

    /// Reserve only incremental growth. A shared layout is converted at most
    /// once, with its existing fixed sixteen-key bound, never per insertion.
    pub(crate) fn prepare_growth(
        &mut self,
        inserts: usize,
        removes: bool,
        single_key: Option<&Key>,
    ) -> Result<PreparedIndexGrowth, HeapFault> {
        if inserts == 0 && !removes {
            return Ok(PreparedIndexGrowth {
                replacement: None,
                shared_append: false,
                sequences: None,
                join: false,
            });
        }
        self.next()
            .checked_add(inserts as u64)
            .ok_or_else(|| HeapFault::Malformed("table insertion sequence overflow".into()))?;
        if inserts == 1
            && !removes
            && let (Repr::Shared(layout), Some(key)) = (&self.0, single_key)
            && let Some(next) = layout.next(key)
        {
            return Ok(PreparedIndexGrowth {
                replacement: Some(Index(Repr::Shared(next))),
                shared_append: true,
                sequences: None,
                join: false,
            });
        }
        let mut replacement = match &self.0 {
            Repr::Shared(layout) => Some(Index(Repr::Own(Box::new(Own::dense(
                layout.own(),
                u64::from(layout.len()),
            ))))),
            Repr::Own(_) => None,
        };
        let index = replacement.as_mut().unwrap_or(self);
        let Repr::Own(own) = &mut index.0 else {
            unreachable!("growth owns its index");
        };
        let join = own.width != 0;
        if join && own.seqs.len() != 1 {
            return Err(HeapFault::Malformed(
                "table growth needs a joined resident index".into(),
            ));
        }
        own.keys.try_reserve(inserts).map_err(|_| exhausted())?;
        let mut prepared_sequences = None;
        if removes && own.is_dense() {
            let count = usize::try_from(own.next)
                .ok()
                .and_then(|count| count.checked_add(inserts))
                .ok_or_else(exhausted)?;
            let mut sequences = Vec::new();
            sequences.try_reserve(count).map_err(|_| exhausted())?;
            sequences.extend(0..own.next);
            let mut chunks = Vec::new();
            chunks.try_reserve(1).map_err(|_| exhausted())?;
            chunks.push(sequences);
            prepared_sequences = Some(chunks);
        } else if !own.is_dense() {
            let Some(sequences) = own.seqs.first_mut() else {
                return Err(HeapFault::Malformed(
                    "table growth lost its sequences".into(),
                ));
            };
            sequences.try_reserve(inserts).map_err(|_| exhausted())?;
        }
        Ok(PreparedIndexGrowth {
            replacement,
            shared_append: false,
            sequences: prepared_sequences,
            join,
        })
    }
    pub(crate) fn insert_prepared(&mut self, key: Key, sequence: u64) {
        let Repr::Own(own) = &mut self.0 else {
            unreachable!("prepared insertion has an owned index");
        };
        if !own.is_dense() {
            own.seqs[0].push(sequence);
        }
        own.next = sequence + 1;
        own.keys.insert(key, sequence);
    }
    pub(crate) fn remove_prepared(&mut self, key: &Key, at: usize) {
        let Repr::Own(own) = &mut self.0 else {
            unreachable!("prepared removal has an owned index");
        };
        own.seqs[0].remove(at);
        own.keys.remove(key);
    }
}
impl PreparedIndexGrowth {
    pub(crate) fn install(self, index: &mut Index) -> bool {
        if let Some(replacement) = self.replacement {
            *index = replacement;
        }
        if let Repr::Own(own) = &mut index.0 {
            if let Some(sequences) = self.sequences {
                own.seqs = sequences;
            }
            if self.join {
                own.width = 0;
                own.absent.clear();
            }
        }
        self.shared_append
    }
}
