//! What one turn's commit settles of the rows its run admitted (FIG-3927).
//!
//! A turn completes the rows it delivered and hands back, open at their own
//! positions, the rows it withheld and will not deliver. Every row is named
//! by its id and settled under the run that holds it; nothing here carries
//! authority of its own. The run's shift fence, presented by the commit, is
//! the one authority.

use crate::runtime::logical_turn::WithheldTerminalWork;

#[derive(Clone, Default)]
pub(super) struct TurnIngressSettlement {
    pub(super) completed_batches: Vec<crate::QueuedWorkCompletion>,
    pub(super) completed_inputs: Vec<crate::TurnInputCompletion>,
    /// Work withheld from a terminal checkpoint that no follow-on turn will
    /// shift: a cancelled turn's (FIG-3531, FIG-3543), or one whose run spent
    /// its follow-on bound. Wakes are released and keep their redelivery
    /// floor; input is released or dropped by the cancellation's
    /// undelivered disposition.
    pub(super) undelivered: WithheldTerminalWork,
}

impl TurnIngressSettlement {
    pub(super) fn new(
        completed_batches: Vec<crate::QueuedWorkCompletion>,
        completed_inputs: Vec<crate::TurnInputCompletion>,
    ) -> Self {
        Self {
            completed_batches,
            completed_inputs,
            undelivered: WithheldTerminalWork::default(),
        }
    }

    pub(super) fn with_undelivered(mut self, undelivered: WithheldTerminalWork) -> Self {
        self.undelivered = undelivered;
        self
    }

    /// Whether the turn settles no row.
    pub(super) fn is_empty(&self) -> bool {
        self.completed_batches
            .iter()
            .all(|completion| completion.batch_ids.is_empty())
            && self
                .completed_inputs
                .iter()
                .all(|completion| completion.input_ids.is_empty())
            && self.undelivered.is_empty()
    }
}
