use super::{ReferenceModel, admitted_input_ids};

/// Every modeled input as the store reads it: `Admitted{root}` while the
/// unfinished root holds it, `Open` otherwise.
pub(super) fn pending_input_reads(model: &ReferenceModel) -> Vec<crate::PendingTurnInputRead> {
    let held = admitted_input_ids(model);
    let root = model.root.as_ref().map(|root| root.root.clone());
    let mut inputs = model
        .inputs
        .values()
        .map(|modeled| {
            let input = modeled.input.clone();
            match &root {
                Some(root) if held.contains(&input.input_id) => {
                    crate::PendingTurnInputRead::admitted(input, root.clone())
                }
                _ => crate::PendingTurnInputRead::open(input),
            }
        })
        .collect::<Vec<_>>();
    inputs.sort_by_key(|read| read.input.enqueue_seq);
    inputs
}
