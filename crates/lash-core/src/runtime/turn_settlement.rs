//! What one turn's commit settles of the rows its root admitted (FIG-3927).
//!
//! A turn completes the rows it delivered and hands back, open at their own
//! positions, the rows it withheld and will not deliver. Every row is named
//! by its id and settled under the root that holds it; nothing here carries
//! authority of its own. The root's drive fence, presented by the commit, is
//! the one authority.

use crate::runtime::logical_turn::WithheldTerminalWork;
use crate::store::{IngressRowId, IngressSettlement};

#[derive(Clone, Default)]
pub(super) struct TurnIngressSettlement {
    pub(super) completed_batches: Vec<crate::QueuedWorkCompletion>,
    pub(super) completed_inputs: Vec<crate::TurnInputCompletion>,
    /// Work withheld from a terminal checkpoint that no follow-on turn will
    /// drive: a cancelled turn's (FIG-3531, FIG-3543), or one whose run spent
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

    /// The settlement `root`'s commit carries, each row named once: a row
    /// the turn completed is never also handed back, and a row two admitted
    /// sets name (a replayed checkpoint re-delivering its own rows) settles
    /// once. `disposition` is the cancellation's undelivered disposition,
    /// `Defer` when the turn was not cancelled.
    pub(super) fn into_ingress(
        self,
        root: crate::TurnId,
        disposition: crate::TurnCancelUndeliveredInputPolicy,
    ) -> IngressSettlement {
        let mut seen = std::collections::BTreeSet::new();
        let mut settlement = IngressSettlement::new(root);
        for mut completion in self.completed_inputs {
            completion
                .data
                .input_ids
                .retain(|input| seen.insert(IngressRowId::Input(input.clone())));
            if !completion.input_ids.is_empty() {
                let kept = completion.input_ids.clone();
                completion
                    .data
                    .applications
                    .retain(|application| kept.contains(&application.input_id));
                settlement.completed_inputs.push(completion);
            }
        }
        for mut completion in self.completed_batches {
            completion
                .batch_ids
                .retain(|batch| seen.insert(IngressRowId::Batch(batch.clone())));
            if !completion.batch_ids.is_empty() {
                settlement.completed_batches.push(completion);
            }
        }
        for queued in self.undelivered.queued {
            for batch in queued.batches {
                let row = IngressRowId::Batch(batch.batch_id);
                if seen.insert(row.clone()) {
                    settlement.released.push(row);
                }
            }
        }
        for inputs in self.undelivered.turn_inputs {
            for input in inputs.inputs {
                let row = IngressRowId::Input(input.input_id);
                if !seen.insert(row.clone()) {
                    continue;
                }
                match disposition {
                    crate::TurnCancelUndeliveredInputPolicy::Defer => settlement.released.push(row),
                    crate::TurnCancelUndeliveredInputPolicy::Drop => settlement.dropped.push(row),
                }
            }
        }
        settlement
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;

    fn completion(input_ids: &[&str]) -> crate::TurnInputCompletion {
        crate::TurnInputCompletion {
            session_id: SessionId::from("s"),
            data: crate::TurnInputCompletionData {
                input_ids: input_ids
                    .iter()
                    .map(|id| crate::InputId::from(*id))
                    .collect(),
                applications: Vec::new(),
            },
        }
    }

    #[test]
    fn a_row_two_admitted_sets_name_settles_once() {
        let settlement = TurnIngressSettlement::new(
            vec![crate::QueuedWorkCompletion {
                session_id: SessionId::from("s"),
                batch_ids: vec!["b1".into(), "b1".into()],
            }],
            vec![completion(&["i1", "i2"]), completion(&["i2", "i3"])],
        )
        .into_ingress(
            crate::TurnId::from("root"),
            crate::TurnCancelUndeliveredInputPolicy::Defer,
        );
        assert_eq!(
            settlement.rows(),
            vec![
                IngressRowId::Input("i1".into()),
                IngressRowId::Input("i2".into()),
                IngressRowId::Input("i3".into()),
                IngressRowId::Batch("b1".into()),
            ]
        );
        assert!(settlement.validate(&SessionId::from("s")).is_ok());
    }
}
