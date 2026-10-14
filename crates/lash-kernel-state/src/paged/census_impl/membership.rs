use crate::paged::MembershipRun;
use lash_kernel_doc::heap::HeapFault;

/// Canonical range membership for a lazy frozen family. Absence always
/// wins over a frozen default or an override. A fork clones these runs.
#[derive(Clone, Debug)]
pub struct FamilyMembership {
    extent: u64,
    runs: Vec<MembershipRun>,
}

impl FamilyMembership {
    pub fn fresh(extent: u64) -> Self {
        Self {
            extent,
            runs: if extent == 0 {
                Vec::new()
            } else {
                vec![MembershipRun {
                    start: 0,
                    len: extent,
                    live: true,
                }]
            },
        }
    }

    /// Checks a persisted table without consulting the frozen contents.
    pub fn from_runs(extent: u64, runs: Vec<MembershipRun>) -> Result<Self, HeapFault> {
        let mut end = 0_u64;
        let mut previous = None;
        for run in &runs {
            if run.len == 0 || run.start != end || previous == Some(run.live) {
                return Err(HeapFault::Malformed(
                    "noncanonical family membership".into(),
                ));
            }
            end = end
                .checked_add(run.len)
                .ok_or_else(|| HeapFault::Malformed("family membership overflow".into()))?;
            previous = Some(run.live);
        }
        if end != extent {
            return Err(HeapFault::Malformed(
                "membership does not cover family extent".into(),
            ));
        }
        Ok(Self { extent, runs })
    }

    pub fn member(&self, ordinal: u64) -> bool {
        if ordinal >= self.extent {
            return false;
        }
        let index = self.runs.partition_point(|run| run.start <= ordinal);
        index != 0 && self.runs[index - 1].live
    }

    pub fn runs(&self) -> &[MembershipRun] {
        &self.runs
    }

    /// Tombstones a proved-unreachable ordinal. Repeating a sweep is safe;
    /// there is deliberately no operation that resurrects an ordinal.
    pub fn sweep(&mut self, ordinal: u64) -> Result<(), HeapFault> {
        if !self.member(ordinal) {
            return Ok(());
        }
        let index = self.runs.partition_point(|run| run.start <= ordinal) - 1;
        let run = self.runs[index];
        let end = run
            .start
            .checked_add(run.len)
            .ok_or_else(|| HeapFault::Malformed("family membership overflow".into()))?;
        self.runs.try_reserve(2).map_err(|_| HeapFault::Exhausted {
            resource: lash_kernel_doc::heap::HostResource::TraceBudget,
        })?;
        let mut replacement = Vec::with_capacity(3);
        if ordinal > run.start {
            replacement.push(MembershipRun {
                start: run.start,
                len: ordinal - run.start,
                live: true,
            });
        }
        replacement.push(MembershipRun {
            start: ordinal,
            len: 1,
            live: false,
        });
        if ordinal + 1 < end {
            replacement.push(MembershipRun {
                start: ordinal + 1,
                len: end - ordinal - 1,
                live: true,
            });
        }
        self.runs.splice(index..=index, replacement);
        // Only the neighbors of the split can have become adjacent tombstones.
        let mut i = index.saturating_sub(1);
        while i + 1 < self.runs.len() && i <= index + 2 {
            if self.runs[i].live == self.runs[i + 1].live {
                self.runs[i].len = self.runs[i]
                    .len
                    .checked_add(self.runs[i + 1].len)
                    .ok_or_else(|| HeapFault::Malformed("membership merge overflow".into()))?;
                self.runs.remove(i + 1);
            } else {
                i += 1;
            }
        }
        Ok(())
    }
}
