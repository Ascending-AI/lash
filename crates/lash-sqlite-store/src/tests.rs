use super::*;
use std::sync::atomic::Ordering;

use lash_core::ProcessInput;
use lashlang::LashlangArtifactStore;

static CHECKPOINT_DATA_STATEMENT_COUNT: AtomicUsize = AtomicUsize::new(0);

fn count_checkpoint_data_statement(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event {
        let sql = sql.trim_start();
        if ["SELECT", "INSERT", "UPDATE", "DELETE", "WITH"]
            .iter()
            .any(|prefix| sql.starts_with(prefix))
        {
            CHECKPOINT_DATA_STATEMENT_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn set_checkpoint_statement_trace(store: &Store, enabled: bool) {
    store
        .conn
        .call(move |conn| {
            conn.trace_v2(
                if enabled {
                    rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT
                } else {
                    rusqlite::trace::TraceEventCodes::empty()
                },
                enabled.then_some(
                    count_checkpoint_data_statement as fn(rusqlite::trace::TraceEvent<'_>),
                ),
            );
            Ok(())
        })
        .await
        .expect("configure SQLite checkpoint statement trace");
}

fn checkpoint_with_changed_components(depth: usize) -> HydratedSessionCheckpoint {
    HydratedSessionCheckpoint {
        components: (0..depth)
            .map(|index| {
                (
                    format!("arbitrary/depth-invariance/{index:05}"),
                    lash_core::HydratedCheckpointComponent::changed(
                        format!("depth-invariance-body-{index:05}").into_bytes(),
                    ),
                )
            })
            .collect(),
        ..Default::default()
    }
}

fn checkpoint_with_unchanged_components(manifest: &SessionCheckpoint) -> HydratedSessionCheckpoint {
    HydratedSessionCheckpoint {
        turn_state: manifest.turn_state.clone(),
        components: manifest
            .components
            .iter()
            .map(|(key, descriptor)| {
                (
                    key.clone(),
                    lash_core::HydratedCheckpointComponent::unchanged(descriptor),
                )
            })
            .collect(),
        plugin_snapshot_revision: manifest.plugin_snapshot_revision,
    }
}

async fn durable_state(store: &Store, session_id: &str) -> lash_core::RuntimeSessionState {
    let state = lash_core::RuntimeSessionState {
        session_id: session_id.to_string(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    store
        .admit_and_bind_session(&lash_core::SessionBinding::root(session_id))
        .await
        .expect("bind SQLite test session");
    state
}

#[tokio::test]
async fn checkpoint_probe_skips_writes_for_deferred_head() {
    let store = Arc::new(Store::memory().await.expect("open counter store"));
    lash_core::testing::conformance::checkpoint_claim_probe_transaction_counts(
        Arc::clone(&store) as Arc<dyn RuntimePersistence>,
        "sqlite-checkpoint-counter",
        || store.checkpoint_claim_counts(),
    )
    .await;
}

#[tokio::test]
async fn checkpoint_component_statement_count_is_depth_invariant() {
    let mut observed = Vec::new();
    for depth in [10, 100, 1_000, 4_000] {
        let store = Arc::new(Store::memory().await.expect("open depth-invariance store"));
        let mut state = durable_state(&store, &format!("sqlite-checkpoint-depth-{depth}")).await;
        let mut seed = RuntimeCommit::persisted_state_for_test(&state, &[]);
        seed.checkpoint = checkpoint_with_changed_components(depth);
        let seeded = store
            .commit_runtime_state(seed)
            .await
            .expect("seed checkpoint component bodies");
        state.head_revision = seeded.head_revision;
        let mut unchanged = RuntimeCommit::persisted_state_for_test(&state, &[]);
        unchanged.checkpoint = checkpoint_with_unchanged_components(&seeded.manifest);
        assert!(
            unchanged
                .checkpoint
                .components
                .values()
                .all(|component| component.body().is_none()),
            "measured commit must carry zero changed component bodies"
        );

        CHECKPOINT_DATA_STATEMENT_COUNT.store(0, Ordering::Relaxed);
        set_checkpoint_statement_trace(&store, true).await;
        let commit_started = std::time::Instant::now();
        store
            .commit_runtime_state(unchanged)
            .await
            .expect("commit unchanged checkpoint component refs");
        let commit_elapsed = commit_started.elapsed();
        set_checkpoint_statement_trace(&store, false).await;
        let commit_statements = CHECKPOINT_DATA_STATEMENT_COUNT.load(Ordering::Relaxed);

        CHECKPOINT_DATA_STATEMENT_COUNT.store(0, Ordering::Relaxed);
        set_checkpoint_statement_trace(&store, true).await;
        let load_started = std::time::Instant::now();
        let loaded = store
            .load_session()
            .await
            .expect("load checkpoint component bodies")
            .expect("stored checkpoint session");
        let load_elapsed = load_started.elapsed();
        set_checkpoint_statement_trace(&store, false).await;
        let load_statements = CHECKPOINT_DATA_STATEMENT_COUNT.load(Ordering::Relaxed);

        assert_eq!(
            loaded
                .checkpoint
                .expect("loaded checkpoint")
                .components
                .len(),
            depth
        );
        observed.push((depth, commit_statements, load_statements));
        eprintln!(
            "sqlite checkpoint depth={depth} commit_statements={commit_statements} load_statements={load_statements} commit_ms={:.3} load_ms={:.3}",
            commit_elapsed.as_secs_f64() * 1_000.0,
            load_elapsed.as_secs_f64() * 1_000.0,
        );
    }
    assert!(
        observed
            .iter()
            .all(|(_, commit, load)| { *commit == observed[0].1 && *load == observed[0].2 }),
        "checkpoint commit/load statement counts must be independent of component depth: {observed:?}"
    );
}

fn registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryDisposition::ExternallyOwned,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new("session")),
    )
}

#[tokio::test]
async fn real_locked_catalog_surfaces_typed_contention() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("contended.db");
    let store = Store::open(&path).await.expect("open store");
    store.bind_session("contended").expect("bind store");
    let state = durable_state(&store, "contended").await;
    store
        .conn
        .call(|conn| {
            conn.busy_timeout(std::time::Duration::ZERO)?;
            Ok(())
        })
        .await
        .expect("disable busy wait");

    let locker = rusqlite::Connection::open(&path).expect("open lock holder");
    locker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold catalog writer lock");
    let result = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await;
    locker
        .execute_batch("ROLLBACK")
        .expect("release writer lock");

    assert!(matches!(result, Err(StoreError::Contended)));
}

#[tokio::test]
async fn live_attachment_refs_reads_the_factory_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("sessions");
    std::fs::create_dir_all(&root).expect("mkdir sessions");
    let factory = SqliteSessionStoreFactory::new(&root);

    let catalog = factory.catalog_path();
    let attachment_id = lash_core::AttachmentId::new("a".repeat(64));
    {
        let store = Store::open(&catalog).await.expect("open catalog");
        lash_core::AttachmentManifest::record_intent(
            &store,
            lash_core::AttachmentIntent {
                attachment_id: attachment_id.clone(),
                session_id: "sess-1".to_string(),
                canonical_uri: format!("lash-attachment://sha256/{attachment_id}"),
                intent_at_epoch_ms: 1_000,
                owner_kind: None,
                owner_id: None,
            },
        )
        .expect("record intent");
        lash_core::AttachmentManifest::commit_refs(
            &store,
            "sess-1",
            std::slice::from_ref(&attachment_id),
        )
        .expect("commit ref");
    }

    let refs = lash_core::AttachmentRootSet::live_attachment_refs(&factory, 0)
        .await
        .expect("root discovery");
    assert!(
        refs.contains(&attachment_id),
        "the catalog's committed ref must be discovered"
    );
    assert_eq!(refs.len(), 1, "only the catalog contributes refs: {refs:?}");
}

#[tokio::test]
async fn live_attachment_refs_aborts_on_unreadable_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("sessions");
    std::fs::create_dir_all(&root).expect("mkdir sessions");
    let factory = SqliteSessionStoreFactory::new(&root);

    std::fs::write(factory.catalog_path(), b"corrupt not-a-db").expect("write corrupt");

    let result = lash_core::AttachmentRootSet::live_attachment_refs(&factory, 0).await;
    assert!(
        result.is_err(),
        "an unreadable durable-core catalog must abort discovery, got {result:?}"
    );
}

#[tokio::test]
async fn attachment_gc_aborts_when_a_missing_catalog_has_a_deletion_candidate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let live_root = dir.path().join("live-sessions");
    let live_factory = SqliteSessionStoreFactory::new(&live_root);
    let request = SessionStoreCreateRequest {
        session_id: "live-attachment".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let store = live_factory
        .create_store(&request)
        .await
        .expect("create live session store");
    let backend = lash_core::attachments::InMemoryAttachmentStore::new();
    let attachment = lash_core::AttachmentStore::put(
        &backend,
        b"sqlite-live-committed-blob".to_vec(),
        lash_sansio::AttachmentCreateMeta::new(
            lash_sansio::MediaType::parse("application/octet-stream").expect("media type"),
            None,
            Some("live".to_string()),
        ),
    )
    .await
    .expect("put shared backend blob");
    lash_core::AttachmentManifest::record_intent(
        &*store,
        lash_core::AttachmentIntent {
            attachment_id: attachment.id.clone(),
            session_id: request.session_id.clone(),
            canonical_uri: format!("lash-attachment://sha256/{}", attachment.id),
            intent_at_epoch_ms: 1,
            owner_kind: None,
            owner_id: None,
        },
    )
    .expect("record live attachment intent");
    lash_core::AttachmentManifest::commit_refs(
        &*store,
        &request.session_id,
        std::slice::from_ref(&attachment.id),
    )
    .expect("commit live attachment ref");

    let missing_factory = SqliteSessionStoreFactory::new(dir.path().join("wrong-sessions"));
    let result = lash_core::attachments::reclaim_unreferenced_attachments(
        &missing_factory,
        &backend,
        lash_core::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: lash_core::EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await;

    assert!(
        matches!(result, Err(lash_core::AttachmentStoreError::Backend(ref message)) if message.contains("failed to enumerate live attachment refs")),
        "a missing catalog must abort GC even when delete-all is authorized: {result:?}"
    );
    lash_core::AttachmentStore::get(&backend, &attachment.id)
        .await
        .expect("live committed blob survives the refused sweep");
}

#[tokio::test]
async fn attachment_gc_allows_a_fresh_deployment_with_an_empty_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let factory = SqliteSessionStoreFactory::new(dir.path().join("sessions"));
    let backend = lash_core::attachments::InMemoryAttachmentStore::new();

    let result = lash_core::attachments::reclaim_unreferenced_attachments(
        &factory,
        &backend,
        lash_core::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: lash_core::EmptyRootSetPolicy::Refuse,
        },
    )
    .await;

    let report = result.expect("an empty fresh deployment has nothing to protect");
    assert_eq!(report.scanned_blob_count, 0);
    assert_eq!(report.reclaimed_count, 0);
    assert!(
        report
            .root_enumeration_failure
            .as_deref()
            .is_some_and(|failure| failure.contains("durable-core catalog")),
        "the returned report must distinguish enumeration failure: {report:?}"
    );
}

#[tokio::test]
async fn attachment_gc_allows_an_operator_reset_with_an_empty_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let factory = SqliteSessionStoreFactory::new(dir.path().join("sessions"));
    let request = SessionStoreCreateRequest {
        session_id: "reset-empty-attachment-gc".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let store = factory
        .create_store(&request)
        .await
        .expect("initialize factory catalog");
    drop(store);
    std::fs::remove_file(factory.catalog_path()).expect("remove catalog for operator reset");
    let backend = lash_core::attachments::InMemoryAttachmentStore::new();

    let result = lash_core::attachments::reclaim_unreferenced_attachments(
        &factory,
        &backend,
        lash_core::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: lash_core::EmptyRootSetPolicy::Refuse,
        },
    )
    .await;

    let report = result.expect("an empty reset deployment has nothing to protect");
    assert_eq!(report.scanned_blob_count, 0);
    assert_eq!(report.reclaimed_count, 0);
    assert!(
        report
            .root_enumeration_failure
            .as_deref()
            .is_some_and(|failure| failure.contains("durable-core catalog")),
        "the returned report must distinguish enumeration failure: {report:?}"
    );
}

#[tokio::test]
async fn targeted_attachment_ref_probe_aborts_when_the_factory_catalog_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let factory = SqliteSessionStoreFactory::new(dir.path().join("missing-sessions"));
    let attachment_id = lash_core::AttachmentId::new("b".repeat(64));

    let result =
        lash_core::AttachmentRootSet::has_live_attachment_ref(&factory, &attachment_id, 0).await;

    assert!(
        result.is_err(),
        "a missing catalog must abort the targeted root probe: {result:?}"
    );
}

#[tokio::test]
async fn open_existing_store_aborts_on_unreadable_requested_session_meta() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("sessions");
    let factory = SqliteSessionStoreFactory::new(&root);
    let request = SessionStoreCreateRequest {
        session_id: "corrupt-session-meta".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };

    let store = factory
        .create_store(&request)
        .await
        .expect("create requested session");
    drop(store);
    let raw = rusqlite::Connection::open(factory.catalog_path()).expect("open raw catalog");
    raw.execute(
        "UPDATE session_meta SET relation_json = '{'
             WHERE session_id = ?1",
        params![request.session_id],
    )
    .expect("corrupt requested session metadata");
    drop(raw);

    let result = factory.open_existing_store(&request).await;
    assert!(
        result.is_err(),
        "unreadable requested session metadata must not look absent"
    );
}

#[tokio::test]
async fn segment_handover_persist_keeps_current_input_for_crash_replay() {
    let registry = SqliteProcessRegistry::memory()
        .await
        .expect("memory registry");
    registry
        .register_process(registration("segment-crash"))
        .await
        .expect("register");
    let handover = |segment_ordinal| PersistedSegmentHandover {
        segment_ordinal,
        program_hash: "program-v1".to_string(),
        handover: lash_core::SegmentHandover {
            reason: lash_core::BoundaryReason::JournalBudget,
            program_hash: Some("program-v1".to_string()),
            engine_state: vec![segment_ordinal as u8],
        },
    };
    registry
        .put_segment_handover("segment-crash", handover(1))
        .await
        .expect("persist current segment input");
    registry
        .put_segment_handover("segment-crash", handover(2))
        .await
        .expect("persist successor before send");

    assert_eq!(
        registry
            .get_segment_handover("segment-crash", 1)
            .await
            .expect("replay read"),
        Some(handover(1)),
        "a crash before successor send must leave segment 1 replayable"
    );
    assert_eq!(
        registry
            .latest_segment_handover("segment-crash")
            .await
            .expect("latest handover"),
        Some(handover(2))
    );
}

#[tokio::test]
async fn terminal_segment_handover_cleanup_removes_continuation_state() {
    let registry = SqliteProcessRegistry::memory()
        .await
        .expect("memory registry");
    registry
        .register_process(registration("segment-terminal"))
        .await
        .expect("register");
    registry
        .put_segment_handover(
            "segment-terminal",
            PersistedSegmentHandover {
                segment_ordinal: 1,
                program_hash: "program-v1".to_string(),
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: Some("program-v1".to_string()),
                    engine_state: vec![7],
                },
            },
        )
        .await
        .expect("persist handover");
    registry
        .delete_segment_handovers("segment-terminal")
        .await
        .expect("terminal cleanup");
    assert!(
        registry
            .latest_segment_handover("segment-terminal")
            .await
            .expect("latest handover")
            .is_none()
    );
}

#[tokio::test]
async fn sqlite_lashlang_artifact_store_round_trips_verified_module_artifacts() {
    let store = Store::memory().await.expect("memory store");
    let module = lashlang::parse("process scan(root: str) { finish root }").expect("parse module");
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::all(),
        ),
    )
    .expect("link module");

    store
        .put_module_artifact(&linked.artifact)
        .await
        .expect("put artifact");
    let restored = store
        .get_module_artifact(&linked.module_ref)
        .await
        .expect("get artifact")
        .expect("artifact exists");

    assert_eq!(restored.module_ref, linked.module_ref);
    assert_eq!(
        restored.process_ref("scan"),
        linked.artifact.process_ref("scan")
    );
}

#[tokio::test]
async fn sqlite_process_registry_persists_rows_after_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("processes.db");
    {
        let registry = SqliteProcessRegistry::open(&path, dir.path().join("sessions"))
            .await
            .expect("open registry");
        let session_scope = lash_core::SessionScope::new("session");
        registry
            .register_process(registration("proc-persist"))
            .await
            .expect("register");
        registry
            .add_observer(
                &session_scope.session_id,
                "proc-persist",
                lash_core::ProcessObserverBy::host("sqlite-reopen-test"),
            )
            .await
            .expect("observe");
        registry
            .complete_process(
                "proc-persist",
                ProcessAwaitOutput::Success {
                    value: serde_json::json!({"ok": true}),
                    control: None,
                },
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete");
    }

    let registry = Arc::new(
        SqliteProcessRegistry::open(&path, dir.path().join("sessions"))
            .await
            .expect("reopen registry"),
    ) as Arc<dyn lash_core::ProcessRegistry>;
    let session_scope = lash_core::SessionScope::new("session");
    let record = registry
        .get_process("proc-persist")
        .await
        .expect("read process")
        .expect("persisted process");

    assert_eq!(record.originator_id(), session_scope.session_id);
    assert_eq!(
        record.provenance.originator,
        lash_core::ProcessOriginator::session(session_scope.clone())
    );
    assert_eq!(
        lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry))
            .await_terminal("proc-persist")
            .await
            .expect("await persisted"),
        ProcessAwaitOutput::Success {
            value: serde_json::json!({"ok": true}),
            control: None,
        }
    );
    assert_eq!(
        registry
            .list_observed_by(&session_scope.session_id)
            .await
            .expect("observed processes")
            .len(),
        1
    );
}
