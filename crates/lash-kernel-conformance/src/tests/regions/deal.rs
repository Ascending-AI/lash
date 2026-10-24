//! Stable matrix assignment by case identity, with one owner per slice.

use super::*;

pub(super) struct Deal {
    group: Option<usize>,
}

impl Deal {
    pub(super) fn new(group: Option<usize>) -> Self {
        Self { group }
    }

    /// A case's name, independent of generated code and preceding cases,
    /// fixes its groups. Each slice keeps its cross-resumes and bounds;
    /// distinct groups own distinct slices, so no sweep is overwritten.
    pub(super) fn mine(&mut self, case: &Case, _variants: u128, counts: &mut Counts) -> bool {
        let Some(group) = self.group else {
            return false;
        };
        let identity = case
            .name
            .bytes()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        let first = (identity % GROUPS as u64) as usize;
        let part = (group + GROUPS - first) % GROUPS;
        if part >= SLICES.len() {
            return false;
        }
        counts.sweep = Sweep {
            part,
            parts: SLICES.len(),
        };
        true
    }
}

/// FIG-7325: a case keeps its selectors when lowering or another case changes;
/// every slice's comparisons still belong to exactly one selector.
#[test]
fn case_identity_owns_each_slice_independently_of_generated_source() {
    let original = program_case("stable case", "main { return null }", 16);
    let mut changed = original.clone();
    changed.document = original.document.repeat(1000);
    let assignment = |case: &Case, prefix: bool| {
        let mut owners = vec![Vec::new(); SLICES.len()];
        for group in 0..GROUPS {
            let mut deal = Deal::new(Some(group));
            let mut counts = Counts::default();
            if prefix {
                deal.mine(&program_case("another case", "main {}", 16), 1, &mut counts);
            }
            if deal.mine(case, 2, &mut counts) {
                for (slice, owners) in owners.iter_mut().enumerate() {
                    if counts.sweep.owns(slice) {
                        owners.push(group);
                    }
                }
            }
        }
        owners
    };
    let owners = assignment(&original, false);
    assert!(owners.iter().all(|owners| owners.len() == 1));
    assert_eq!(owners, assignment(&changed, false));
    assert_eq!(owners, assignment(&original, true));
}
