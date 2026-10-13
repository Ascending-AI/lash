//! Session-owner blob reclaim laws shared by every factory backend.

use super::session_store_factory::session_store_request;
use super::*;
use lash_sansio::SessionId;
use pretty_assertions::assert_eq;

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

    /// Integrator class (ADR 0051): **conformance-suite embedders** implement
    /// this fault injection at their backend's delete boundary.
    async fn fail_next_blob_delete(&self);

    /// Remove any backend fault object that outlives the failed transaction.
    ///
    /// Integrator class (ADR 0051): **conformance-suite embedders** implement
    /// this when their injected fault persists beyond one transaction.
    async fn clear_blob_delete_failure(&self) {}

    /// Observe one exact checkpoint-root projection edge when the backend
    /// materializes that projection. Backends without a projection table
    /// return `None`.
    async fn checkpoint_component_edge_exists(
        &self,
        _checkpoint_ref: &crate::BlobRef,
        _blob_ref: &crate::BlobRef,
    ) -> Option<bool> {
        None
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
    pub factory: Arc<dyn crate::DeploymentStore>,
    /// Backend-specific exact-blob observation and fault handle.
    ///
    /// Integrator class (ADR 0051): **conformance-suite embedders** supply this
    /// handle for their backend.
    pub probe: Arc<dyn SessionDeleteBlobProbe>,
    /// A fresh, empty attachment byte store the retention laws put through.
    pub attachments: Arc<dyn crate::AttachmentStore>,
}

struct CommittedCheckpoint {
    request: crate::SessionStoreCreateRequest,
    store: Arc<dyn crate::RuntimeStore>,
    checkpoint_ref: crate::BlobRef,
    manifest: crate::SessionCheckpoint,
    component_refs: Vec<crate::BlobRef>,
    leaf_node_id: String,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn committed_checkpoint(
    factory: &Arc<dyn crate::DeploymentStore>,
    session_id: &SessionId,
) -> CommittedCheckpoint {
    let request = session_store_request(
        session_id,
        "session-delete-blob-reclaim-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .admit_view(&request)
        .await
        .expect("create blob-reclaim session");
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::fixture(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    let leaf_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("initialized session has a leaf");
    let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
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
        store: Arc::clone(store.store()),
        checkpoint_ref: receipt.checkpoint_ref,
        manifest: receipt.manifest,
        component_refs,
        leaf_node_id: leaf_node_id.to_string(),
    }
}

pub(super) struct ContentAliasedCheckpointRoots {
    pub(super) dependent_root: crate::BlobRef,
    pub(super) aliased_root: crate::BlobRef,
    pub(super) aliased_component: crate::BlobRef,
    /// The OpaqueLeaf through which the aliased root names its component.
    pub(super) aliased_leaf: crate::BlobRef,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn encoded_checkpoint_manifest(manifest: &crate::SessionCheckpoint) -> Vec<u8> {
    let (root, nodes) = manifest
        .to_graph()
        .expect("encode checkpoint manifest for content alias");
    nodes[&root].bytes.to_vec()
}

/// The OpaqueLeaf through which `manifest` names component `name`: a
/// checkpoint root's edge goes to the leaf, and the leaf's to the
/// component's blob.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn component_leaf(manifest: &crate::SessionCheckpoint, name: &str) -> crate::BlobRef {
    let blob = &manifest.components[name].blob_ref;
    let (root, nodes) = manifest.to_graph().expect("stage the checkpoint manifest");
    nodes
        .values()
        .find(|node| {
            node.id != root
                && node.children().is_ok_and(|children| {
                    children.iter().any(|child| child.to_hex() == blob.as_str())
                })
        })
        .map(|leaf| crate::BlobRef::for_content(&leaf.bytes))
        .expect("the manifest names the component through a leaf")
}

/// Whether `root` still projects onto `blob` through `leaf`, when the
/// backend materializes edges: the root's edge to the leaf and the leaf's
/// to the blob both stand.
async fn projects(
    probe: &dyn SessionDeleteBlobProbe,
    root: &crate::BlobRef,
    leaf: &crate::BlobRef,
    blob: &crate::BlobRef,
) -> Option<bool> {
    let to_leaf = probe.checkpoint_component_edge_exists(root, leaf).await?;
    let to_blob = probe.checkpoint_component_edge_exists(leaf, blob).await?;
    Some(to_leaf && to_blob)
}

/// The nonce makes B sort before A, reproducing the restrictive-FK delete order that matters
/// to a multi-root reclaim batch.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn commit_content_aliased_checkpoint_roots(
    factory: &Arc<dyn crate::DeploymentStore>,
    dependent_session_id: &SessionId,
    aliased_session_id: &SessionId,
) -> ContentAliasedCheckpointRoots {
    let aliased = committed_checkpoint(factory, aliased_session_id).await;
    let aliased_root_bytes = encoded_checkpoint_manifest(&aliased.manifest);
    assert_eq!(
        crate::BlobRef::for_content(&aliased_root_bytes),
        aliased.checkpoint_ref,
        "the law must reproduce the backend's checkpoint-root content address"
    );

    let request = session_store_request(
        dependent_session_id,
        "session-delete-blob-reclaim-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .admit_view(&request)
        .await
        .expect("create dependent content-alias session");
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::fixture(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
    commit.checkpoint.components.insert(
        "conformance/content-aliased-root".to_string(),
        crate::HydratedCheckpointComponent::changed(aliased_root_bytes),
    );

    let predicted_dependent_root = (0_u64..10_000)
        .find_map(|nonce| {
            commit.checkpoint.components.insert(
                "conformance/content-alias-order".to_string(),
                crate::HydratedCheckpointComponent::changed(nonce.to_be_bytes().to_vec()),
            );
            let manifest = commit
                .checkpoint
                .manifest(store.fleet_format())
                .expect("project dependent root");
            let root = crate::BlobRef::for_content(&encoded_checkpoint_manifest(&manifest));
            (aliased.checkpoint_ref.as_str() < root.as_str()).then_some(root)
        })
        .expect("find a dependent checkpoint root that sorts after its aliased component root");
    let receipt = store
        .commit_runtime_state(commit)
        .await
        .expect("commit dependent content-alias checkpoint");
    assert_eq!(receipt.checkpoint_ref, predicted_dependent_root);
    assert_eq!(
        receipt
            .manifest
            .component_ref("conformance/content-aliased-root"),
        Some(&aliased.checkpoint_ref),
        "root B's bytes must be stored as an opaque component of root A"
    );

    ContentAliasedCheckpointRoots {
        dependent_root: receipt.checkpoint_ref,
        aliased_leaf: component_leaf(&aliased.manifest, "conformance/session-delete-owned"),
        aliased_root: aliased.checkpoint_ref,
        aliased_component: aliased
            .component_refs
            .into_iter()
            .next()
            .expect("aliased root has one owned component"),
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn advance_checkpoint(
    store: &Arc<dyn crate::RuntimeStore>,
    session_id: &SessionId,
    retain_owned_component: bool,
) -> crate::store::RuntimeCommitReceipt {
    let mut state = super::helpers::load_window_state(store, session_id)
        .await
        .expect("hydrate the current head")
        .expect("the session has a committed head");
    let mut snapshot = state.to_snapshot();
    snapshot.turn_index += 1;
    state.adopt_snapshot(snapshot);
    let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
    if !retain_owned_component {
        commit
            .checkpoint
            .components
            .remove("conformance/session-delete-owned");
    }
    lash_core::testing::store_fixtures::commit_runtime_state_for_test(
        store,
        commit,
        "session-delete-advance",
    )
    .await
    .expect("advance the session head")
}

/// Superseded checkpoints must not leave edges to reclaimed shared components.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_delete_reclaims_after_head_advances<F>(backend: &str, make: F)
where
    F: Fn() -> SessionDeleteBlobHandles,
{
    let handles = make();
    let first =
        committed_checkpoint(&handles.factory, &SessionId::from("delete-advanced-head")).await;
    let second = advance_checkpoint(&first.store, &first.request.session_id, true).await;
    let third = advance_checkpoint(&first.store, &first.request.session_id, true).await;
    assert_ne!(first.checkpoint_ref, second.checkpoint_ref);
    assert_ne!(second.checkpoint_ref, third.checkpoint_ref);
    let shared = first.manifest.components["conformance/session-delete-owned"]
        .blob_ref
        .clone();
    assert_eq!(
        third
            .manifest
            .component_ref("conformance/session-delete-owned"),
        Some(&shared),
        "{backend}: the advanced head shares a component with both dead roots"
    );
    let leaf = component_leaf(&first.manifest, "conformance/session-delete-owned");
    for root in [&first.checkpoint_ref, &second.checkpoint_ref] {
        if let Some(exists) = projects(handles.probe.as_ref(), root, &leaf, &shared).await {
            assert!(
                exists,
                "{backend}: the fixture must contain a dead-root edge"
            );
        }
    }
    let report = handles
        .factory
        .delete_session(&first.request.session_id)
        .await
        .expect("delete a session whose head advanced twice");
    assert!(report.deleted_blob_count > 0);
    assert!(
        handles
            .factory
            .is_deleted(&first.request.session_id)
            .await
            .expect("read deletion"),
        "{backend}: deletion must reach Deleted"
    );
    assert_components_exist(
        backend,
        handles.probe.as_ref(),
        &first.component_refs,
        false,
    )
    .await;
    for root in [&first.checkpoint_ref, &second.checkpoint_ref] {
        if let Some(exists) = handles
            .probe
            .checkpoint_component_edge_exists(root, &leaf)
            .await
        {
            assert!(
                !exists,
                "{backend}: reclaim must sever the dead-root edge in its transaction"
            );
        }
    }
}

/// A surviving admission protects its checkpoint and components during another session's delete.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_delete_preserves_admission_base_blobs<F>(backend: &str, make: F)
where
    F: Fn() -> SessionDeleteBlobHandles,
{
    for advance_source in [false, true] {
        let handles = make();
        let source = committed_checkpoint(
            &handles.factory,
            &SessionId::from("delete-admission-source"),
        )
        .await;
        let fork_id = SessionId::from("delete-admission-survivor");
        handles
            .factory
            .fork_session(&crate::ForkSessionRequest {
                pending_observer_intents: Vec::new(),
                session_id: fork_id.clone(),
                source_session_id: source.request.session_id.clone(),
                head_revision: crate::conformance::helpers::revision_at(
                    handles.factory.as_ref(),
                    &source.request.session_id,
                    &source.leaf_node_id,
                )
                .await,
                relation: crate::SessionRelation::Root,
                config: crate::PersistedSessionConfig::from_policy(
                    &source.request.config.session_policy(),
                ),
                retention: crate::Retention::UntilGc,
            })
            .await
            .expect("fork the admission's base");
        let fork = handles
            .factory
            .live_view(&fork_id)
            .await
            .expect("open fork")
            .expect("live fork");
        let base_window = fork
            .load_session_window(crate::store::WindowSelector::Current)
            .await
            .expect("read base window")
            .expect("base exists");
        let base = crate::store::SessionHeadRef {
            generation: fork
                .store()
                .read_session_state_version(&fork_id)
                .await
                .expect("read generation"),
            revision: base_window.head_revision,
            leaf: base_window.window.leaf_node_id.clone(),
            checkpoint: base_window.checkpoint_ref,
        };
        fork.store()
            .retain_admission_base(&fork_id, &base)
            .await
            .expect("retain admission base");
        advance_checkpoint(fork.store(), &fork_id, false).await;
        if advance_source {
            advance_checkpoint(&source.store, &source.request.session_id, true).await;
        }
        handles
            .factory
            .delete_session(&source.request.session_id)
            .await
            .expect("delete the source while the fork retains an admission base");
        assert!(
            handles.probe.blob_exists(&source.checkpoint_ref).await,
            "{backend}: an admission base is a root even without a head or anchor"
        );
        let shared = &source.manifest.components["conformance/session-delete-owned"].blob_ref;
        assert!(
            handles.probe.blob_exists(shared).await,
            "{backend}: retain the base's exclusive component"
        );
        let leaf = component_leaf(&source.manifest, "conformance/session-delete-owned");
        if let Some(exists) = projects(
            handles.probe.as_ref(),
            &source.checkpoint_ref,
            &leaf,
            shared,
        )
        .await
        {
            assert!(
                exists,
                "{backend}: the admission root's projection must remain"
            );
        }
        let replay = fork
            .load_session_window(crate::store::WindowSelector::Admitted(base.clone()))
            .await
            .expect("hydrate the surviving admission base")
            .expect("admitted window");
        assert_eq!(replay.checkpoint_ref, Some(source.checkpoint_ref));
        assert_eq!(
            replay
                .checkpoint
                .expect("hydrated base")
                .component_ref("conformance/session-delete-owned"),
            Some(shared)
        );
        handles
            .factory
            .delete_session(&fork_id)
            .await
            .expect("delete the final admission owner");
        assert!(
            handles
                .factory
                .is_deleted(&fork_id)
                .await
                .expect("read final deletion")
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
    attachment_prefix_retention(backend, make()).await;
    session_delete_reclaims_exclusive_checkpoint_blobs(backend, make()).await;
    session_delete_keeps_fork_shared_checkpoint_blobs(backend, make()).await;
    session_delete_reclaims_content_aliased_checkpoint_roots(backend, make()).await;
    session_delete_blob_failure_rolls_back_with_partial_report(backend, make()).await;
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_delete_reclaims_content_aliased_checkpoint_roots(
    backend: &str,
    handles: SessionDeleteBlobHandles,
) {
    const DEPENDENT_SESSION_ID: &str = "delete-content-alias-dependent";
    const ALIASED_SESSION_ID: &str = "delete-content-alias-root";
    let roots = commit_content_aliased_checkpoint_roots(
        &handles.factory,
        &SessionId::from(DEPENDENT_SESSION_ID),
        &SessionId::from(ALIASED_SESSION_ID),
    )
    .await;
    assert!(
        handles.probe.blob_exists(&roots.aliased_root).await,
        "{backend}: root B must exist as both a live root and root A's opaque component"
    );

    handles
        .factory
        .delete_session(&SessionId::from(ALIASED_SESSION_ID))
        .await
        .expect("delete root B's session while root A still aliases it");
    assert!(
        handles.probe.blob_exists(&roots.aliased_root).await,
        "{backend}: deleting B's head must retain B while live root A aliases its bytes"
    );
    // A node's references are its bytes': while A aliases B's bytes, B's
    // root still names its component, which stays.
    assert!(
        handles.probe.blob_exists(&roots.aliased_component).await,
        "{backend}: what B's retained root references must stay while A aliases it"
    );
    if let Some(projects) = projects(
        handles.probe.as_ref(),
        &roots.aliased_root,
        &roots.aliased_leaf,
        &roots.aliased_component,
    )
    .await
    {
        assert!(
            projects,
            "{backend}: B's retained root keeps its edges to what it references"
        );
    }

    handles
        .factory
        .delete_session(&SessionId::from(DEPENDENT_SESSION_ID))
        .await
        .expect("delete root A's session and its content-aliased component");
    assert!(
        !handles.probe.blob_exists(&roots.aliased_root).await,
        "{backend}: deleting root A must reclaim the now-unowned aliased root B"
    );
    assert!(
        !handles.probe.blob_exists(&roots.aliased_component).await,
        "{backend}: deleting root A must reclaim what only B's root referenced"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_delete_reclaims_exclusive_checkpoint_blobs(
    backend: &str,
    handles: SessionDeleteBlobHandles,
) {
    let committed =
        committed_checkpoint(&handles.factory, &SessionId::from("delete-exclusive-blobs")).await;
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_delete_keeps_fork_shared_checkpoint_blobs(
    backend: &str,
    handles: SessionDeleteBlobHandles,
) {
    let committed =
        committed_checkpoint(&handles.factory, &SessionId::from("delete-shared-source")).await;
    let head_revision = crate::conformance::helpers::revision_at(
        handles.factory.as_ref(),
        &committed.request.session_id,
        &committed.leaf_node_id,
    )
    .await;
    let fork_request = crate::ForkSessionRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("delete-shared-fork"),
        source_session_id: committed.request.session_id.clone(),
        head_revision,
        relation: crate::SessionRelation::Root,
        config: crate::PersistedSessionConfig::from_policy(
            &committed.request.config.session_policy(),
        ),
        retention: crate::Retention::UntilGc,
    };
    handles
        .factory
        .fork_session(&fork_request)
        .await
        .expect("fork shared checkpoint");

    // Config is owned by each session; the checkpoint closure is shared.
    // Observe those lifetimes directly instead of assuming every deletion
    // candidate belongs to the shared checkpoint closure.
    let config_ref = |head: crate::store::SessionHeadMeta| {
        crate::BlobRef::for_content(&serde_json::to_vec(&head.payload()).expect("config payload"))
    };
    let source_config = config_ref(
        committed
            .store
            .load_session_head_meta(&committed.request.session_id)
            .await
            .expect("source head")
            .expect("source config"),
    );
    let fork_config = config_ref(
        committed
            .store
            .load_session_head_meta(&fork_request.session_id)
            .await
            .expect("fork head")
            .expect("fork config"),
    );
    assert!(handles.probe.blob_exists(&source_config).await);
    assert!(handles.probe.blob_exists(&fork_config).await);

    let source_report = handles
        .factory
        .delete_session(&committed.request.session_id)
        .await
        .expect("delete source with surviving fork");
    assert!(
        !handles.probe.blob_exists(&source_config).await,
        "{backend}: the deleted session releases its config"
    );
    assert!(
        handles.probe.blob_exists(&fork_config).await,
        "{backend}: the fork owns its config"
    );
    assert_eq!(
        source_report.retained_blob_count + source_report.deleted_blob_count,
        source_report.enumerated_blob_count,
        "{backend}: every candidate has a reclaim decision"
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
    assert!(
        !handles.probe.blob_exists(&fork_config).await,
        "{backend}: the final owner releases its config"
    );
    assert_components_exist(
        backend,
        handles.probe.as_ref(),
        &committed.component_refs,
        false,
    )
    .await;
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_delete_blob_failure_rolls_back_with_partial_report(
    backend: &str,
    handles: SessionDeleteBlobHandles,
) {
    let committed =
        committed_checkpoint(&handles.factory, &SessionId::from("delete-blob-failure")).await;
    handles.probe.fail_next_blob_delete().await;
    let failure = handles
        .factory
        .delete_session(&committed.request.session_id)
        .await
        .expect_err("injected blob failure must fail session delete");
    handles.probe.clear_blob_delete_failure().await;
    assert!(matches!(failure.stop, crate::MaintenanceStop::Failed(_)));
    assert_eq!(
        failure.partial.deleted_blob_count, 0,
        "{backend}: rolled-back deletes must not be reported as durable work"
    );
    assert!(
        handles
            .factory
            .live_view_for(&committed.request)
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

/// FIG-2501: fork and pin roots protect attachment bytes after owner deletion.
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn attachment_prefix_retention(backend_name: &str, handles: SessionDeleteBlobHandles) {
    let request = session_store_request(
        &SessionId::from("attachment-prefix-parent"),
        "session-delete-blob-reclaim-model",
        crate::SessionRelation::Root,
    );
    let store = handles.factory.admit_view(&request).await.unwrap();
    let bytes = Arc::clone(&handles.attachments);
    let parent = crate::RuntimeAttachmentStore::new(
        bytes.clone(),
        Arc::clone(store.store()) as Arc<dyn crate::AttachmentReferrers>,
        crate::RuntimeOwner::Session((request.session_id).clone()),
        crate::AttachmentPolicy::standard(),
    );
    let reference = parent
        .put(
            vec![1, 2, 3],
            crate::AttachmentCreateMeta::new(
                crate::MediaType::parse("image/png").unwrap(),
                None,
                None,
            ),
        )
        .await
        .expect("put shared-prefix attachment");
    let staging = store
        .store()
        .attachment_referrers(&reference.id)
        .await
        .unwrap();
    // A settled execution has an explicit cleanup boundary.
    let orphan_referrer = crate::conformance::attachment_referrers::execution(
        &request.session_id,
        "completed-fixture",
    );
    let orphan = bytes
        .put(
            vec![4, 5, 6],
            crate::AttachmentCreateMeta::new(
                crate::MediaType::parse("image/png").unwrap(),
                None,
                None,
            ),
        )
        .await
        .unwrap();
    crate::conformance::helpers::record_completed_attachment_write(
        store.store(),
        crate::AttachmentWrite {
            attachment_id: orphan.id.clone(),
            claim: crate::conformance::attachment_referrers::claim(
                crate::conformance::attachment_referrers::execution(
                    &(request.session_id.clone()),
                    "completed-fixture",
                ),
            ),
        },
    )
    .await;
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::fixture(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    state.session_graph.append_message(crate::Message {
        id: "shared-image".into(),
        role: crate::MessageRole::User,
        origin: None,
        parts: Arc::new(vec![crate::Part::attachment_part(
            "shared-image-part".into(),
            String::new(),
            Some(lash_sansio::PartAttachment {
                reference: reference.clone(),
            }),
        )]),
        reply_marker: None,
    });
    let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
    commit.committed_attachment_ids = vec![reference.id.clone()];
    let receipt = store.commit_runtime_state(commit).await.unwrap();
    for referrer in staging {
        store
            .store()
            .end_attachment_referrer(&referrer)
            .await
            .unwrap();
    }
    store
        .store()
        .end_attachment_referrer(&orphan_referrer)
        .await
        .unwrap();
    let fork_request = crate::ForkSessionRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("attachment-prefix-child"),
        source_session_id: request.session_id.clone(),
        head_revision: receipt.head_revision,
        relation: crate::SessionRelation::Root,
        config: crate::PersistedSessionConfig::from_policy(&request.config.session_policy()),
        retention: crate::Retention::UntilGc,
    };
    handles
        .factory
        .fork_session(&fork_request)
        .await
        .expect("fork prefix");
    let fork = handles
        .factory
        .live_view_for(&session_store_request(
            &fork_request.session_id,
            "session-delete-blob-reclaim-model",
            crate::SessionRelation::Root,
        ))
        .await
        .unwrap()
        .unwrap();
    let inherited = fork
        .load_session_window(crate::store::WindowSelector::Current)
        .await
        .unwrap()
        .unwrap();
    assert!(
        inherited
            .window
            .nodes
            .iter()
            .any(|node| serde_json::to_string(node)
                .unwrap()
                .contains(reference.id.as_str())),
        "fork history retains the stored image reference"
    );
    let child = crate::RuntimeAttachmentStore::new(
        bytes.clone(),
        Arc::clone(fork.store()) as Arc<dyn crate::AttachmentReferrers>,
        crate::RuntimeOwner::Session((fork_request.session_id).clone()),
        crate::AttachmentPolicy::standard(),
    );
    assert_eq!(
        child
            .read(&reference)
            .await
            .expect("fork reads shared-prefix attachment"),
        vec![1, 2, 3]
    );
    handles
        .factory
        .delete_session(&request.session_id)
        .await
        .unwrap();
    let retained = handles
        .factory
        .reclaim_retained_evidence(crate::RetentionBound {
            committed_before_epoch_ms: u64::MAX,
            turn_watermark: lash_core::store::TurnProjectionWatermark::NoProjector,
        })
        .await
        .unwrap();
    assert_eq!(
        retained.removed_receipt_count, 1,
        "terminal parent receipt is pruned while the fork retains its image"
    );
    let policy =
        crate::AttachmentReclamationPolicy::new(0, crate::EmptyRootSetPolicy::AuthorizeDeleteAll);
    let reconciled =
        crate::reclaim_unreferenced_attachments(handles.factory.as_ref(), bytes.as_ref(), policy)
            .await
            .unwrap();
    assert_eq!(
        reconciled.reclaimed_count, 1,
        "receipt pruning cannot leak the settled execution's bytes"
    );
    assert!(matches!(
        bytes.get(&orphan.id, 32 * 1024 * 1024).await,
        Err(crate::AttachmentStoreError::NotFound(_))
    ));
    assert_eq!(
        child
            .read(&reference)
            .await
            .expect("surviving fork retains attachment after parent deletion"),
        vec![1, 2, 3],
        "{backend_name}"
    );
    handles
        .factory
        .delete_session(&fork_request.session_id)
        .await
        .unwrap();
    assert_eq!(
        store
            .store()
            .session_referrer_state(&request.session_id)
            .await
            .unwrap(),
        crate::SessionReferrerState::DeletedRetired
    );
    store
        .store()
        .end_attachment_referrer(&crate::ArtifactReferrer::Session(
            request.session_id.clone(),
        ))
        .await
        .unwrap();
    let report =
        crate::reclaim_unreferenced_attachments(handles.factory.as_ref(), bytes.as_ref(), policy)
            .await
            .unwrap();
    assert_eq!(
        report.reclaimed_count, 1,
        "{backend_name}: last reader gone collects orphan"
    );
    assert!(matches!(
        bytes.get(&reference.id, 32 * 1024 * 1024).await,
        Err(crate::AttachmentStoreError::NotFound(_))
    ));
}
