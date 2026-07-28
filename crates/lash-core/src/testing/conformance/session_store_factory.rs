//! [`SessionStoreFactory`](crate::SessionStoreFactory) conformance: create,
//! reopen, delete, and session metadata.

use super::*;

/// Run the [`SessionStoreFactory`](crate::SessionStoreFactory) conformance
/// suite against the backend produced by `make`. `make` must return a fresh,
/// empty factory on each call.
pub async fn session_store_factory<F>(make: F)
where
    F: Fn() -> Arc<dyn crate::SessionStoreFactory>,
{
    session_store_factory_open_missing_returns_none(make()).await;
    session_store_factory_create_seeds_and_reopens_meta(make()).await;
    session_store_factory_create_is_idempotent(make()).await;
    attachment_ownership_isolation(make()).await;
    session_store_factory_rejects_cross_session_graph_parents(make()).await;
    session_store_factory_fork_semantics(make()).await;
    session_store_factory_vacuums_organic_retained_tombstone(make()).await;
    session_store_factory_delete_removes_store_and_is_idempotent(make()).await;
}

/// Process-retention conformance: pruning a terminal process releases its
/// attachment intents and removes both durable session stores owned by the
/// process before the process row disappears.
pub async fn process_prune_deletes_owned_session_stores(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
) {
    const PROCESS_ID: &str = "process-prune-owned-session-stores";
    registry
        .register_process(crate::ProcessRegistration::new(
            PROCESS_ID,
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::RecoveryDisposition::ExternallyOwned,
            crate::ProcessProvenance::host(),
        ))
        .await
        .expect("register process with owned stores");

    let mut requests = Vec::new();
    for (index, session_id) in crate::process_runtime_session_ids(PROCESS_ID)
        .into_iter()
        .enumerate()
    {
        let request = crate::SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: crate::SessionRelation::default(),
            policy: crate::SessionPolicy::default(),
        };
        let store = factory
            .create_store(&request)
            .await
            .expect("create process-owned session store");
        crate::AttachmentManifest::record_intent(
            store.as_ref(),
            crate::AttachmentIntent {
                attachment_id: crate::AttachmentId::new(format!(
                    "process-owned-session-intent-{index}"
                )),
                session_id,
                canonical_uri: format!("lash-attachment://process-owned-{index}"),
                intent_at_epoch_ms: 1,
                owner_kind: Some(crate::AttachmentOwnerKind::Process),
                owner_id: Some(PROCESS_ID.to_string()),
            },
        )
        .expect("record process-owned attachment intent");
        requests.push(request);
    }

    let terminal = registry
        .complete_process(
            PROCESS_ID,
            crate::ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete process with owned stores");
    let report = registry
        .prune_terminal_processes(terminal.updated_at_ms.saturating_add(1), None, None)
        .await
        .expect("prune process with owned stores");
    assert_eq!(report.pruned_processes, 1);

    for request in requests {
        assert!(
            factory
                .open_existing_store(&request)
                .await
                .expect("probe pruned process-owned store")
                .is_none(),
            "process prune left session store {} behind",
            request.session_id
        );
    }
}

/// Exercise the shared-bytes attachment contract: identical bytes across
/// sessions dedup to one blob, the session-boundary guard keeps sessions from
/// resolving each other's blobs, and mark-and-sweep GC collects a blob only
/// once no session references it.
pub async fn attachment_ownership_isolation(factory: Arc<dyn crate::SessionStoreFactory>) {
    attachment_ownership_isolation_with_store(
        factory,
        Arc::new(crate::InMemoryAttachmentStore::new()),
    )
    .await;
}

/// Run [`attachment_ownership_isolation`] against a concrete flat byte backend,
/// combining manifest reference tracking with the shared physical layout.
pub async fn attachment_ownership_isolation_with_store(
    factory: Arc<dyn crate::SessionStoreFactory>,
    backend: Arc<dyn crate::AttachmentStore>,
) {
    let a_request = session_store_request(
        "attachment-owner-a",
        "attachment-model",
        crate::SessionRelation::Root,
    );
    let b_request = session_store_request(
        "attachment-owner-b",
        "attachment-model",
        crate::SessionRelation::Root,
    );
    let a_manifest = factory
        .create_store(&a_request)
        .await
        .expect("create attachment owner a");
    let b_manifest = factory
        .create_store(&b_request)
        .await
        .expect("create attachment owner b");
    let session_a = crate::SessionAttachmentStore::new(
        backend.clone(),
        Arc::new(crate::attachments::PersistenceManifestAdapter(
            a_manifest.clone(),
        )),
        a_request.session_id.clone(),
    );
    let session_b = crate::SessionAttachmentStore::new(
        backend.clone(),
        Arc::new(crate::attachments::PersistenceManifestAdapter(
            b_manifest.clone(),
        )),
        b_request.session_id.clone(),
    );
    let png = AttachmentCreateMeta::new(
        MediaType::parse("image/png").unwrap(),
        Some(AttachmentTypeMetadata::image(Some(10), Some(20))),
        Some("a.png".to_string()),
    );
    let jpeg = AttachmentCreateMeta::new(
        MediaType::parse("image/jpeg").unwrap(),
        Some(AttachmentTypeMetadata::image(Some(30), Some(40))),
        Some("b.jpg".to_string()),
    );

    // Session A writes and commits the bytes.
    let a_ref = session_a
        .put(vec![6, 2, 6, 4], png)
        .await
        .expect("put a attachment");
    a_manifest
        .commit_refs(&a_request.session_id, std::slice::from_ref(&a_ref.id))
        .expect("commit a's attachment ref");

    // Boundary guard: session B never referenced A's blob, so its facade get
    // must NotFound even though the backend physically holds the bytes.
    assert!(
        matches!(
            session_b.get(&a_ref.id).await,
            Err(AttachmentStoreError::NotFound(_))
        ),
        "session B must not resolve session A's committed blob"
    );
    backend
        .get(&a_ref.id)
        .await
        .expect("backend physically holds the shared blob");

    // Session B writes identical bytes: ONE physical blob, divergent reference
    // presentation. Commit B's ref too, so the multi-session GC narrative below
    // rests on stable committed roots rather than the age-only fallback used by
    // these deliberately ownerless facade puts.
    let b_ref = session_b
        .put(vec![6, 2, 6, 4], jpeg)
        .await
        .expect("put identical bytes for b");
    assert_eq!(a_ref.id, b_ref.id, "identical bytes share one content id");
    assert_eq!(a_ref.media_type.as_str(), "image/png");
    assert_eq!(b_ref.media_type.as_str(), "image/jpeg");
    assert_eq!(a_ref.label.as_deref(), Some("a.png"));
    assert_eq!(b_ref.label.as_deref(), Some("b.jpg"));
    session_b
        .get(&b_ref.id)
        .await
        .expect("b resolves the blob it now references");
    b_manifest
        .commit_refs(&b_request.session_id, std::slice::from_ref(&b_ref.id))
        .expect("commit b's attachment ref");

    // Sweep: A's and B's committed refs both count as live roots, so the shared
    // blob survives.
    let report = crate::reclaim_unreferenced_attachments(&*factory, &*backend, 0)
        .await
        .expect("sweep with two live refs");
    assert_eq!(
        report.reclaimed_count, 0,
        "a blob referenced by any session is never swept, got {report:?}"
    );
    backend
        .get(&a_ref.id)
        .await
        .expect("blob survives while referenced");

    // Session A releases its ref: B's ref still holds the blob.
    session_a.delete(&a_ref.id).await.expect("a releases ref");
    let report = crate::reclaim_unreferenced_attachments(&*factory, &*backend, 0)
        .await
        .expect("sweep with one remaining ref");
    assert_eq!(report.reclaimed_count, 0, "b still references the blob");

    // Both sessions release: now unreferenced, GC collects the single blob.
    session_b.delete(&b_ref.id).await.expect("b releases ref");
    let report = crate::reclaim_unreferenced_attachments(&*factory, &*backend, 0)
        .await
        .expect("sweep with no refs");
    assert_eq!(report.reclaimed_count, 1, "unreferenced blob is reclaimed");
    assert!(
        matches!(
            backend.get(&a_ref.id).await,
            Err(AttachmentStoreError::NotFound(_))
        ),
        "reclaimed blob bytes are gone"
    );
}

fn session_store_request(
    session_id: &str,
    model_id: &str,
    relation: crate::SessionRelation,
) -> crate::SessionStoreCreateRequest {
    crate::SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation,
        policy: crate::SessionPolicy {
            model: crate::ModelSpec::from_token_limits(model_id, Default::default(), 200_000, None)
                .expect("valid conformance model"),
            provider_id: "conformance-provider".to_string(),
            session_id: Some(session_id.to_string()),
            autonomous: false,
            max_turns: None,
            prompt: crate::PromptLayer::new(),
        },
    }
}

fn assert_meta_matches_request(
    meta: &SessionMeta,
    request: &crate::SessionStoreCreateRequest,
    expected_model: &str,
) {
    assert_eq!(meta.session_id, request.session_id);
    assert_eq!(meta.session_name, request.session_id);
    assert_eq!(meta.model, expected_model);
    assert_eq!(meta.relation, request.relation);
    assert!(
        !meta.created_at.is_empty(),
        "created session metadata must carry a timestamp"
    );
}

async fn session_store_factory_open_missing_returns_none(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "missing-session",
        "missing-model",
        crate::SessionRelation::Root,
    );
    let opened = factory
        .open_existing_store(&request)
        .await
        .expect("open missing session");
    assert!(
        opened.is_none(),
        "open_existing_store must return None for unknown sessions"
    );
}

async fn session_store_factory_create_seeds_and_reopens_meta(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let relation = crate::SessionRelation::Child {
        parent_session_id: "parent-session".to_string(),
        caused_by: None,
    };
    let request = session_store_request("session-a", "model-a", relation);

    let created = factory
        .create_store(&request)
        .await
        .expect("create session store");
    let created_meta = created
        .load_session_meta()
        .await
        .expect("load created session meta")
        .expect("created session meta");
    assert_meta_matches_request(&created_meta, &request, "model-a");

    let reopened = factory
        .open_existing_store(&request)
        .await
        .expect("open existing session store")
        .expect("existing session store");
    let reopened_meta = reopened
        .load_session_meta()
        .await
        .expect("load reopened session meta")
        .expect("reopened session meta");
    assert_meta_matches_request(&reopened_meta, &request, "model-a");
}

async fn session_store_factory_create_is_idempotent(factory: Arc<dyn crate::SessionStoreFactory>) {
    let initial = session_store_request(
        "stable-session",
        "initial-model",
        crate::SessionRelation::Root,
    );
    let created = factory
        .create_store(&initial)
        .await
        .expect("create stable session");
    let incarnation_id = created
        .load_session_meta()
        .await
        .expect("load created metadata")
        .expect("created metadata exists")
        .incarnation_id;
    created
        .save_session_meta(SessionMeta {
            session_id: "stable-session".to_string(),
            incarnation_id,
            session_name: "custom-name".to_string(),
            created_at: "custom-created-at".to_string(),
            model: "custom-model".to_string(),
            cwd: Some("/tmp/conformance".to_string()),
            relation: crate::SessionRelation::Child {
                parent_session_id: "custom-parent".to_string(),
                caused_by: None,
            },
        })
        .await
        .expect("write custom meta");

    let changed = session_store_request(
        "stable-session",
        "changed-model",
        crate::SessionRelation::Root,
    );
    let recreated = factory
        .create_store(&changed)
        .await
        .expect("recreate stable session");
    let meta = recreated
        .load_session_meta()
        .await
        .expect("load recreated meta")
        .expect("recreated meta");
    assert_eq!(
        meta.session_name, "custom-name",
        "create_store must not overwrite existing session metadata"
    );
    assert_eq!(meta.model, "custom-model");
    assert_eq!(
        meta.parent_session_id(),
        Some("custom-parent"),
        "create_store must preserve the original relation"
    );
}

async fn session_store_factory_rejects_cross_session_graph_parents(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let first_request = session_store_request(
        "graph-parent-owner",
        "graph-parent-model",
        crate::SessionRelation::Root,
    );
    let second_request = session_store_request(
        "graph-parent-intruder",
        "graph-parent-model",
        crate::SessionRelation::Root,
    );
    let first = factory
        .create_store(&first_request)
        .await
        .expect("create graph parent owner");
    let second = factory
        .create_store(&second_request)
        .await
        .expect("create graph parent intruder");
    let mut first_state = crate::RuntimeSessionState {
        session_id: first_request.session_id.clone(),
        session_lifetime: crate::SessionLifetime::durable(
            first
                .load_session_meta()
                .await
                .expect("load graph parent owner metadata")
                .expect("graph parent owner metadata")
                .incarnation_id,
        ),
        ..Default::default()
    };
    first_state.ensure_agent_frame_initialized();
    first
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &first_state,
            &[],
        ))
        .await
        .expect("commit graph parent owner frame");
    let foreign_parent = first_state
        .current_frame_node_id
        .clone()
        .expect("owner frame node id");
    let mut second_state = crate::RuntimeSessionState {
        session_id: second_request.session_id.clone(),
        session_lifetime: crate::SessionLifetime::durable(
            second
                .load_session_meta()
                .await
                .expect("load graph parent intruder metadata")
                .expect("graph parent intruder metadata")
                .incarnation_id,
        ),
        ..Default::default()
    };
    second_state.ensure_agent_frame_initialized();
    let second_result = second
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &second_state,
            &[],
        ))
        .await
        .expect("commit intruder's own frame");
    second_state.apply_persisted_commit_result(second_result);
    assert!(
        second
            .load_node(&foreign_parent)
            .await
            .expect("probe unrelated history")
            .is_none(),
        "a bound store must not expose an unrelated session's node"
    );
    let child = crate::SessionNodeRecord {
        node_id: "cross-session-child".to_string(),
        parent_node_id: Some(foreign_parent.clone()),
        timestamp: "2026-07-27T00:00:00Z".to_string(),
        payload: crate::SessionNodePayload::Event {
            event: crate::SessionHistoryRecord::Protocol(
                crate::ProtocolEvent::typed("cross-session", serde_json::Value::Null)
                    .expect("protocol event"),
            ),
        },
    };
    let state = crate::RuntimeSessionState {
        head_revision: second_state.head_revision,
        persisted_node_ids: second_state.persisted_node_ids,
        session_id: second_state.session_id,
        session_lifetime: second_state.session_lifetime,
        current_frame_node_id: Some(foreign_parent),
        ..Default::default()
    };
    let commit = crate::RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend {
            nodes: vec![child],
            leaf_node_id: Some("cross-session-child".to_string()),
        },
        &[],
    );
    let child_node_id = commit.graph.nodes[0].node_id.clone();
    let error = second
        .commit_runtime_state(commit)
        .await
        .expect_err("a graph parent must belong to the committing session");
    assert!(match &error {
        crate::StoreError::InvalidGraphParent { node_id, .. } => node_id == &child_node_id,
        crate::StoreError::MissingFrameOpenAncestor { leaf_node_id } => {
            leaf_node_id == &child_node_id
        }
        _ => false,
    });
    let intruder_after_rejection = second
        .load_session()
        .await
        .expect("load intruder after rejection")
        .expect("intruder head survives rejection");
    assert_eq!(
        intruder_after_rejection.graph.nodes.len(),
        1,
        "cross-session parent rejection must be atomic"
    );
}

/// First-class fork contract shared by in-memory, SQLite, and PostgreSQL:
/// pins are roots, past unpinned checkpoints are normally unavailable,
/// forks write no graph nodes, and deleting either sibling cannot reclaim the
/// prefix still reachable from the other.
async fn session_store_factory_fork_semantics(factory: Arc<dyn crate::SessionStoreFactory>) {
    let source_request =
        session_store_request("fork-source", "fork-model", crate::SessionRelation::Root);
    let source = factory
        .create_store(&source_request)
        .await
        .expect("create fork source");
    let mut state = crate::RuntimeSessionState {
        session_id: source_request.session_id.clone(),
        session_lifetime: crate::SessionLifetime::durable(
            source
                .load_session_meta()
                .await
                .expect("load fork source metadata")
                .expect("fork source metadata")
                .incarnation_id,
        ),
        execution_state_snapshot: Some(vec![0xFA, 0xCE]),
        ..Default::default()
    };
    state.ensure_agent_frame_initialized();
    let root_ids = state
        .session_graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    let root_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("source root leaf");
    let first = source
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &state,
            &[crate::TokenLedgerEntry {
                source: "source-only".to_string(),
                model: "fork-model".to_string(),
                usage: crate::TokenUsage {
                    input_tokens: 7,
                    ..Default::default()
                },
            }],
        ))
        .await
        .expect("commit fork root");
    state.apply_persisted_commit_result(first);
    state.mark_node_ids_persisted(root_ids);

    let pinned = factory.pin(&root_node_id).await.expect("pin fork root");
    assert_eq!(pinned.node_id, root_node_id);
    assert_eq!(pinned.source_session_id, source_request.session_id);
    assert!(pinned.pinned);

    append_fork_conformance_message(&mut state, "reuse-proof-old", "old incarnation");
    let reuse_operation =
        crate::OperationId::turn(&source_request.session_id, "reuse-proof", "conformance");
    let (old_commit, old_node_ids) = crate::RuntimeCommit::persisted_state_with_operation(
        &mut state,
        &[],
        reuse_operation.clone(),
    )
    .expect("derive old-incarnation node ids");
    let old_ordinary_node_id = old_node_ids
        .last()
        .cloned()
        .expect("old incarnation ordinary node id");
    let old_result = source
        .commit_runtime_state(old_commit)
        .await
        .expect("commit old-incarnation ordinary node");
    state.apply_persisted_commit_result(old_result);
    state.mark_node_ids_persisted(old_node_ids);

    append_fork_conformance_message(&mut state, "source-child", "source child");
    commit_fork_conformance_state(&source, &mut state)
        .await
        .expect("advance source past pinned root");
    let unpinned_past_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("source child leaf");
    append_fork_conformance_message(&mut state, "source-tip", "source tip");
    commit_fork_conformance_state(&source, &mut state)
        .await
        .expect("advance source past unpinned child");
    let source_tip_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("source tip leaf");
    let (first_pin, second_pin) = tokio::join!(
        factory.pin(&source_tip_node_id),
        factory.pin(&source_tip_node_id)
    );
    first_pin.expect("first concurrent pin succeeds");
    second_pin.expect("second concurrent pin is idempotent");
    assert_eq!(
        factory
            .fork_points()
            .await
            .expect("enumerate concurrent pin")
            .iter()
            .filter(|point| point.node_id == source_tip_node_id)
            .count(),
        1,
        "fork-point enumeration deduplicates shared node ids"
    );
    factory
        .unpin(&source_tip_node_id)
        .await
        .expect("remove concurrent pin");

    let delete_first_request = crate::ForkSessionRequest {
        session_id: "aaa-fork-delete-first".to_string(),
        node_id: source_tip_node_id.clone(),
        relation: crate::SessionRelation::Root,
        policy: source_request.policy.clone(),
    };
    factory
        .fork_at(&delete_first_request)
        .await
        .expect("fork live source tip");
    let deduplicated_tip = factory
        .fork_points()
        .await
        .expect("enumerate shared live tip")
        .into_iter()
        .find(|point| point.node_id == source_tip_node_id)
        .expect("shared live tip remains forkable");
    assert_eq!(
        deduplicated_tip.source_session_id, delete_first_request.session_id,
        "unpinned shared heads use a lexicographic source-session tie-break"
    );
    factory
        .delete_session(&delete_first_request.session_id)
        .await
        .expect("delete branch before source");
    assert!(
        source
            .load_node(&source_tip_node_id)
            .await
            .expect("load source tip after branch-first delete")
            .is_some(),
        "deleting a branch first must not reclaim its live source sibling"
    );

    let unretained_error = factory
        .fork_at(&crate::ForkSessionRequest {
            session_id: "fork-unretained".to_string(),
            node_id: unpinned_past_node_id.clone(),
            relation: crate::SessionRelation::Root,
            policy: source_request.policy.clone(),
        })
        .await
        .expect_err("unpinned past turn must not be forkable");
    assert!(matches!(
        unretained_error,
        crate::StoreError::ForkPointNotRetained { node_id }
            if node_id == unpinned_past_node_id
    ));

    let fork_request = crate::ForkSessionRequest {
        session_id: "fork-branch".to_string(),
        node_id: root_node_id.clone(),
        relation: crate::SessionRelation::Root,
        policy: source_request.policy.clone(),
    };
    let forked = factory
        .fork_at(&fork_request)
        .await
        .expect("fork pinned root");
    assert_eq!(forked.node_id, root_node_id);
    let branch = factory
        .open_existing_store(&crate::SessionStoreCreateRequest {
            session_id: fork_request.session_id.clone(),
            relation: fork_request.relation.clone(),
            policy: fork_request.policy.clone(),
        })
        .await
        .expect("open fork")
        .expect("fork exists");
    let branch_read = branch
        .load_session()
        .await
        .expect("load fork")
        .expect("fork head");
    assert_eq!(branch_read.head_revision, 0);
    assert_eq!(branch_read.graph.nodes.len(), 1, "fork writes zero nodes");
    assert_eq!(
        branch_read.graph.leaf_node_id.as_deref(),
        Some(root_node_id.as_str())
    );
    assert_eq!(
        branch_read
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.execution_state.as_deref()),
        Some(&[0xFA, 0xCE][..]),
        "fork inherits the retained continuation checkpoint"
    );
    assert!(
        branch_read.token_ledger.is_empty(),
        "usage is execution-scoped and must not cross a fork"
    );
    let mut branch_state = crate::store::load_persisted_session_state(branch.as_ref())
        .await
        .expect("load fork state")
        .expect("fork state exists");
    append_fork_conformance_message(&mut branch_state, "branch-child", "branch child");
    commit_fork_conformance_state(&branch, &mut branch_state)
        .await
        .expect("advance fork independently");
    let branch_leaf = branch_state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("branch leaf");
    assert_ne!(
        branch_leaf,
        state
            .session_graph
            .leaf_node_id
            .clone()
            .expect("source leaf"),
        "siblings must navigate independently"
    );

    // Composed rewind: the host pinned the target, forked there, then deletes
    // the superseded source. The fork remains a valid, independently writable
    // session and the shared prefix survives.
    factory
        .delete_session(&source_request.session_id)
        .await
        .expect("delete superseded source");
    assert!(
        branch
            .load_node(&root_node_id)
            .await
            .expect("load shared prefix after source delete")
            .is_some(),
        "deleting one branch must stop at the first still-referenced node"
    );
    factory
        .unpin(&root_node_id)
        .await
        .expect("release rewind pin");
    assert!(
        branch
            .load_node(&root_node_id)
            .await
            .expect("load prefix after unpin")
            .is_some(),
        "the live branch child edge retains the prefix after unpin"
    );

    let recreated = factory
        .create_store(&source_request)
        .await
        .expect("recreate deleted source session id");
    let recreated_meta = recreated
        .load_session_meta()
        .await
        .expect("load recreated source metadata")
        .expect("recreated source metadata exists");
    assert_ne!(
        Some(&recreated_meta.incarnation_id),
        state.session_lifetime.as_durable(),
        "delete-then-recreate must mint a new session incarnation"
    );
    let mut recreated_state = crate::RuntimeSessionState {
        session_id: source_request.session_id.clone(),
        session_lifetime: crate::SessionLifetime::durable(recreated_meta.incarnation_id),
        ..Default::default()
    };
    recreated_state.ensure_agent_frame_initialized();
    let recreated_frame_node_id = recreated_state
        .current_frame_node_id
        .clone()
        .expect("recreated frame node id");
    assert_ne!(
        recreated_frame_node_id, root_node_id,
        "recreated frame identity must not alias retained old history"
    );
    let recreated_frame_ids = recreated_state
        .session_graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    let recreated_frame_result = recreated
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &recreated_state,
            &[],
        ))
        .await
        .expect("commit recreated frame");
    recreated_state.apply_persisted_commit_result(recreated_frame_result);
    recreated_state.mark_node_ids_persisted(recreated_frame_ids);
    append_fork_conformance_message(&mut recreated_state, "reuse-proof-new", "new incarnation");
    let (recreated_commit, recreated_node_ids) =
        crate::RuntimeCommit::persisted_state_with_operation(
            &mut recreated_state,
            &[],
            reuse_operation,
        )
        .expect("derive new-incarnation node ids");
    let recreated_ordinary_node_id = recreated_node_ids
        .last()
        .cloned()
        .expect("new incarnation ordinary node id");
    assert_ne!(
        recreated_ordinary_node_id, old_ordinary_node_id,
        "ordinary history identity must include the session incarnation"
    );
    recreated
        .commit_runtime_state(recreated_commit)
        .await
        .expect("commit same operation identity in recreated incarnation");
}

fn append_fork_conformance_message(
    state: &mut crate::RuntimeSessionState,
    id: &str,
    content: &str,
) {
    let parent_node_id = state.session_graph.leaf_node_id.clone();
    let node = crate::SessionNodeRecord {
        node_id: id.to_string(),
        parent_node_id,
        timestamp: "2026-07-27T00:00:00Z".to_string(),
        payload: crate::SessionNodePayload::Event {
            event: crate::SessionHistoryRecord::Protocol(
                crate::ProtocolEvent::typed(
                    "fork-conformance",
                    serde_json::json!({ "content": content }),
                )
                .expect("fork conformance event"),
            ),
        },
    };
    state.session_graph.push_node_record(node);
    state.session_graph.set_leaf_node_id(Some(id.to_string()));
}

async fn commit_fork_conformance_state(
    store: &Arc<dyn crate::RuntimePersistence>,
    state: &mut crate::RuntimeSessionState,
) -> Result<(), crate::StoreError> {
    let operation = crate::OperationId::turn(
        &state.session_id,
        format!("fork-conformance-{}", state.head_revision),
        "commit",
    );
    let (commit, new_node_ids) =
        crate::RuntimeCommit::persisted_state_with_operation(state, &[], operation)?;
    let result = store.commit_runtime_state(commit).await?;
    state.apply_persisted_commit_result(result);
    state.mark_node_ids_persisted(new_node_ids);
    Ok(())
}

async fn session_store_factory_vacuums_organic_retained_tombstone(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "retained-tombstone-source",
        "tombstone-model",
        crate::SessionRelation::Root,
    );
    let source = factory
        .create_store(&request)
        .await
        .expect("create retained-tombstone source");
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        session_lifetime: crate::SessionLifetime::durable(
            source
                .load_session_meta()
                .await
                .expect("load retained-tombstone metadata")
                .expect("retained-tombstone metadata")
                .incarnation_id,
        ),
        ..Default::default()
    };
    state.ensure_agent_frame_initialized();
    let leaf_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("retained-tombstone leaf");
    source
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit retained-tombstone source");
    factory
        .pin(&leaf_node_id)
        .await
        .expect("pin retained-tombstone leaf");
    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete retained-tombstone source");
    factory
        .unpin(&leaf_node_id)
        .await
        .expect("unpin deleted source leaf to zero");

    assert!(
        source
            .load_node(&leaf_node_id)
            .await
            .expect("read retained tombstone")
            .is_none(),
        "decrement-to-zero tombstones must be hidden before vacuum"
    );
    let fork_error = factory
        .fork_at(&crate::ForkSessionRequest {
            session_id: "retained-tombstone-fork".to_string(),
            node_id: leaf_node_id.clone(),
            relation: crate::SessionRelation::Root,
            policy: request.policy,
        })
        .await
        .expect_err("a retained tombstone must not be forkable");
    assert!(matches!(
        fork_error,
        crate::StoreError::ForkPointNotRetained { node_id } if node_id == leaf_node_id
    ));

    let report = source.vacuum().await.expect("vacuum retained tombstone");
    assert_eq!(
        report.removed_node_count, 1,
        "vacuum must physically remove the organically created tombstone"
    );
    assert_eq!(
        source
            .vacuum()
            .await
            .expect("repeat retained-tombstone vacuum")
            .removed_node_count,
        0,
        "vacuum must consume each retained tombstone exactly once"
    );
}

async fn session_store_factory_delete_removes_store_and_is_idempotent(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "delete-session",
        "delete-model",
        crate::SessionRelation::Root,
    );
    let created = factory
        .create_store(&request)
        .await
        .expect("create deleted session");
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        session_lifetime: crate::SessionLifetime::durable(
            created
                .load_session_meta()
                .await
                .expect("load deleted session metadata")
                .expect("deleted session metadata")
                .incarnation_id,
        ),
        ..Default::default()
    };
    state.ensure_agent_frame_initialized();
    let frame = state
        .session_graph
        .nodes
        .first()
        .cloned()
        .expect("initial frame node");
    let frame_node_id = frame.node_id.clone();
    let child_node = |node_id: &str| crate::SessionNodeRecord {
        node_id: node_id.to_string(),
        parent_node_id: Some(frame_node_id.clone()),
        timestamp: "2026-07-27T00:00:00Z".to_string(),
        payload: crate::SessionNodePayload::Event {
            event: crate::SessionHistoryRecord::Protocol(
                crate::ProtocolEvent::typed(node_id, serde_json::Value::Null)
                    .expect("protocol event"),
            ),
        },
    };
    let live_leaf = child_node("delete-live-leaf");
    state.session_graph = crate::SessionGraph::from_nodes(
        vec![frame, live_leaf.clone()],
        Some(live_leaf.node_id.clone()),
    );
    created
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit graph chain before delete");
    created
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                &request.session_id,
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("pending input before delete"),
            )
            .with_source_key("delete-session:pending-input"),
        )
        .await
        .expect("enqueue pending turn input before delete");
    assert_eq!(
        created
            .list_pending_turn_inputs(&request.session_id)
            .await
            .expect("list pending input before delete")
            .len(),
        1
    );
    let initial_lease = created
        .try_claim_session_execution_lease(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque("delete-session-owner", "before-delete"),
            60_000,
        )
        .await
        .expect("claim session execution lease before delete")
        .acquired()
        .expect("session execution lease before delete must be acquired");
    assert_eq!(
        initial_lease.fencing_token, 1,
        "newly created session should start with the first execution lease fence"
    );
    assert!(
        factory
            .open_existing_store(&request)
            .await
            .expect("open before delete")
            .is_some(),
        "session must exist before delete"
    );

    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete session");
    for node_id in [&frame_node_id, &live_leaf.node_id] {
        assert!(
            created
                .load_node(node_id)
                .await
                .expect("load reclaimed node through stale handle")
                .is_none(),
            "delete_session must physically reclaim graph node {node_id}"
        );
    }
    assert!(
        factory
            .open_existing_store(&request)
            .await
            .expect("open after delete")
            .is_none(),
        "delete_session must remove the session store"
    );
    factory
        .delete_session(&request.session_id)
        .await
        .expect("second delete must be idempotent");

    let recreated_request = session_store_request(
        "delete-session",
        "recreated-model",
        crate::SessionRelation::Root,
    );
    let recreated = factory
        .create_store(&recreated_request)
        .await
        .expect("recreate deleted session");
    assert!(
        recreated
            .list_pending_turn_inputs(&recreated_request.session_id)
            .await
            .expect("list pending turn inputs after recreate")
            .is_empty(),
        "delete_session must remove pending turn-input evidence for the deleted session"
    );
    let recreated_lease = recreated
        .try_claim_session_execution_lease(
            &recreated_request.session_id,
            &crate::LeaseOwnerIdentity::opaque("delete-session-owner", "after-delete"),
            60_000,
        )
        .await
        .expect("claim session execution lease after recreate")
        .acquired()
        .expect("recreated session must not retain the deleted session's execution lease");
    assert_eq!(
        recreated_lease.fencing_token, 1,
        "delete_session must remove session execution lease state before recreation"
    );
    let meta = recreated
        .load_session_meta()
        .await
        .expect("load recreated session meta")
        .expect("recreated session meta");
    assert_meta_matches_request(&meta, &recreated_request, "recreated-model");
    let mut recreated_state = crate::RuntimeSessionState {
        session_id: recreated_request.session_id.clone(),
        session_lifetime: crate::SessionLifetime::durable(meta.incarnation_id),
        ..Default::default()
    };
    recreated_state.ensure_agent_frame_initialized();
    recreated
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &recreated_state,
            &[],
        ))
        .await
        .expect("recreated session must be able to reuse its deterministic initial frame id");
    let read = recreated
        .load_session()
        .await
        .expect("load recreated session")
        .expect("recreated session has a committed head");
    assert_eq!(read.graph.nodes.len(), 1);
    assert_eq!(
        read.graph.leaf_node_id,
        recreated_state.session_graph.leaf_node_id
    );
}
