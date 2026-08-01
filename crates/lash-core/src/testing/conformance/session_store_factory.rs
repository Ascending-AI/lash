//! [`SessionStoreFactory`](crate::SessionStoreFactory) conformance: create,
//! reopen, delete, and session metadata.

use super::*;

/// Run the [`SessionStoreFactory`](crate::SessionStoreFactory) conformance
/// suite against the backend produced by `make`. `make` must return a fresh,
/// empty factory on each call.
pub async fn session_store_factory<F, S>(make: F, make_unbound_store: S)
where
    F: Fn() -> Arc<dyn crate::SessionStoreFactory>,
    S: Fn() -> Arc<dyn crate::RuntimePersistence>,
{
    session_admission_contract(make(), make_unbound_store()).await;
    session_store_factory_open_missing_returns_none(make()).await;
    session_store_factory_create_seeds_and_reopens_meta(make()).await;
    session_store_factory_create_is_idempotent(make()).await;
    session_store_factory_never_used_delete_is_noop(make()).await;
    session_store_factory_rejects_writes_after_delete(make()).await;
    attachment_ownership_isolation(make()).await;
    session_store_factory_rejects_cross_session_graph_parents(make()).await;
    session_store_factory_fork_semantics(make()).await;
    session_store_factory_vacuums_organic_retained_tombstone(make()).await;
    session_store_factory_delete_removes_store_and_is_idempotent(make()).await;
    session_store_factory_delete_fences_stale_handles(make()).await;
}

/// Deleting a session must erase readable state and fence handles that were
/// opened before the delete.
///
/// A store handle held by an in-flight turn outlives `delete_session`. If that
/// stale handle can still read retained state or write, backend behavior
/// depends on object lifetime and the delete can be undone after the host
/// retired the id.
pub async fn session_store_factory_delete_fences_stale_handles(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "delete-fence-stale-handle",
        "delete-fence-model",
        crate::SessionRelation::Root,
    );
    let stale = factory
        .create_store(&request)
        .await
        .expect("create the handle that will go stale");
    let stale_meta = stale
        .load_session_meta()
        .await
        .expect("load stale handle metadata")
        .expect("stale handle metadata");
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..Default::default()
    };
    state.ensure_agent_frame_initialized();
    stale
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("seed a checkpoint on the handle that will go stale");
    stale
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                &request.session_id,
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("pending input on the handle that will go stale"),
            )
            .with_source_key("delete-fence-stale-handle:pending-input"),
        )
        .await
        .expect("seed a pending turn input on the handle that will go stale");
    stale
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            &request.session_id,
            crate::DeliveryPolicy::EarliestSafeBoundary,
            crate::SlotPolicy::Exclusive,
            vec![crate::QueuedWorkPayload::session_command(
                crate::SessionCommand::RefreshToolCatalog {
                    reason: "queued work on the handle that will go stale".to_string(),
                },
            )],
        ))
        .await
        .expect("seed queued work on the handle that will go stale");
    let has_session_artifact_refs = stale
        .seed_session_trigger_manifest_ref_for_testing(&request.session_id)
        .await
        .expect("seed the session trigger-manifest ref through the stale handle");
    if has_session_artifact_refs {
        let refs = stale
            .raw_session_owned_artifact_refs_for_testing(&request.session_id)
            .await
            .expect("read seeded session artifact refs through the stale handle");
        assert_eq!(
            refs.len(),
            1,
            "the deletion fixture must exercise the exact session-owned artifact-ref namespace"
        );
        assert_eq!(
            refs[0].1,
            crate::TriggerOwnerScope::session(&request.session_id).namespace(),
            "the deletion fixture must exercise the exact session owner ref"
        );
    }

    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete the session out from under the stale handle");

    assert!(
        stale
            .load_session_meta()
            .await
            .expect("read metadata through the stale handle")
            .is_none(),
        "a stale handle must observe deleted session metadata as absent"
    );
    assert!(
        stale
            .load_session()
            .await
            .expect("load checkpoint through the stale handle")
            .is_none(),
        "a stale handle must observe the deleted session checkpoint as absent"
    );
    assert!(
        stale
            .list_pending_turn_inputs(&request.session_id)
            .await
            .expect("list pending inputs through the stale handle")
            .is_empty(),
        "a stale handle must observe deleted pending turn inputs as absent"
    );
    assert!(
        stale
            .list_queued_work(&request.session_id)
            .await
            .expect("list queued work through the stale handle")
            .is_empty(),
        "a stale handle must observe deleted queued work as absent"
    );
    assert!(
        stale
            .raw_session_owned_artifact_refs_for_testing(&request.session_id)
            .await
            .expect("read artifact refs through the stale handle")
            .is_empty(),
        "a stale handle must observe deleted session-owned artifact refs as absent"
    );

    let ensure_error = stale
        .admit_and_bind_session(&crate::SessionBinding::from_create_request(&request))
        .await
        .expect_err("a stale handle must not reinsert deleted session metadata");
    assert!(
        matches!(
            ensure_error,
            crate::StoreError::SessionDeleted { ref session_id }
                if session_id == &request.session_id
        ),
        "stale session binding must be fenced as deleted, got: {ensure_error}"
    );
    let save_error = stale
        .save_session_meta(stale_meta)
        .await
        .expect_err("a stale handle must not restore deleted session metadata");
    assert!(
        matches!(
            save_error,
            crate::StoreError::SessionDeleted { ref session_id }
                if session_id == &request.session_id
        ),
        "stale metadata writes must be fenced as deleted, got: {save_error}"
    );

    let error = stale
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect_err("a stale handle must not resurrect a deleted session");
    assert!(
        matches!(
            error,
            crate::StoreError::SessionDeleted { ref session_id }
                if session_id == &request.session_id
        ),
        "a stale commit into a deleted session must be fenced as deleted, got: {error}"
    );
    assert!(
        factory
            .open_existing_store(&request)
            .await
            .expect("open after the fenced commit")
            .is_none(),
        "a fenced commit must leave the session deleted"
    );

    let recreate_error = match factory.create_store(&request).await {
        Ok(_) => panic!("explicit create must not lift a retired session id's fence"),
        Err(error) => error,
    };
    assert_session_id_was_used_and_deleted(recreate_error, &request.session_id);

    let stale_error = stale
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect_err("the pre-delete handle must remain fenced after refused recreation");
    assert!(
        matches!(
            stale_error,
            crate::StoreError::SessionDeleted { ref session_id }
                if session_id == &request.session_id
        ),
        "a pre-delete handle must remain fenced after refused recreation, got: {stale_error}"
    );
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
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            crate::ProjectionWatermark::NoProjector,
        )
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
        factory
            .create_store(&request)
            .await
            .expect("runtime-internal session id must be reclaimable without tombstone");
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

pub(crate) fn session_store_request(
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
            generation: crate::GenerationOptions::default(),
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

async fn session_admission_contract(
    factory: Arc<dyn crate::SessionStoreFactory>,
    unbound: Arc<dyn crate::RuntimePersistence>,
) {
    let empty = crate::SessionBinding::root("", &crate::SessionPolicy::default());
    assert!(matches!(
        unbound
            .admit_and_bind_session(&empty)
            .await
            .expect_err("empty session id must be rejected"),
        crate::StoreError::InvalidSessionId { .. }
    ));

    let relation = crate::SessionRelation::Child {
        parent_session_id: "admission-parent".to_string(),
        caused_by: None,
    };
    let request = session_store_request("admission-created", "admission-model", relation.clone());
    let binding = crate::SessionBinding {
        session_id: request.session_id.clone(),
        relation,
        model_id: request.policy.model.id.clone(),
        cwd: Some("/tmp/admission-cwd".to_string()),
    };
    assert_eq!(
        unbound
            .admit_and_bind_session(&binding)
            .await
            .expect("admit fresh session"),
        crate::SessionAdmission::Created
    );
    let created_meta = unbound
        .load_session_meta()
        .await
        .expect("load admitted metadata")
        .expect("admission must durably materialize metadata");
    assert_eq!(created_meta.session_id, binding.session_id);
    assert_eq!(created_meta.relation, binding.relation);
    assert_eq!(created_meta.model, binding.model_id);
    assert_eq!(created_meta.cwd, binding.cwd);

    let changed_binding = crate::SessionBinding {
        model_id: "must-not-replace-model".to_string(),
        relation: crate::SessionRelation::Root,
        cwd: None,
        ..binding.clone()
    };
    assert_eq!(
        unbound
            .admit_and_bind_session(&changed_binding)
            .await
            .expect("rebind same session"),
        crate::SessionAdmission::Rebound
    );
    assert_eq!(
        unbound
            .load_session_meta()
            .await
            .expect("reload rebound metadata")
            .expect("rebound metadata"),
        created_meta,
        "rebind must preserve the original durable binding"
    );

    let cross_session = crate::SessionBinding {
        session_id: "admission-other".to_string(),
        ..binding
    };
    assert!(matches!(
        unbound
            .admit_and_bind_session(&cross_session)
            .await
            .expect_err("cross-session handle reuse must fail"),
        crate::StoreError::SessionBindingMismatch { .. }
    ));

    let deleted_request = session_store_request(
        "admission-deleted",
        "admission-model",
        crate::SessionRelation::Root,
    );
    let deleted_store = factory
        .create_store(&deleted_request)
        .await
        .expect("create admission deletion fixture");
    factory
        .delete_session(&deleted_request.session_id)
        .await
        .expect("delete admission fixture");
    let deleted_binding = crate::SessionBinding::from_create_request(&deleted_request);
    assert_session_id_was_used_and_deleted(
        deleted_store
            .admit_and_bind_session(&deleted_binding)
            .await
            .expect_err("deleted id admission must fail"),
        &deleted_request.session_id,
    );
    deleted_store
        .vacuum()
        .await
        .expect("vacuum deleted admission fixture");
    assert_session_id_was_used_and_deleted(
        deleted_store
            .admit_and_bind_session(&deleted_binding)
            .await
            .expect_err("vacuum must preserve admission tombstone"),
        &deleted_request.session_id,
    );
}

async fn session_store_factory_never_used_delete_is_noop(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "never-used-delete",
        "never-used-model",
        crate::SessionRelation::Root,
    );
    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete never-used id is a no-op");
    factory
        .create_store(&request)
        .await
        .expect("never-used id remains admissible after no-op delete");
}

async fn session_store_factory_rejects_writes_after_delete(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "write-after-delete",
        "write-after-delete-model",
        crate::SessionRelation::Root,
    );
    let stale = factory
        .create_store(&request)
        .await
        .expect("create write-after-delete fixture");
    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete write-after-delete fixture");

    assert_deleted_write(
        stale
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                &request.session_id,
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("must not persist"),
            ))
            .await,
        &request.session_id,
        "pending turn input",
    );
    assert_deleted_write(
        stale
            .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
                &request.session_id,
                crate::DeliveryPolicy::EarliestSafeBoundary,
                crate::SlotPolicy::Exclusive,
                vec![crate::QueuedWorkPayload::session_command(
                    crate::SessionCommand::RefreshToolCatalog {
                        reason: "must not persist".to_string(),
                    },
                )],
            ))
            .await,
        &request.session_id,
        "queued work",
    );
    assert_deleted_write(
        crate::AttachmentManifest::record_intent(
            stale.as_ref(),
            crate::AttachmentIntent {
                attachment_id: crate::AttachmentId::new("write-after-delete-attachment"),
                session_id: request.session_id.clone(),
                canonical_uri: "lash-attachment://write-after-delete".to_string(),
                intent_at_epoch_ms: 1,
                owner_kind: None,
                owner_id: None,
            },
        ),
        &request.session_id,
        "attachment intent",
    );
    assert_deleted_write(
        stale
            .try_claim_session_execution_lease(
                &request.session_id,
                &crate::LeaseOwnerIdentity::opaque("deleted-owner", "deleted-incarnation"),
                60_000,
            )
            .await,
        &request.session_id,
        "session execution lease",
    );

    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..Default::default()
    };
    state.ensure_agent_frame_initialized();
    assert_deleted_write(
        stale
            .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
                &state,
                &[crate::TokenLedgerEntry {
                    source: "write-after-delete".to_string(),
                    model: "write-after-delete-model".to_string(),
                    usage: crate::TokenUsage {
                        input_tokens: 1,
                        ..Default::default()
                    },
                }],
            ))
            .await,
        &request.session_id,
        "usage-bearing runtime commit",
    );

    factory
        .delete_session(&request.session_id)
        .await
        .expect("re-delete cleans any historical post-delete orphans");
}

fn assert_deleted_write<T>(result: Result<T, crate::StoreError>, session_id: &str, surface: &str) {
    let error = match result {
        Ok(_) => panic!("{surface} write unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            crate::StoreError::SessionDeleted {
                session_id: ref deleted
            } if deleted == session_id
        ),
        "{surface} write must fail with SessionDeleted, got {error:?}"
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
    assert_eq!(
        pinned.config.provider_id,
        state.policy.recorded_provider_id()
    );
    assert_eq!(pinned.config.model, state.policy.model);
    assert!(pinned.pinned);

    append_conformance_event_node(&mut state, "source-child", "source child");
    commit_conformance_state(&source, &mut state)
        .await
        .expect("advance source past pinned root");
    let unpinned_past_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("source child leaf");
    append_conformance_event_node(&mut state, "source-tip", "source tip");
    commit_conformance_state(&source, &mut state)
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
    append_conformance_event_node(&mut branch_state, "branch-child", "branch child");
    commit_conformance_state(&branch, &mut branch_state)
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
    let orphaned_source_point = factory
        .fork_points()
        .await
        .expect("enumerate retained point after source deletion")
        .into_iter()
        .find(|point| point.node_id == root_node_id)
        .expect("pin must outlive its deleted source session");
    assert_eq!(
        orphaned_source_point.config.provider_id,
        state.policy.recorded_provider_id()
    );
    assert_eq!(orphaned_source_point.config.model, state.policy.model);
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

    let recreate_error = match factory.create_store(&source_request).await {
        Ok(_) => panic!("a deleted source session id must never be reused"),
        Err(error) => error,
    };
    assert_session_id_was_used_and_deleted(recreate_error, &source_request.session_id);

    let fork_reuse_error = factory
        .fork_at(&crate::ForkSessionRequest {
            session_id: source_request.session_id.clone(),
            node_id: root_node_id,
            relation: crate::SessionRelation::Root,
            policy: source_request.policy.clone(),
        })
        .await
        .expect_err("forking must reject a previously deleted target session id");
    assert_session_id_was_used_and_deleted(fork_reuse_error, &source_request.session_id);
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

    let recreate_error = match factory.create_store(&request).await {
        Ok(_) => panic!("a deleted session id must not be reusable"),
        Err(error) => error,
    };
    assert_session_id_was_used_and_deleted(recreate_error, &request.session_id);

    created
        .vacuum()
        .await
        .expect("vacuum after deletion must preserve the permanent id tombstone");
    let after_vacuum_error = match factory.create_store(&request).await {
        Ok(_) => panic!("vacuum must not make a deleted session id reusable"),
        Err(error) => error,
    };
    assert_session_id_was_used_and_deleted(after_vacuum_error, &request.session_id);
}

fn assert_session_id_was_used_and_deleted(error: crate::StoreError, session_id: &str) {
    assert!(
        matches!(
            &error,
            crate::StoreError::SessionDeleted {
                session_id: deleted
            } if deleted == session_id
        ),
        "reuse must fail with StoreError::SessionDeleted, got {error:?}"
    );
    assert!(
        error.to_string().contains("was used and deleted"),
        "reuse error must explain that the id was used and deleted: {error}"
    );
}
