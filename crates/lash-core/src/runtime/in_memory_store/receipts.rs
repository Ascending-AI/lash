use std::collections::HashMap;

#[derive(Clone)]
pub(super) struct RuntimeTurnCommitRecord {
    pub(super) turn_commit_hash: String,
    pub(super) result: crate::store::RuntimeCommitResult,
    pub(super) committed_at_ms: u64,
    pub(super) request_identity_hash: Option<String>,
    pub(super) requested_node_count: Option<usize>,
    pub(super) _requested_ancestor_node_id: Option<String>,
    pub(super) identity_encoding_version: Option<u32>,
}

pub(super) type RuntimeTurnCommitMap = HashMap<(String, String), RuntimeTurnCommitRecord>;

pub(super) fn replay_existing_runtime_commit(
    stored: RuntimeTurnCommitRecord,
    attempted_commit_hash: &str,
    attempted: &crate::RuntimeTurnCommitStamp,
    session_id: String,
    operation_key: String,
) -> Result<crate::store::RuntimeCommitResult, crate::store::StoreError> {
    let stored_count = stored
        .requested_node_count
        .map(u64::try_from)
        .transpose()
        .map_err(|_| {
            crate::store::StoreError::Backend(
                "stored append requested-node count does not fit u64".to_string(),
            )
        })?;
    let attempted_count = attempted
        .requested_node_count
        .map(u64::try_from)
        .transpose()
        .map_err(|_| {
            crate::store::StoreError::Backend(
                "attempted append requested-node count does not fit u64".to_string(),
            )
        })?;
    match crate::store::decide_runtime_commit_receipt(
        &stored.turn_commit_hash,
        attempted_commit_hash,
        stored.identity_encoding_version,
        attempted.identity_encoding_version,
        stored.request_identity_hash.as_deref(),
        attempted.request_identity_hash.as_deref(),
        stored_count,
        attempted_count,
    ) {
        crate::store::RuntimeCommitReceiptDecision::Replay => {
            let mut result = stored.result;
            result.receipt_replayed = true;
            Ok(result)
        }
        crate::store::RuntimeCommitReceiptDecision::AppendIdentityConflict => {
            Err(crate::store::StoreError::AppendOperationIdentityConflict {
                session_id,
                operation_key,
            })
        }
        crate::store::RuntimeCommitReceiptDecision::RuntimeCommitConflict => {
            Err(crate::store::StoreError::RuntimeTurnCommitConflict {
                session_id,
                turn_id: operation_key,
            })
        }
        crate::store::RuntimeCommitReceiptDecision::CorruptRequestedNodeCount {
            stored,
            attempted,
        } => Err(
            crate::store::StoreError::AppendReceiptRequestedNodeCountCorrupt {
                session_id,
                operation_key,
                stored,
                attempted,
            },
        ),
    }
}

pub(super) fn enforce_fresh_append_ancestor(
    graph: &std::sync::Mutex<crate::SessionGraph>,
    attempted: &crate::RuntimeTurnCommitStamp,
) -> Result<(), crate::store::StoreError> {
    if attempted.request_identity_hash.is_some()
        && let Some(required_node_id) = attempted.requested_ancestor_node_id.as_deref()
        && !graph
            .lock()
            .expect("lock graph for append ancestor fence")
            .active_path_contains(required_node_id)
    {
        return Err(crate::store::StoreError::AppendAncestorNotActive {
            required_node_id: required_node_id.to_string(),
        });
    }
    Ok(())
}
