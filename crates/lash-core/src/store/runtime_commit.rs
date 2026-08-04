//! Runtime commit envelope and result types.

use super::{
    BlobRef, GraphAppend, HydratedSessionCheckpoint, OperationId, RealizedNodeTimestamp,
    SessionCheckpoint, SessionExecutionLeaseCompletion, StoreError, commit_identity,
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeCommit {
    pub session_id: String,
    pub expected_head_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_session_execution_lease: Option<SessionExecutionLeaseCompletion>,
    pub config: crate::PersistedSessionConfig,
    pub current_frame_node_id: Option<String>,
    pub graph: GraphAppend,
    pub checkpoint: HydratedSessionCheckpoint,
    /// Usage rows published atomically by this commit, each carrying a stable
    /// identity so retrying an unknown commit outcome cannot double-account.
    pub usage_deltas: Vec<RuntimeUsageDelta>,
    pub turn_commit: RuntimeTurnCommitStamp,
    pub completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
    pub completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_turn_input_turn_id: Option<String>,
    /// Attachment ids explicitly adopted by this commit. In the same
    /// transaction the backend also stamps every uncommitted manifest row owned
    /// by the turn id in `turn_commit.operation`, including ids that appear only in plain tool
    /// JSON. This list preserves typed-output and cross-turn re-references;
    /// adoption updates existing rows only and deliberately no-ops when this
    /// session has no matching intent.
    pub committed_attachment_ids: Vec<crate::AttachmentId>,
}

/// Durable identity for one usage row submitted through a runtime commit.
///
/// The operation key, ordinal, and payload hash are assigned before the first
/// commit attempt and must be reused byte-for-byte until a commit containing
/// the row has a confirmed outcome. Stores enforce uniqueness per session over
/// all three fields.
///
/// `payload_hash` is lowercase hexadecimal SHA-256 of the UTF-8 JSON bytes
/// emitted for [`crate::TokenLedgerEntry`] by Lash's stable serialization
/// helper (`serde_json::to_writer`, compact encoding, struct fields in declared
/// order, and JSON object keys in canonical map order). Binding the content
/// makes reuse of an operation ordinal for a different row a distinct durable
/// identity while preserving exact retry deduplication.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct RuntimeUsageDeltaIdentity {
    /// Canonical [`OperationId::storage_key`] of the operation that first
    /// staged this row.
    pub operation_storage_key: String,
    /// Zero-based row position within that operation's staged usage batch.
    pub entry_ordinal: u64,
    /// SHA-256 of the entry's canonical serialized content, encoded as 64
    /// lowercase hexadecimal characters.
    pub payload_hash: String,
}

impl RuntimeUsageDeltaIdentity {
    /// Construct the full identity for `entry` using Lash's canonical payload
    /// encoding.
    pub fn for_entry(
        operation_storage_key: String,
        entry_ordinal: u64,
        entry: &crate::TokenLedgerEntry,
    ) -> Result<Self, StoreError> {
        let payload_hash = crate::stable_hash::stable_json_sha256_hex(entry).map_err(|err| {
            StoreError::Backend(format!(
                "failed to canonically serialize runtime usage delta: {err}"
            ))
        })?;
        Ok(Self {
            operation_storage_key,
            entry_ordinal,
            payload_hash,
        })
    }
}

/// One identity-bearing usage row in a [`RuntimeCommit`].
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**
/// persist the identity beside the row and ignore a duplicate identity inside
/// the same transaction as the rest of the commit.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeUsageDelta {
    /// Stable exactly-once publication identity.
    pub identity: RuntimeUsageDeltaIdentity,
    /// Usage counters published under that identity.
    pub entry: crate::TokenLedgerEntry,
}

impl RuntimeUsageDelta {
    pub(crate) fn for_operation(
        operation: &OperationId,
        entries: &[crate::TokenLedgerEntry],
    ) -> Result<Vec<Self>, StoreError> {
        let operation_storage_key = operation.storage_key()?;
        entries
            .iter()
            .cloned()
            .enumerate()
            .map(|(ordinal, entry)| {
                let entry_ordinal = u64::try_from(ordinal).map_err(|_| {
                    StoreError::Backend(
                        "usage delta ordinal does not fit durable u64 identity".to_string(),
                    )
                })?;
                let identity = RuntimeUsageDeltaIdentity::for_entry(
                    operation_storage_key.clone(),
                    entry_ordinal,
                    &entry,
                )?;
                Ok(Self { identity, entry })
            })
            .collect()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeCommitResult {
    pub head_revision: u64,
    pub checkpoint_ref: BlobRef,
    pub manifest: SessionCheckpoint,
    /// Leaf selected by the committed operation. Receipt replay returns the
    /// first attempt's value even when later commits have advanced the session.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_leaf_node_id: Option<String>,
    /// Store-realized timestamps for nodes appended by this operation.
    ///
    /// Node timestamps are clock-derived and excluded from commit intent, so a
    /// receipt replay must return the first attempt's values for the resident
    /// graph to converge with durable history.
    pub realized_node_timestamps: Vec<RealizedNodeTimestamp>,
    /// Usage identities actually present in the transaction represented by
    /// this result. A replay returns the first attempt's list, allowing a host
    /// to retain re-ridden staged rows that the first attempt did not carry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committed_usage_delta_identities: Vec<RuntimeUsageDeltaIdentity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enqueued_queue_batches: Vec<crate::QueuedWorkBatch>,
    /// Canonical input applications settled by this idempotent turn commit.
    ///
    /// Keeping these identities in the durable turn-commit result lets hosts
    /// reconcile after the bounded live observation window has been lost.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_input_applications: Vec<crate::TurnInputApplication>,
    /// Whether the store answered this attempt from an existing durable receipt.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// set this transient decision bit when returning an earlier commit result;
    /// it is stored as `false` in the receipt itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub receipt_replayed: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeTurnCommitStamp {
    pub operation: OperationId,
    /// Version of the append-request canonical encoding, or `None` for a
    /// non-append operation or a legacy receipt.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_encoding_version: Option<u32>,
    /// SHA-256 identity of the semantic append request, or `None` when exact
    /// commit-hash replay semantics apply.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_identity_hash: Option<String>,
    /// Number of semantic nodes supplied by the append caller.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_node_count: Option<usize>,
    /// Branch ancestor named by the append caller, when one was required.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_ancestor_node_id: Option<String>,
}

impl RuntimeTurnCommitStamp {
    /// Binds one operation identity to a runtime commit for store implementors enforcing replay and
    /// idempotency at the atomic commit boundary.
    pub fn new(operation: OperationId) -> Self {
        Self {
            operation,
            identity_encoding_version: None,
            request_identity_hash: None,
            requested_node_count: None,
            requested_ancestor_node_id: None,
        }
    }

    pub(crate) fn append_session_nodes(
        operation: OperationId,
        requested_ancestor_node_id: Option<&str>,
        nodes: &[crate::SessionAppendNode],
    ) -> Result<Self, StoreError> {
        let request_identity_hash = commit_identity::append_request_identity_hash(
            &operation,
            requested_ancestor_node_id,
            nodes,
        )?;
        Ok(Self {
            operation,
            identity_encoding_version: Some(
                commit_identity::APPEND_REQUEST_IDENTITY_ENCODING_VERSION,
            ),
            request_identity_hash: Some(request_identity_hash),
            requested_node_count: Some(nodes.len()),
            requested_ancestor_node_id: requested_ancestor_node_id.map(str::to_string),
        })
    }
}
