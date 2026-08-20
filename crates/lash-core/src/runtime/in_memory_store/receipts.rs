use std::collections::HashMap;

#[derive(Clone)]
pub(super) struct RuntimeTurnCommitRecord {
    pub(super) turn_commit_hash: String,
    pub(super) result: crate::store::RuntimeCommitReceipt,
    pub(super) committed_at_ms: u64,
    pub(super) request_identity_hash: Option<String>,
    pub(super) requested_node_count: Option<usize>,
    pub(super) _requested_ancestor_node_id: Option<String>,
    pub(super) identity_encoding_version: Option<u32>,
}

pub(super) type RuntimeTurnCommitMap = HashMap<(String, String), RuntimeTurnCommitRecord>;
