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
    /// Conformance seam for a marker guarding bytes the current codec cannot read.
    async fn stamp_session_state_version_and_corrupt_payload_for_testing(
        &self,
        _version: u32,
    ) -> Result<(), StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "stamp_session_state_version_and_corrupt_payload_for_testing",
        })
    }

    /// Seed the exact session-owned trigger-manifest artifact-ref namespace for
    /// deletion conformance and differential tests.
    ///
    /// Returns `false` when the backend has no artifact-ref namespace on this
    /// store surface (the in-memory runtime store is such a backend).
    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        session_id: &str,
    ) -> Result<bool, StoreError>;

    /// Return session-owned artifact-ref identities through this retained store
    /// handle. Values are `(namespace, artifact_ref)` pairs; physical pointer
    /// and body representations are deliberately excluded.
    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError>;
}

/// Build an identity-bearing append commit with a caller-owned clock.
#[doc(hidden)]
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

/// A [`SessionStoreFactory`](crate::SessionStoreFactory) that can also hand
/// out [`ConformancePersistence`] handles.
///
/// The production factory returns `Arc<dyn RuntimePersistence>`, which has no
/// test hooks; the factory-driven conformance suites take this gated trait and
/// create the handles they probe through it. Backends implement it under their
/// own `testing` gate, typically by sharing the concrete constructor behind
/// their production `create_store`.
#[async_trait::async_trait]
pub trait ConformanceSessionStoreFactory: crate::SessionStoreFactory {
    /// Create a session store exactly as `create_store` would, keeping the
    /// test-support hooks reachable on the returned handle.
    async fn create_conformance_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<std::sync::Arc<dyn ConformancePersistence>, StoreError>;

    /// Reopen a session store exactly as `open_existing_store` would, keeping
    /// the test-support hooks reachable on the returned handle.
    async fn open_existing_conformance_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Option<std::sync::Arc<dyn ConformancePersistence>>, String>;
}
