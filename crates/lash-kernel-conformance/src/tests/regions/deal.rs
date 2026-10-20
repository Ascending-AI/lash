//! Deterministic matrix assignment by program size and comparison capacity.

use super::*;

pub(super) struct Deal {
    group: Option<usize>,
    loads: [u128; GROUPS],
}

impl Deal {
    pub(super) fn new(group: Option<usize>) -> Self {
        Self {
            group,
            loads: [0; GROUPS],
        }
    }

    /// Each caller visits the same ordered programs. A large matrix is split
    /// across at most one part per slice; a slice keeps its cross-resumes and
    /// sliced bounds. Every part goes to a distinct least-loaded group, so the
    /// same program can never overwrite a group's selected sweep.
    pub(super) fn mine(&mut self, case: &Case, variants: u128, counts: &mut Counts) -> bool {
        if self.group.is_none() {
            return false;
        }
        let cost = cost(case) * variants;
        let parts = cost.div_ceil(250_000).clamp(1, SLICES.len() as u128) as usize;
        let mut assigned = Vec::with_capacity(parts);
        let mut mine = false;
        for part in 0..parts {
            let group = (0..GROUPS)
                .filter(|group| !assigned.contains(group))
                .min_by_key(|&group| (self.loads[group], group))
                .expect("there are more groups than sweep parts");
            self.loads[group] += cost.div_ceil(parts as u128);
            assigned.push(group);
            if Some(group) == self.group {
                counts.sweep = Sweep { part, parts };
                mine = true;
            }
        }
        mine
    }
}

/// A deterministic upper-matrix proxy: source bytes times the maximum bounds
/// and the six comparisons per slice (Exact, Off, cross-resume and three
/// bounded slices). The program text includes helper/callee definitions, so a
/// large body costs more than a small one even when both hit the bound cap.
/// Estimating must not itself run every program in every group: that repeats
/// expensive natural runs even in groups that own none of their comparisons.
fn cost(case: &Case) -> u128 {
    case.document.len() as u128 * (BOUNDS + SLICES.len() * 6) as u128
}
