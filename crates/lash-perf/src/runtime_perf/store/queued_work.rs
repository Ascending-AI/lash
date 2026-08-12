use std::collections::BTreeSet;

use lash_core::LeaseOwnerIdentity;
use lash_core::runtime::QueuedWorkBatch;

#[derive(Clone)]
pub(super) struct RuntimePerfQueuedBatch {
    pub(super) batch: QueuedWorkBatch,
    pub(super) claim_id: Option<String>,
    pub(super) claim_token: Option<String>,
    pub(super) claim_owner: Option<LeaseOwnerIdentity>,
    pub(super) claim_fencing_token: u64,
    pub(super) claim_session_lease_generation: u64,
}

pub(super) struct SelectedBatchPresence {
    pub(super) requested_ids: BTreeSet<String>,
    pub(super) present_ids: BTreeSet<String>,
    pub(super) already_satisfied_batch_ids: Vec<String>,
}

pub(super) fn selected_batch_presence(
    queued: &[RuntimePerfQueuedBatch],
    session_id: &str,
    batch_ids: &[String],
) -> Option<SelectedBatchPresence> {
    let requested_ids = batch_ids.iter().cloned().collect::<BTreeSet<_>>();
    if requested_ids.len() != batch_ids.len() {
        return None;
    }
    let present_ids = queued
        .iter()
        .filter(|entry| {
            entry.batch.session_id == session_id && requested_ids.contains(&entry.batch.batch_id)
        })
        .map(|entry| entry.batch.batch_id.clone())
        .collect::<BTreeSet<_>>();
    let already_satisfied_batch_ids = batch_ids
        .iter()
        .filter(|batch_id| !present_ids.contains(batch_id.as_str()))
        .cloned()
        .collect();
    Some(SelectedBatchPresence {
        requested_ids,
        present_ids,
        already_satisfied_batch_ids,
    })
}
