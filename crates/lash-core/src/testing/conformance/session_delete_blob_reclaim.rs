//! Session-owner blob reclaim laws shared by every factory backend.

use super::session_store_factory::session_store_request;
use super::*;

/// Backend observation and fault seam for the session-delete blob laws.
///
/// Integrator class (ADR 0051): **conformance-suite embedders** implement this
/// probe for a custom store so the shared laws can observe and fault its exact
/// blob-deletion boundary.
#[async_trait::async_trait]
pub trait SessionDeleteBlobProbe: Send + Sync {
    /// Observe whether one exact content address exists.
    ///
    /// Integrator class (ADR 0051): **conformance-suite embedders** implement
    /// this probe operation for their backend.
    async fn blob_exists(&self, blob_ref: &crate::BlobRef) -> bool;

    /// Make the next deletion of a reclaimable session-owned blob fail.
    ///
    /// Integrator class (ADR 0051): **conformance-suite embedders** implement
    /// this fault injection at their backend's delete boundary.
    async fn fail_next_blob_delete(&self);

    /// Remove any backend fault object that outlives the failed transaction.
    ///
    /// Integrator class (ADR 0051): **conformance-suite embedders** implement
    /// this when their injected fault persists beyond one transaction.
    async fn clear_blob_delete_failure(&self) {}

    /// Break the factory-global GC scope while leaving exact edge rows intact.
    /// Backends with no fallible GC scope return `false`.
    ///
    /// Integrator class (ADR 0051): **conformance-suite embedders** implement
    /// this when their backend exposes a separately fallible global GC scope.
    async fn break_factory_gc_scope(&self, _checkpoint_ref: &crate::BlobRef) -> bool {
        false
    }
}

/// Factory and observation handles consumed by the session-delete blob laws.
///
/// Integrator class (ADR 0051): **conformance-suite embedders** construct this
/// pair for each fresh custom-backend test instance.
pub struct SessionDeleteBlobHandles {
    /// Fresh session-store factory under conformance test.
    ///
    /// Integrator class (ADR 0051): **conformance-suite embedders** supply this
    /// handle for their backend.
    pub factory: Arc<dyn crate::SessionStoreFactory>,
    /// Backend-specific exact-blob observation and fault handle.
    ///
    /// Integrator class (ADR 0051): **conformance-suite embedders** supply this
    /// handle for their backend.
    pub probe: Arc<dyn SessionDeleteBlobProbe>,
}

struct CommittedCheckpoint {
    request: crate::SessionStoreCreateRequest,
    store: Arc<dyn crate::RuntimePersistence>,
    checkpoint_ref: crate::BlobRef,
    component_refs: Vec<crate::BlobRef>,
    leaf_node_id: String,
}

async fn committed_checkpoint(
    factory: &Arc<dyn crate::SessionStoreFactory>,
    session_id: &str,
) -> CommittedCheckpoint {
    let request = session_store_request(
        session_id,
        "session-delete-blob-reclaim-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create blob-reclaim session");
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.policy.clone())
    };
    state.ensure_agent_frame_initialized();
    let leaf_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("initialized session has a leaf");
    let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.checkpoint.components.insert(
        "conformance/session-delete-owned".to_string(),
        crate::HydratedCheckpointComponent::changed(
            format!("session-delete-owned:{session_id}").into_bytes(),
        ),
    );
    let receipt = store
        .commit_runtime_state(commit)
        .await
        .expect("commit blob-reclaim checkpoint");
    let component_refs = receipt
        .manifest
        .components
        .values()
        .map(|component| component.blob_ref.clone())
        .collect();
    CommittedCheckpoint {
        request,
        store,
        checkpoint_ref: receipt.checkpoint_ref,
        component_refs,
        leaf_node_id,
    }
}

async fn assert_components_exist(
    backend: &str,
    probe: &dyn SessionDeleteBlobProbe,
    refs: &[crate::BlobRef],
    expected: bool,
) {
    for blob_ref in refs {
        assert_eq!(
            probe.blob_exists(blob_ref).await,
            expected,
            "{backend}: component blob `{}` existence mismatch",
            blob_ref.as_str()
        );
    }
}

/// Prove session deletion reclaims only blobs whose final exact edge it severs.
///
/// Integrator class (ADR 0051): **conformance-suite embedders** run this against
/// each custom session-store backend.
pub async fn session_delete_blob_reclaim_conformance<F>(backend: &str, make: F)
where
    F: Fn() -> SessionDeleteBlobHandles,
{
    session_delete_reclaims_exclusive_checkpoint_blobs(backend, make()).await;
    session_delete_keeps_fork_shared_checkpoint_blobs(backend, make()).await;
    session_delete_blob_failure_rolls_back_with_partial_report(backend, make()).await;
    session_delete_ignores_broken_factory_gc_scope(backend, make()).await;
}

async fn session_delete_reclaims_exclusive_checkpoint_blobs(
    backend: &str,
    handles: SessionDeleteBlobHandles,
) {
    let committed = committed_checkpoint(&handles.factory, "delete-exclusive-blobs").await;
    assert_components_exist(
        backend,
        handles.probe.as_ref(),
        &committed.component_refs,
        true,
    )
    .await;

    let report = handles
        .factory
        .delete_session(&committed.request.session_id)
        .await
        .expect("delete exclusively rooted checkpoint");
    assert_eq!(
        crate::MaintenanceReport::sweep(&report),
        crate::MaintenanceSweep::Swept,
        "{backend}: exclusive checkpoint delete must reclaim blobs"
    );
    assert!(report.enumerated_blob_count >= committed.component_refs.len());
    assert_eq!(report.retained_blob_count, 0);
    assert_components_exist(
        backend,
        handles.probe.as_ref(),
        &committed.component_refs,
        false,
    )
    .await;
}

async fn session_delete_keeps_fork_shared_checkpoint_blobs(
    backend: &str,
    handles: SessionDeleteBlobHandles,
) {
    let committed = committed_checkpoint(&handles.factory, "delete-shared-source").await;
    let fork_request = crate::ForkSessionRequest {
        session_id: "delete-shared-fork".to_string(),
        node_id: committed.leaf_node_id,
        relation: crate::SessionRelation::Root,
        policy: committed.request.policy.clone(),
    };
    handles
        .factory
        .fork_at(&fork_request)
        .await
        .expect("fork shared checkpoint");

    let source_report = handles
        .factory
        .delete_session(&committed.request.session_id)
        .await
        .expect("delete source with surviving fork");
    assert_eq!(source_report.deleted_blob_count, 0);
    assert_eq!(
        source_report.retained_blob_count, source_report.enumerated_blob_count,
        "{backend}: every source candidate remains referenced by the fork edge"
    );
    assert_components_exist(
        backend,
        handles.probe.as_ref(),
        &committed.component_refs,
        true,
    )
    .await;

    let fork_report = handles
        .factory
        .delete_session(&fork_request.session_id)
        .await
        .expect("delete final checkpoint referrer");
    assert!(fork_report.deleted_blob_count > 0);
    assert_components_exist(
        backend,
        handles.probe.as_ref(),
        &committed.component_refs,
        false,
    )
    .await;
}

async fn session_delete_blob_failure_rolls_back_with_partial_report(
    backend: &str,
    handles: SessionDeleteBlobHandles,
) {
    let committed = committed_checkpoint(&handles.factory, "delete-blob-failure").await;
    handles.probe.fail_next_blob_delete().await;
    let failure = handles
        .factory
        .delete_session(&committed.request.session_id)
        .await
        .expect_err("injected blob failure must fail session delete");
    handles.probe.clear_blob_delete_failure().await;
    assert!(matches!(failure.stop, crate::MaintenanceStop::Failed(_)));
    assert!(
        failure.partial.enumerated_blob_count >= committed.component_refs.len(),
        "{backend}: failure must carry the witnessed candidate scope"
    );
    assert_eq!(
        failure.partial.deleted_blob_count, 0,
        "{backend}: rolled-back deletes must not be reported as durable work"
    );
    assert!(
        handles
            .factory
            .open_existing_store(&committed.request)
            .await
            .expect("open after failed delete")
            .is_some(),
        "{backend}: blob failure must roll the owning delete back"
    );
    assert_components_exist(
        backend,
        handles.probe.as_ref(),
        &committed.component_refs,
        true,
    )
    .await;
}

async fn session_delete_ignores_broken_factory_gc_scope(
    backend: &str,
    handles: SessionDeleteBlobHandles,
) {
    let victim = committed_checkpoint(&handles.factory, "delete-with-broken-gc-victim").await;
    let survivor = committed_checkpoint(&handles.factory, "delete-with-broken-gc-survivor").await;
    if !handles
        .probe
        .break_factory_gc_scope(&survivor.checkpoint_ref)
        .await
    {
        tracing::warn!(
            backend,
            "backend has no fallible factory-GC scope to isolate"
        );
        return;
    }
    assert!(
        survivor.store.gc_unreachable().await.is_err(),
        "{backend}: fault must break the factory-global GC lever"
    );
    handles
        .factory
        .delete_session(&victim.request.session_id)
        .await
        .expect("broken factory GC must not abort an exact-edge session delete");
    assert_components_exist(
        backend,
        handles.probe.as_ref(),
        &victim.component_refs,
        false,
    )
    .await;
}

#[async_trait::async_trait]
impl SessionDeleteBlobProbe for crate::InMemorySessionStoreFactory {
    async fn blob_exists(&self, blob_ref: &crate::BlobRef) -> bool {
        self.checkpoint_blob_exists_for_testing(blob_ref)
    }

    async fn fail_next_blob_delete(&self) {
        self.fail_next_session_blob_delete_for_testing();
    }
}
