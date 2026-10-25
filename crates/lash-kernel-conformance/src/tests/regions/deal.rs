//! Stable matrix assignment by case identity, with one owner per complete sweep.

use super::*;

pub(super) struct Deal {
    group: Option<usize>,
}

impl Deal {
    pub(super) fn new(group: Option<usize>) -> Self {
        Self { group }
    }

    /// A case's name, independent of generated code and preceding cases,
    /// fixes its single owner. Bounds, slices and cross-resumes stay together;
    /// the case's natural run and formation witnesses execute only once.
    pub(super) fn mine(&self, case: &Case) -> bool {
        let Some(group) = self.group else {
            return false;
        };
        let identity = case
            .name
            .bytes()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        (identity % GROUPS as u64) as usize == group
    }
}

/// FIG-7325: one identity owns the complete comparison domain, independently
/// of generated code size or unrelated cases being inserted before it.
#[test]
fn case_identity_owns_the_complete_sweep_independently_of_generated_source() {
    let original = program_case("stable case", "main { return null }", 16);
    let mut changed = original.clone();
    changed.document = original.document.repeat(1000);
    let assignment = |case: &Case, prefix: bool| {
        (0..GROUPS)
            .filter(|group| {
                let deal = Deal::new(Some(*group));
                if prefix {
                    deal.mine(&program_case("another case", "main {}", 16));
                }
                deal.mine(case)
            })
            .collect::<Vec<_>>()
    };
    let owners = assignment(&original, false);
    assert_eq!(owners.len(), 1);
    assert_eq!(owners, assignment(&changed, false));
    assert_eq!(owners, assignment(&original, true));
}
