use std::collections::HashMap;

#[derive(Clone)]
pub(super) struct RuntimeTurnCommitRecord {
    pub(super) turn_commit_hash: String,
    pub(super) result: crate::store::RuntimeCommitReceipt,
    pub(super) committed_at_ms: u64,
    pub(super) append_request_identity: crate::AppendRequestIdentity,
}

pub(super) type RuntimeTurnCommitMap = HashMap<(String, String), RuntimeTurnCommitRecord>;
