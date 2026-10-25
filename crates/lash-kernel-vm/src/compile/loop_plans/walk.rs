//! Charge accounting while deriving a region.
use super::*;

/// What a statement's derivation, or an arm's, has seen so far: the fixed
/// units and the formulas' most it charges on its path, its program and
/// whether it allocates.
pub(super) struct Walk {
    pub(super) fixed: u64,
    pub(super) formula_max: Option<u64>,
    pub(super) program: Vec<Event>,
    pub(super) allocates: bool,
}

impl Walk {
    pub(super) fn new(fixed: u64, program: Vec<Event>) -> Self {
        Self {
            fixed,
            formula_max: Some(0),
            program,
            allocates: false,
        }
    }

    /// The charge regions `closes` of `stream`, charged where an op closes
    /// them: their totals and their events.
    pub(super) fn close(&mut self, stream: &Stream, closes: &[expr::Close]) -> Option<()> {
        for close in closes {
            self.fixed = self.fixed.checked_add(u64::from(close.total))?;
            self.program.extend(
                stream
                    .regions
                    .events(close.region)
                    .iter()
                    .map(|event| Event::Fixed(event.weight)),
            );
        }
        Some(())
    }
}
