use super::{RuntimeCommit, RuntimeTurnCommitStamp, StoreError};
use crate::{FrameNodeId, NodeId, SessionId};

/// Rows a store decoded since it opened, by kind (ADR 0112 §14): what the
/// residency conformance cases compare before and after a read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodedRowCounts {
    pub graph_node_bodies: u64,
    pub usage_rows: u64,
    pub usage_holes: u64,
    pub turn_receipt_bodies: u64,
}

/// One raw corruption of a stored graph row, for the corrupt-anchor cases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphRowCorruption {
    DeleteRow,
    SetParent(Option<NodeId>),
    SetFramePointer(NodeId),
    SetBodyBytes(u64),
    /// Rewrite the row to a valid `Plugin` payload,
    /// [`GraphRowCorruption::plugin_payload`], and set `body_bytes` to the
    /// new body's JSON length. The node id, parent, generation and frame
    /// pointer are preserved, so a window base becomes a non-`FrameOpen` row.
    SetPayloadKindToPlugin,
}

impl GraphRowCorruption {
    /// The payload [`Self::SetPayloadKindToPlugin`] writes: one fixed,
    /// decodable plugin payload, so every backend injects the same row.
    pub fn plugin_payload() -> crate::SessionNodePayload {
        crate::SessionNodePayload::Plugin {
            plugin_type: "lash-conformance/corrupt-anchor".to_string(),
            body: crate::session_graph::SharedJsonValue::new(serde_json::json!({})),
        }
    }
}

/// Test-only probes and fault-injection seams on a runtime store handle.
///
/// This trait is the home for every `*_for_testing` hook the conformance and
/// differential suites need from a backend. It is compiled only under
/// `cfg(any(test, feature = "testing"))`, is never a supertrait of a
/// production store trait, and is reached only through the
/// [`ConformanceStore`] alias the suites take. Backends implement it
/// under the same gate (`lash-s3-store` sets the pattern with its
/// `cfg`-gated `raw_blobs_for_testing`).
#[async_trait::async_trait]
pub trait StoreTestSupport: Send + Sync {
    /// Rows this store has decoded since it opened, counted per catalog.
    fn decoded_row_counts_for_testing(&self) -> DecodedRowCounts;

    /// Corrupt the stored row of `node_id` in place.
    async fn corrupt_graph_row_for_testing(
        &self,
        node_id: &NodeId,
        corruption: GraphRowCorruption,
    ) -> Result<(), StoreError>;

    /// Rewrite only the head's `current_frame_node_id` pointer of
    /// `session_id`, leaving its leaf as it is.
    async fn set_head_current_frame_for_testing(
        &self,
        session_id: &SessionId,
        frame: Option<FrameNodeId>,
    ) -> Result<(), StoreError>;

    /// Rewrite the versioned session-head authority bytes in the real backend.
    ///
    /// `None` removes `config.tool_access`; `Some` writes the supplied raw JSON.
    /// Conformance uses this only to prove current malformed records and
    /// predecessor formats refuse through production read paths.
    async fn rewrite_session_tool_access_for_testing(
        &self,
        _session_id: &SessionId,
        _schema_version: u32,
        _tool_access: Option<serde_json::Value>,
    ) -> Result<(), StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "rewrite_session_tool_access_for_testing",
        })
    }

    /// Conformance seam for a session a previous build left behind: rewrite
    /// only its physical session-state generation marker, leaving every
    /// guarded payload as it was written.
    async fn stamp_session_state_version_for_testing(
        &self,
        _session_id: &SessionId,
        _version: u32,
    ) -> Result<(), StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "stamp_session_state_version_for_testing",
        })
    }

    /// Conformance seam for a marker guarding bytes the current codec cannot read.
    async fn stamp_session_state_version_and_corrupt_payload_for_testing(
        &self,
        _session_id: &SessionId,
        _version: u32,
    ) -> Result<(), StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "stamp_session_state_version_and_corrupt_payload_for_testing",
        })
    }
}

/// Build an identity-bearing append commit with a caller-owned clock.
pub fn append_request_commit_with_clock_for_testing(
    state: &mut crate::RuntimeSessionState,
    operation_id: &str,
    nodes: &[crate::SessionAppendNode],
    requested_ancestor_node_id: Option<&str>,
    clock: &dyn crate::Clock,
) -> Result<RuntimeCommit, StoreError> {
    let operation = crate::runtime::state::boundary_operation(
        &state.session_id,
        operation_id,
        "append-session-nodes",
    );
    let stamp = RuntimeTurnCommitStamp::append_session_nodes(
        operation.clone(),
        requested_ancestor_node_id,
        nodes,
    )?;
    let draft_namespace = operation.storage_key()?;
    crate::runtime::state::append_session_nodes_to_state_with_clock(
        state,
        nodes,
        &draft_namespace,
        clock,
    );
    let mut graph = state.pending_graph_commit();
    graph.derive_node_ids(&state.session_id, &operation)?;
    let mut commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        state,
        graph,
        &[],
        operation,
    )?;
    commit.turn_commit = stamp;
    commit.debug_assert_append_envelope_scope();
    Ok(commit)
}

/// A runtime store together with its test-only hooks: the store type the
/// conformance and differential suites take.
///
/// Blanket-implemented for every `RuntimeStore + StoreTestSupport` type,
/// under the same gate as [`StoreTestSupport`]. An `Arc<dyn ConformanceStore>`
/// upcasts to `Arc<dyn RuntimeStore>` wherever production code is exercised.
pub trait ConformanceStore: super::RuntimeStore + StoreTestSupport {}

impl<T> ConformanceStore for T where T: super::RuntimeStore + StoreTestSupport + ?Sized {}
