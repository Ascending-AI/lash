//! [`SessionStoreFactory`](crate::SessionStoreFactory) conformance: create,
//! reopen, delete, session metadata, and declared durability.

use super::*;

/// Run the [`SessionStoreFactory`](crate::SessionStoreFactory) conformance
/// suite against the backend produced by `make`. `make` must return a fresh,
/// empty factory on each call.
pub async fn session_store_factory<F>(make: F, expected_tier: DurabilityTier)
where
    F: Fn() -> Arc<dyn crate::SessionStoreFactory>,
{
    session_store_factory_reports_declared_tier(make(), expected_tier);
    session_store_factory_open_missing_returns_none(make()).await;
    session_store_factory_create_seeds_and_reopens_meta(make(), expected_tier).await;
    session_store_factory_create_is_idempotent(make()).await;
    attachment_ownership_isolation(make()).await;
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

fn session_store_factory_reports_declared_tier(
    factory: Arc<dyn crate::SessionStoreFactory>,
    expected: DurabilityTier,
) {
    assert_eq!(
        factory.durability_tier(),
        expected,
        "factory durability tier must match the backend"
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
    expected_tier: DurabilityTier,
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
    assert_eq!(
        created.durability_tier(),
        expected_tier,
        "created store durability tier must match the factory"
    );
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
    created
        .save_session_meta(SessionMeta {
            session_id: "stable-session".to_string(),
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
}
