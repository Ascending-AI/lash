use super::{RuntimeCommit, RuntimeTurnCommitStamp, StoreError};

/// Test-only probes and fault-injection seams on a runtime store handle.
///
/// This trait is the home for every `*_for_testing` hook the conformance and
/// differential suites need from a backend. It is compiled only under
/// `cfg(any(test, feature = "testing"))`, is never a supertrait of a
/// production store trait, and is reached only through the
/// [`ConformancePersistence`] alias the suites take. Backends implement it
/// under the same gate (`lash-s3-store` sets the pattern with its
/// `cfg`-gated `raw_blobs_for_testing`).
#[async_trait::async_trait]
pub trait StoreTestSupport: Send + Sync {
    /// Rewrite the versioned session-head authority bytes in the real backend.
    ///
    /// `None` removes `config.tool_access`; `Some` writes the supplied raw JSON.
    /// Conformance uses this only to prove current malformed records and
    /// predecessor formats refuse through production read paths.
    async fn rewrite_session_tool_access_for_testing(
        &self,
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
        _version: u32,
    ) -> Result<(), StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "stamp_session_state_version_for_testing",
        })
    }

    /// Conformance seam for a marker guarding bytes the current codec cannot read.
    async fn stamp_session_state_version_and_corrupt_payload_for_testing(
        &self,
        _version: u32,
    ) -> Result<(), StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "stamp_session_state_version_and_corrupt_payload_for_testing",
        })
    }

    /// Execute one session-ingress settlement, fenced by `fence`, in a
    /// transaction of its own.
    ///
    /// Production settles ingress claims inside the head commit that
    /// delivers or applies them; the ingress laws drive the same planner and
    /// the same row writes through this seam until that commit carries them.
    async fn settle_session_ingress_for_testing(
        &self,
        _fence: &super::DriveFence,
        _settlement: super::IngressClaimSettlement,
    ) -> Result<super::IngressSettlementReceipt, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "settle_session_ingress_for_testing",
        })
    }

    /// Every session-ingress row of `session_id`, open and tombstoned, in
    /// `(lane, enqueue_seq)` order.
    async fn session_ingress_rows_for_testing(
        &self,
        _session_id: &crate::SessionId,
    ) -> Result<Vec<super::IngressItem>, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "session_ingress_rows_for_testing",
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

/// A runtime store handle together with its test-only hooks: the handle type
/// the conformance and differential suites take.
///
/// Blanket-implemented for every `RuntimePersistence + StoreTestSupport`
/// type, under the same gate as [`StoreTestSupport`]. An
/// `Arc<dyn ConformancePersistence>` upcasts to `Arc<dyn RuntimePersistence>`
/// wherever production code is exercised.
pub trait ConformancePersistence: super::RuntimePersistence + StoreTestSupport {}

impl<T> ConformancePersistence for T where T: super::RuntimePersistence + StoreTestSupport + ?Sized {}
