use super::{ReferenceModel, active_input_ids};

pub(super) fn pending_input_reads(model: &ReferenceModel) -> Vec<crate::PendingTurnInputRead> {
    let held = active_input_ids(model);
    let live_lease_expiry = model
        .current_lease
        .as_ref()
        .map(|lease| lease.fencing_token);
    let mut inputs = model
        .inputs
        .values()
        .map(|modeled| {
            let input = modeled.input.clone();
            match live_lease_expiry {
                Some(drive_epoch) if held.contains(&input.input_id) => {
                    crate::PendingTurnInputRead::held(input, drive_epoch)
                }
                _ => crate::PendingTurnInputRead::pending(input),
            }
        })
        .collect::<Vec<_>>();
    inputs.sort_by_key(|read| read.input.enqueue_seq);
    inputs
}
