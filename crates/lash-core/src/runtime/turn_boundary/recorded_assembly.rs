//! The committed content of one turn that the commit reads from the
//! driver's recorded state rather than from history (ADR 0105 §1; FIG-3672
//! P6).
//!
//! The driver records here, from each code cell's recorded response, the
//! outputs the turn's cells retained out of history. None of it is read back
//! from the observation stream the host receives, so how and when
//! observations are published cannot change what a turn commits, and a
//! replay rebuilds the same content from the same recorded outcomes.

/// The committed content of one turn, folded in the driver's program order.
pub struct RecordedTurnAssembly {
    /// Outputs the turn's code cells retained out of history (FIG-1643), in
    /// cell order, from each cell's recorded response.
    pub(in crate::runtime) retained_outputs: Vec<crate::RetainedOutput>,
}

impl Default for RecordedTurnAssembly {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordedTurnAssembly {
    pub(in crate::runtime) fn new() -> Self {
        Self {
            retained_outputs: Vec::new(),
        }
    }

    /// Record the outputs a code cell's recorded response retained out of
    /// history: its prints and its finish value (FIG-1643). History holds
    /// only their witnesses and references, inside protocol records the
    /// commit cannot read, so the commit names them from here.
    pub fn note_code_outputs(&mut self, response: &crate::ExecResponse) {
        self.retained_outputs.extend(
            response
                .output_archive
                .as_ref()
                .into_iter()
                .chain(response.terminal_finish_retained.as_ref())
                .cloned(),
        );
    }
}
