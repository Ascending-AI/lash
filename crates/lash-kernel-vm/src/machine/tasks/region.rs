//! One atomic source-counter installation, coupled to C's sealed execution.
//! This storage guard alone proves no source/ISA semantic correspondence.
use super::{HeapFault, Occurrences};

pub(in crate::machine) struct CounterLease<'a> {
    actual: &'a mut Occurrences,
    indexes: Box<[usize]>,
    entry: Box<[u64]>,
}

pub(in crate::machine) struct PreparedCounterExit<'a> {
    actual: &'a mut Occurrences,
    updates: Box<[(usize, u64)]>,
}

fn invalid(message: &str) -> HeapFault {
    HeapFault::Malformed(message.into())
}
fn exhausted() -> HeapFault {
    HeapFault::Exhausted {
        resource: lash_kernel_doc::heap::HostResource::StagingBytes,
    }
}

impl Occurrences {
    pub(in crate::machine) fn lease_counters<'a>(
        &'a mut self,
        indexes: &[usize],
    ) -> Result<CounterLease<'a>, HeapFault> {
        if indexes.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid(
                "region source counters are not distinct canonical indexes",
            ));
        }
        let mut bound = Vec::new();
        let mut entry = Vec::new();
        bound
            .try_reserve_exact(indexes.len())
            .map_err(|_| exhausted())?;
        entry
            .try_reserve_exact(indexes.len())
            .map_err(|_| exhausted())?;
        for &index in indexes {
            entry.push(
                self.current(index)
                    .ok_or_else(|| invalid("region source counter is absent"))?,
            );
            bound.push(index);
        }
        Ok(CounterLease {
            actual: self,
            indexes: bound.into_boxed_slice(),
            entry: entry.into_boxed_slice(),
        })
    }
}

impl<'a> CounterLease<'a> {
    pub(in crate::machine) fn entry(&self) -> &[u64] {
        &self.entry
    }

    pub(in crate::machine) fn prepare(
        self,
        reached: &[u64],
    ) -> Result<PreparedCounterExit<'a>, HeapFault> {
        if reached.len() != self.entry.len() {
            return Err(invalid("region source counter shape changed"));
        }
        let mut updates = Vec::new();
        updates
            .try_reserve_exact(reached.len())
            .map_err(|_| exhausted())?;
        for ((&index, &entry), &next) in self.indexes.iter().zip(self.entry.iter()).zip(reached) {
            if self.actual.current(index) != Some(entry) || next < entry {
                return Err(invalid(
                    "region source counter entry or ordered prefix changed",
                ));
            }
            if next != entry {
                updates.push((index, next));
            }
        }
        self.actual
            .dirty
            .try_reserve(updates.len())
            .map_err(|_| exhausted())?;
        Ok(PreparedCounterExit {
            actual: self.actual,
            updates: updates.into_boxed_slice(),
        })
    }
}

impl PreparedCounterExit<'_> {
    pub(in crate::machine) fn publish(self) {
        for &(index, next) in &self.updates {
            self.actual.counts[index] = next;
            if !self.actual.marked[index] {
                self.actual.marked[index] = true;
                self.actual.dirty.push(index);
            }
        }
    }
}

impl super::Tasks {
    /// Existing cold rows keep the ordinary lazy task/frame restoration path.
    pub(in crate::machine) fn reconstruct_region_context(&mut self) -> Result<(), HeapFault> {
        for (index, task) in self.cache.iter_mut().flatten() {
            task.frames
                .logical_context(lash_kernel_doc::TaskId(*index), self.logical.clone());
            task.frames.reconstruct_region_context()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod laws {
    use super::*;

    /// K-EVAL-008: abandoning or rejecting a staged prefix leaves every
    /// canonical occurrence unchanged, including unrelated source sites.
    #[test]
    fn region_counter_exit_preserves_the_entry_until_atomic_publication() {
        let mut actual = Occurrences::for_source_law_counts(&[17, 91, u64::MAX - 1]);
        assert!(actual.lease_counters(&[2, 0]).is_err());
        assert!(actual.lease_counters(&[0, 0]).is_err());
        assert!(actual.lease_counters(&[3]).is_err());
        let lease = actual.lease_counters(&[0, 2]).unwrap();
        assert_eq!(lease.entry(), [17, u64::MAX - 1]);
        drop(lease.prepare(&[18, u64::MAX]).unwrap());
        assert_eq!(&*actual.counts, [17, 91, u64::MAX - 1]);
        assert!(
            actual
                .lease_counters(&[0, 2])
                .unwrap()
                .prepare(&[16, u64::MAX])
                .is_err()
        );
        assert!(
            actual
                .lease_counters(&[0, 2])
                .unwrap()
                .prepare(&[18])
                .is_err()
        );
        assert_eq!(&*actual.counts, [17, 91, u64::MAX - 1]);
        actual
            .lease_counters(&[0, 2])
            .unwrap()
            .prepare(&[18, u64::MAX])
            .unwrap()
            .publish();
        assert_eq!(&*actual.counts, [18, 91, u64::MAX]);
        actual
            .lease_counters(&[0, 2])
            .unwrap()
            .prepare(&[21, u64::MAX])
            .unwrap()
            .publish();
        assert_eq!(&*actual.counts, [21, 91, u64::MAX]);
        assert_eq!(actual.dirty, [0, 2]);
    }
}
