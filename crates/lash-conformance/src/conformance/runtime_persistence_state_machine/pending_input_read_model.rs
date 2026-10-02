use super::{ReferenceModel, admitted_input_ids};

/// Every modeled input as the store reads it: `Admitted{run}` while the
/// unfinished run holds it, `Open` otherwise.
pub(super) fn pending_input_reads(model: &ReferenceModel) -> Vec<crate::PendingTurnInputRead> {
    let held = admitted_input_ids(model);
    let run = model.run.as_ref().map(|run| run.run.clone());
    let mut inputs = model
        .inputs
        .values()
        .map(|modeled| {
            let input = modeled.input.clone();
            match &run {
                Some(run) if held.contains(&input.input_id) => {
                    crate::PendingTurnInputRead::admitted(input, run.clone())
                }
                _ => crate::PendingTurnInputRead::open(input),
            }
        })
        .collect::<Vec<_>>();
    inputs.sort_by_key(|read| read.input.enqueue_seq);
    inputs
}
