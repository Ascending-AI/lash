// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::session_listing::list_session_summaries;

/// `process <name>(<param>: str) -> str { finish <param> }`, the publishable
/// one-process module these store fixtures need. ADR 0096 retired the Lashlang
/// front-end, so the fixture states its AST.
fn one_process_module(process_name: &str, param: &str) -> lashlang::Program {
    use lashlang::testing::ast_builders as b;

    b::module(
        vec![b::process_returning(
            process_name,
            vec![b::param(param, lashlang::TypeExpr::Str)],
            lashlang::TypeExpr::Str,
            b::finish(b::var(param)),
        )],
        Vec::new(),
    )
}

use lash_core_execution::{
    ProcessLifecycle as _, ProcessObserverRegistry as _, ProcessRegistrar as _,
};
use lash_sansio::{ProcessId, SessionId};
use std::sync::atomic::Ordering;

use lash_core_execution::ProcessInput;

static CHECKPOINT_DATA_STATEMENT_COUNT: AtomicUsize = AtomicUsize::new(0);
static SESSION_LIST_STATEMENT_COUNT: AtomicUsize = AtomicUsize::new(0);

#[test]
fn public_session_schema_version_tracks_the_internal_schema_version() {
    assert_eq!(SESSION_SCHEMA_VERSION, crate::schema::SCHEMA_VERSION);
}

lash_conformance::tool_access_persistence_tests!({
    let dir = tempfile::tempdir().expect("tool-access SQLite tempdir");
    let factory = Arc::new(SqliteSessionStoreFactory::new(dir.path()));
    (dir, factory)
});

#[test]
fn queued_work_checks_reject_illegal_vocabulary() {
    let connection = rusqlite::Connection::open_in_memory().expect("open SQLite CHECK witness");
    connection
        .execute_batch(crate::schema::SCHEMA)
        .expect("apply SQLite schema to CHECK witness");
    let assert_rejected = |statement: &str, constraint: &str| {
        let error = connection
            .execute(statement, [])
            .expect_err("an illegal queued-work row must violate its schema CHECK");
        assert!(
            error.to_string().contains(constraint),
            "SQLite reported the wrong CHECK: {error}"
        );
    };

    assert_rejected(
        "INSERT INTO queued_work_batches (enqueue_seq,
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             enqueued_at_ms
         ) VALUES (1,
             'bad-kind', 'session', 'earliest_safe_boundary', 'cancel', '{}', 0
         )",
        "ck_queued_work_batches_work_kind",
    );
    assert_rejected(
        "INSERT INTO queued_work_batches (enqueue_seq,
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             enqueued_at_ms
         ) VALUES (1, 'bad-policy', 'session', 'eventually', 'turn', '{}', 0)",
        "ck_queued_work_batches_delivery_policy",
    );
}

#[test]
fn ingress_admission_binding_must_be_all_or_none() {
    let connection = rusqlite::Connection::open_in_memory().expect("open SQLite CHECK witness");
    connection
        .execute_batch(crate::schema::SCHEMA)
        .expect("apply SQLite schema to CHECK witness");
    // A row is open or admitted to a root by a recorded step: a root without
    // its step, or a step without its root, is unrepresentable (FIG-3927).
    for (fields, values) in [("admitted_root", "'root'"), ("admitted_by", "'admit'")] {
        let error = connection
            .execute(
                &format!(
                    "INSERT INTO pending_turn_inputs (enqueue_seq,
                         input_id, session_id, ingress_json, state, input_json,
                         submitted_ingress_json, submission_digest, enqueued_at_ms, {fields}
                     ) VALUES (1, 'input', 'session', '{{\"scope\":\"next_turn\"}}',
                               'deferred_next_turn', '{{}}', '{{}}', 'digest', 0, {values})"
                ),
                [],
            )
            .expect_err("a half-bound pending input must be rejected");
        assert!(
            error
                .to_string()
                .contains("ck_pending_turn_inputs_admission_all_or_none"),
            "SQLite reported the wrong CHECK: {error}"
        );
        let error = connection
            .execute(
                &format!(
                    "INSERT INTO queued_work_batches (enqueue_seq,
                         batch_id, session_id, delivery_policy, work_kind, authority_json,
                         enqueued_at_ms, {fields}
                     ) VALUES (1, 'batch', 'session', 'earliest_safe_boundary', 'turn',
                               '{{}}', 0, {values})"
                ),
                [],
            )
            .expect_err("a half-bound batch must be rejected");
        assert!(
            error
                .to_string()
                .contains("ck_queued_work_batches_admission_all_or_none"),
            "SQLite reported the wrong CHECK: {error}"
        );
    }
    // A settled input is answered, so no root holds it.
    let error = connection
        .execute(
            "INSERT INTO pending_turn_inputs (enqueue_seq,
                 input_id, session_id, ingress_json, state, input_json,
                 submitted_ingress_json, submission_digest, enqueued_at_ms,
                 admitted_root, admitted_by
             ) VALUES (1, 'settled', 'session', '{\"scope\":\"next_turn\"}',
                       'completed', '{}', '{}', 'digest', 0, 'root', 'admit')",
            [],
        )
        .expect_err("a settled input still bound to a root must be rejected");
    assert!(
        error
            .to_string()
            .contains("ck_pending_turn_inputs_settled_unadmitted"),
        "SQLite reported the wrong CHECK: {error}"
    );
}

/// FIG-3886: the `(session_id, source_key)` dedup on both admission tables
/// aborts instead of ignoring — which stored row an id names is the admission
/// verdict's call, never the constraint's — and a violation the pre-insert
/// read missed maps to the same typed identity conflict an `input_id` reuse
/// gets, the one PostgreSQL's `23505` already produces.
#[test]
fn pending_turn_inputs_reject_a_duplicate_source_key_insert() {
    let connection =
        rusqlite::Connection::open_in_memory().expect("open SQLite constraint witness");
    connection
        .execute_batch(crate::schema::SCHEMA)
        .expect("apply SQLite schema to constraint witness");
    let insert = "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, source_key, ingress_json, state, input_json,
             submitted_ingress_json, submission_digest, enqueued_at_ms
         ) VALUES (?1, ?2, 'session', ?3, '{\"scope\":\"next_turn\"}',
                   'deferred_next_turn', '{}', '{}', 'digest', 0)";
    connection
        .execute(insert, rusqlite::params![1, "first", "key"])
        .expect("admit the first row");
    let error = connection
        .execute(insert, rusqlite::params![2, "second", "key"])
        .expect_err("a duplicate (session_id, source_key) insert must abort");
    let mapped =
        crate::sqlite_pending_turn_input_insert_error(error, &SessionId::from("session"), "second");
    assert!(
        matches!(
            &mapped,
            StoreError::PendingTurnInputIdConflict { session_id, input_id }
                if session_id.as_str() == "session" && input_id.as_str() == "second"
        ),
        "the duplicate source-key insert maps to the typed identity conflict: {mapped:?}"
    );
    let error = connection
        .execute(insert, rusqlite::params![3, "first", "other-key"])
        .expect_err("a duplicate input_id insert must abort");
    let mapped =
        crate::sqlite_pending_turn_input_insert_error(error, &SessionId::from("session"), "first");
    assert!(
        matches!(&mapped, StoreError::PendingTurnInputIdConflict { .. }),
        "the duplicate input_id insert maps to the typed identity conflict: {mapped:?}"
    );
}

#[test]
fn queued_work_batches_reject_a_duplicate_source_key_insert() {
    let connection =
        rusqlite::Connection::open_in_memory().expect("open SQLite constraint witness");
    connection
        .execute_batch(crate::schema::SCHEMA)
        .expect("apply SQLite schema to constraint witness");
    let insert = "INSERT INTO queued_work_batches (enqueue_seq,
             batch_id, session_id, source_key, delivery_policy, work_kind,
             authority_json, enqueued_at_ms
         ) VALUES (?1, ?2, 'session', 'key', 'earliest_safe_boundary', 'turn',
                   '{}', 0)";
    connection
        .execute(insert, rusqlite::params![1, "first"])
        .expect("admit the first row");
    let error = connection
        .execute(insert, rusqlite::params![2, "second"])
        .expect_err("a duplicate (session_id, source_key) insert must abort");
    assert!(
        error.to_string().contains(
            "UNIQUE constraint failed: queued_work_batches.session_id, queued_work_batches.source_key"
        ),
        "the duplicate source-key insert names its constraint: {error}"
    );
}

#[tokio::test]
async fn store_options_apply_connection_policy_on_connection_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("policy.db");
    let store = SqliteStore::open_with_options(
        &path,
        StoreOptions {
            blob_profile: BuiltinBlobProfile::Balanced,
            connection_policy: SqliteConnectionPolicy {
                read_connections: std::num::NonZeroUsize::new(4).expect("four is nonzero"),
                busy_timeout: std::time::Duration::from_millis(321),
                synchronous: SqliteSynchronous::Full,
                wal_autocheckpoint_pages: 17,
                cache_size: -4096,
            },
        },
    )
    .await
    .expect("open store with connection policy");

    let pragmas = store
        .conn
        .call(|connection| {
            Ok((
                connection.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))?,
                connection.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))?,
                connection
                    .query_row("PRAGMA wal_autocheckpoint", [], |row| row.get::<_, i64>(0))?,
                connection.query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))?,
                connection.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?,
            ))
        })
        .await
        .expect("read connection policy pragmas on connection thread");

    assert_eq!(pragmas, (321, 2, 17, -4096, "wal".to_string()));
}

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

fn count_session_list_statement(event: rusqlite::trace::TraceEvent<'_>) {
    // Count EVERY statement in the traced window, not just the catalog CTE:
    // the invariant is that listing issues one statement total, so a
    // reintroduced per-session read must be visible to this counter.
    if let rusqlite::trace::TraceEvent::Stmt(..) = event {
        SESSION_LIST_STATEMENT_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

async fn traced_session_list(factory: &SqliteSessionStoreFactory) -> (Vec<SessionSummary>, usize) {
    let conn = SqliteConnection::open_readonly(factory.core.target())
        .await
        .expect("open session catalog for statement tracing");
    SESSION_LIST_STATEMENT_COUNT.store(0, Ordering::Relaxed);
    let summaries = conn
        .call(|conn| {
            conn.trace_v2(
                rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
                Some(count_session_list_statement as fn(rusqlite::trace::TraceEvent<'_>)),
            );
            let result = list_session_summaries(conn, &SessionListFilter::default());
            conn.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
            result
        })
        .await
        .expect("list session summaries under statement trace");
    (
        summaries,
        SESSION_LIST_STATEMENT_COUNT.load(Ordering::Relaxed),
    )
}

#[tokio::test]
async fn session_listing_statement_count_is_session_count_invariant() {
    let dir = tempfile::tempdir().expect("tempdir");
    let factory = SqliteSessionStoreFactory::new(dir.path());
    let mut expected_relations = BTreeMap::new();

    for index in 0..8 {
        let session_id = SessionId::from(format!("listing-statement-count-{index}"));
        let relation = if index == 0 {
            lash_core_execution::SessionRelation::Root
        } else {
            lash_core_execution::SessionRelation::Fork {
                source_session_id: SessionId::from("listing-statement-count-0"),
                source_node_id: format!("source-node-{index}").into(),
            }
        };
        factory
            .create_store(&SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: if index == 0 {
                    Vec::new()
                } else {
                    vec![
                        lash_core_execution::facade_support::SessionObserverIntent::host_requested(
                            ProcessId::fixture(&format!("intent-process-{index}")),
                        ),
                    ]
                },
                session_id: session_id.clone(),
                relation: relation.clone(),
                policy: lash_core_execution::SessionPolicy::new(
                    lash_core_execution::TurnBudget::Unbounded,
                ),
            })
            .await
            .expect("create session listing fixture");
        expected_relations.insert(session_id, relation);

        if index == 0 {
            let (single, statement_count) = traced_session_list(&factory).await;
            assert_eq!(single.len(), 1);
            assert_eq!(statement_count, 1);
        }
    }

    let (many, statement_count) = traced_session_list(&factory).await;
    assert_eq!(statement_count, 1);
    assert_eq!(many.len(), expected_relations.len());
    for summary in many {
        assert_eq!(
            summary.durable_relation.as_ref(),
            expected_relations.get(&summary.session_id)
        );
    }
}

async fn set_checkpoint_statement_trace(store: &SqliteStore, enabled: bool) {
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
                    lash_core_execution::HydratedCheckpointComponent::changed(
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
                    lash_core_execution::HydratedCheckpointComponent::unchanged(descriptor),
                )
            })
            .collect(),
    }
}

async fn durable_state(
    store: &SqliteStore,
    session_id: &SessionId,
) -> lash_core_execution::RuntimeSessionState {
    let state = lash_core_execution::RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    store
        .admit_and_bind_session(&lash_core_execution::SessionBinding::root(session_id))
        .await
        .expect("bind SQLite test session");
    state
}

lash_conformance::checkpoint_admission_probe_tests!({
    let store = Arc::new(
        crate::test_support::memory_store()
            .await
            .expect("open counter store"),
    );
    let counting_store = Arc::clone(&store);
    (
        (),
        store as Arc<dyn RuntimePersistence>,
        SessionId::from("sqlite-checkpoint-counter"),
        move || counting_store.checkpoint_admission_counts(),
        // An in-memory SQLite store needs no session teardown.
        async {},
    )
});

#[tokio::test]
async fn checkpoint_component_statement_count_is_depth_invariant() {
    let mut observed = Vec::new();
    for depth in [10, 100, 1_000, 4_000] {
        let store = Arc::new(
            crate::test_support::memory_store()
                .await
                .expect("open depth-invariance store"),
        );
        let mut state = durable_state(
            &store,
            &SessionId::from(format!("sqlite-checkpoint-depth-{depth}")),
        )
        .await;
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

fn registration() -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core_execution::ProcessProvenance::session(lash_core_execution::SessionScope::new(
            "session",
        )),
        lash_core_execution::Lifetime::Detached,
    )
}

#[tokio::test]
async fn real_locked_catalog_surfaces_typed_contention() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("contended.db");
    let store = SqliteStore::open(&path).await.expect("open store");
    store
        .bind_session(&SessionId::from("contended"))
        .expect("bind store");
    let state = durable_state(&store, &SessionId::from("contended")).await;
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

    let catalog = root.join(crate::SqliteDatabase::DurableCore.file_name());
    let attachment_id =
        lash_core_execution::AttachmentId::parse("a".repeat(64)).expect("valid attachment id");
    {
        let store = SqliteStore::open(&catalog).await.expect("open catalog");
        let intent = lash_core_execution::AttachmentIntent {
            attachment_id: attachment_id.clone(),
            session_id: SessionId::from("sess-1"),
            canonical_uri: format!("lash-attachment://blake3/{attachment_id}"),
            intent_at_epoch_ms: 1_000,
            owner: None,
        };
        let lash_core_execution::AttachmentWriteFence::Granted(permit) =
            lash_core_execution::AttachmentManifest::begin_attachment_write(&store, intent.clone())
                .await
                .expect("begin write")
        else {
            panic!("a free digest must grant its writer");
        };
        lash_core_execution::AttachmentManifest::complete_attachment_write(&store, &intent, permit)
            .await
            .expect("stamp upload evidence");
        lash_core_execution::AttachmentManifest::commit_refs(
            &store,
            &SessionId::from("sess-1"),
            std::slice::from_ref(&attachment_id),
        )
        .await
        .expect("commit ref");
    }

    let refs = lash_core_execution::AttachmentRootSet::live_attachment_refs(&factory, 0)
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

    std::fs::write(
        root.join(crate::SqliteDatabase::DurableCore.file_name()),
        b"corrupt not-a-db",
    )
    .expect("write corrupt");

    let result = lash_core_execution::AttachmentRootSet::live_attachment_refs(&factory, 0).await;
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
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("live-attachment"),
        relation: lash_core_execution::SessionRelation::Root,
        policy: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };
    let store = live_factory
        .create_store(&request)
        .await
        .expect("create live session store");
    let backend =
        lash_core_execution::attachments::FileAttachmentSqliteStore::new(dir.path().join("blobs"));
    let attachment = lash_core_execution::AttachmentSqliteStore::put(
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
    let live_intent = lash_core_execution::AttachmentIntent {
        attachment_id: attachment.id.clone(),
        session_id: request.session_id.clone(),
        canonical_uri: format!("lash-attachment://blake3/{}", attachment.id),
        intent_at_epoch_ms: 1,
        owner: None,
    };
    let lash_core_execution::AttachmentWriteFence::Granted(live_permit) =
        lash_core_execution::AttachmentManifest::begin_attachment_write(
            &*store,
            live_intent.clone(),
        )
        .await
        .expect("begin live attachment write")
    else {
        panic!("a free digest must grant its writer");
    };
    lash_core_execution::AttachmentManifest::complete_attachment_write(
        &*store,
        &live_intent,
        live_permit,
    )
    .await
    .expect("stamp live attachment upload");
    lash_core_execution::AttachmentManifest::commit_refs(
        &*store,
        &request.session_id,
        std::slice::from_ref(&attachment.id),
    )
    .await
    .expect("commit live attachment ref");

    let missing_factory = SqliteSessionStoreFactory::new(dir.path().join("wrong-sessions"));
    let result = lash_core_execution::attachments::reclaim_unreferenced_attachments(
        &missing_factory,
        &backend,
        lash_core_execution::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: lash_core_execution::EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await;

    assert!(
        matches!(
            &result,
            Err(failure)
                if matches!(
                    &failure.stop,
                    lash_core_execution::MaintenanceStop::Failed(
                        lash_core_execution::AttachmentStoreError::RootSetEnumerationFailed { .. }
                    )
                )
        ),
        "a missing catalog must abort GC even when delete-all is authorized: {result:?}"
    );
    lash_core_execution::AttachmentSqliteStore::get(&backend, &attachment.id)
        .await
        .expect("live committed blob survives the refused sweep");
}

#[tokio::test]
async fn attachment_gc_allows_an_operator_reset_with_an_empty_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let factory = SqliteSessionStoreFactory::new(dir.path().join("sessions"));
    let request = SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("reset-empty-attachment-gc"),
        relation: lash_core_execution::SessionRelation::Root,
        policy: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };
    let store = factory
        .create_store(&request)
        .await
        .expect("initialize factory catalog");
    drop(store);
    std::fs::remove_file(
        dir.path()
            .join("sessions")
            .join(crate::SqliteDatabase::DurableCore.file_name()),
    )
    .expect("remove catalog for operator reset");
    let backend =
        lash_core_execution::attachments::FileAttachmentSqliteStore::new(dir.path().join("blobs"));

    let result = lash_core_execution::attachments::reclaim_unreferenced_attachments(
        &factory,
        &backend,
        lash_core_execution::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: lash_core_execution::EmptyRootSetPolicy::Refuse,
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
    let attachment_id =
        lash_core_execution::AttachmentId::parse("b".repeat(64)).expect("valid attachment id");

    let result = lash_core_execution::AttachmentRootSet::has_live_attachment_ref(
        &factory,
        &attachment_id,
        0,
    )
    .await;

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
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("corrupt-session-meta"),
        relation: lash_core_execution::SessionRelation::Root,
        policy: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };

    let store = factory
        .create_store(&request)
        .await
        .expect("create requested session");
    drop(store);
    let raw = rusqlite::Connection::open(factory.catalog_uri()).expect("open raw catalog");
    // `ck_session_meta_relation_kind` forbids this row on any ordinary write.
    // The refusal below is still the contract for a catalog that carries one
    // anyway — restored from a pre-CHECK dump, or ALTERed by a host.
    raw.pragma_update(None, "ignore_check_constraints", true)
        .expect("permit manufacturing a row the DDL now forbids");
    raw.execute(
        "UPDATE session_meta SET relation_kind = 'corrupt'
             WHERE session_id = ?1",
        params![request.session_id.as_str()],
    )
    .expect("corrupt requested session metadata");
    raw.pragma_update(None, "ignore_check_constraints", false)
        .expect("restore CHECK enforcement");
    drop(raw);

    let result = factory.open_existing_store(&request).await;
    assert!(
        result.is_err(),
        "unreadable requested session metadata must not look absent"
    );
}

#[tokio::test]
async fn segment_handover_persist_keeps_current_input_for_crash_replay() {
    let registry = crate::SqliteStoreSet::memory()
        .await
        .expect("memory registry")
        .process_registry();
    let segment_crash_id = registry
        .register_process(registration())
        .await
        .expect("register")
        .id;
    let handover = |segment_ordinal| PersistedSegmentHandover {
        writer: String::new(),
        segment_ordinal,
        written_generation: Some(lash_core_execution::engine::BuildGeneration::for_test("t0")),
        route: "LashProcessWorkflow".to_string(),
        handover: lash_core_execution::SegmentHandover {
            reason: lash_core_execution::BoundaryReason::JournalBudget,
            program_hash: "program-v1".to_string(),
            engine_state: vec![segment_ordinal as u8],
        },
    };
    registry
        .put_segment_handover(&segment_crash_id, handover(1))
        .await
        .expect("persist current segment input");
    registry
        .put_segment_handover(&segment_crash_id, handover(2))
        .await
        .expect("persist successor before send");

    assert_eq!(
        registry
            .get_segment_handover(&segment_crash_id, 1)
            .await
            .expect("replay read"),
        Some(handover(1)),
        "a crash before successor send must leave segment 1 replayable"
    );
    assert_eq!(
        registry
            .latest_segment_handover(&segment_crash_id)
            .await
            .expect("latest handover"),
        Some(handover(2))
    );
}

#[tokio::test]
async fn terminal_segment_handover_cleanup_removes_continuation_state() {
    let registry = crate::SqliteStoreSet::memory()
        .await
        .expect("memory registry")
        .process_registry();
    let segment_terminal_id = registry
        .register_process(registration())
        .await
        .expect("register")
        .id;
    registry
        .put_segment_handover(
            &segment_terminal_id,
            PersistedSegmentHandover {
                writer: String::new(),
                segment_ordinal: 1,
                written_generation: Some(lash_core_execution::engine::BuildGeneration::for_test(
                    "t0",
                )),
                route: "LashProcessWorkflow".to_string(),
                handover: lash_core_execution::SegmentHandover {
                    reason: lash_core_execution::BoundaryReason::JournalBudget,
                    program_hash: "program-v1".to_string(),
                    engine_state: vec![7],
                },
            },
        )
        .await
        .expect("persist handover");
    registry
        .delete_segment_handovers(&segment_terminal_id)
        .await
        .expect("terminal cleanup");
    assert!(
        registry
            .latest_segment_handover(&segment_terminal_id)
            .await
            .expect("latest handover")
            .is_none()
    );
}

#[tokio::test]
async fn sqlite_lashlang_artifact_store_round_trips_verified_module_artifacts() {
    let store = lashlang::LashlangArtifacts::new(Arc::new(
        crate::test_support::memory_store()
            .await
            .expect("memory store"),
    ));
    // process scan(root: str) -> str { finish root }
    let module = one_process_module("scan", "root");
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::all(),
        ),
    )
    .expect("link module");

    store
        .publish_module_artifact(
            &lash_core_execution::ArtifactOwner::host("sqlite-store-test"),
            &linked.artifact,
        )
        .await
        .expect("put artifact");
    let restored = store
        .get_module_artifact(linked.artifact.module_ref())
        .await
        .expect("get artifact")
        .expect("artifact exists");

    assert_eq!(restored.module_ref(), linked.artifact.module_ref());
    assert_eq!(
        restored.process_ref("scan"),
        linked.artifact.process_ref("scan")
    );
}

#[tokio::test]
async fn sqlite_module_cache_does_not_resurrect_artifact_reclaimed_by_another_handle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("artifacts.db");
    let releasing = lashlang::LashlangArtifacts::new(Arc::new(
        SqliteStore::open(&path)
            .await
            .expect("open releasing store"),
    ));
    let cached = lashlang::LashlangArtifacts::new(Arc::new(
        SqliteStore::open(&path).await.expect("open caching store"),
    ));
    // process cache_probe(root: str) -> str { finish root }
    let module = lashlang::ModuleArtifact::from_program(one_process_module("cache_probe", "root"))
        .expect("build module artifact");
    let owner = lash_core_execution::ArtifactOwner::host("cross-handle-cache-owner");

    releasing
        .publish_module_artifact(&owner, &module)
        .await
        .expect("publish module through first handle");
    assert!(
        cached
            .get_module_artifact(module.module_ref())
            .await
            .expect("prime second handle cache")
            .is_some()
    );
    releasing
        .release_module_artifact(&owner, module.module_ref())
        .await
        .expect("release final owner through first handle");

    assert!(
        cached
            .get_module_artifact(module.module_ref())
            .await
            .expect("read after cross-handle reclamation")
            .is_none(),
        "a handle-local cache must not resurrect durably reclaimed bytes"
    );
}

#[tokio::test]
async fn sqlite_process_registry_persists_rows_after_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("processes.db");
    let proc_persist_id = {
        let registry = SqliteProcessRegistry::open(&path, dir.path().join("sessions"))
            .await
            .expect("open registry");
        let session_scope = lash_core_execution::SessionScope::new("session");
        let proc_persist_id = registry
            .register_process(registration())
            .await
            .expect("register")
            .id;
        registry
            .add_observer(
                &session_scope.session_id,
                &proc_persist_id,
                lash_core_execution::ProcessObserverBy::host("sqlite-reopen-test"),
            )
            .await
            .expect("observe");
        registry
            .complete_process(
                &proc_persist_id,
                ProcessAwaitOutput::from_tool_output(lash_core_execution::ToolCallOutput::success(
                    serde_json::json!({"ok": true}),
                )),
                lash_core_execution::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete");
        proc_persist_id
    };

    let registry = Arc::new(
        SqliteProcessRegistry::open(&path, dir.path().join("sessions"))
            .await
            .expect("reopen registry"),
    ) as Arc<dyn lash_core_execution::ProcessRegistry>;
    let session_scope = lash_core_execution::SessionScope::new("session");
    let record = registry
        .get_process(&proc_persist_id)
        .await
        .expect("read process")
        .expect("persisted process");

    assert_eq!(record.originator_id(), session_scope.session_id);
    assert_eq!(
        record.provenance.originator,
        lash_core_execution::ProcessOriginator::session(session_scope.clone())
    );
    assert_eq!(
        lash_core_execution::NoProcessWork::for_registry(Arc::clone(&registry))
            .await_terminal(&proc_persist_id)
            .await
            .expect("await persisted"),
        ProcessAwaitOutput::from_tool_output(lash_core_execution::ToolCallOutput::success(
            serde_json::json!({"ok": true}),
        ))
    );
    assert_eq!(
        registry
            .list_observed_by(
                &session_scope.session_id,
                &lash_core_execution::ProcessListFilter {
                    status: lash_core_execution::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await
            .expect("observed processes")
            .len(),
        1
    );
}

// FIG-1282: two concurrent admissions on one unbound handle race to bind it.
// The loser's SessionBindingMismatch must arrive without its session being
// durably created — rejection precedes creation, atomically (the
// SessionCommitStore contract), so the binding decision runs inside the
// admission's write transaction.
#[tokio::test]
async fn concurrent_admission_loser_leaves_no_metadata() {
    let dir = tempfile::tempdir().expect("admission race tempdir");
    let path = dir.path().join("admission-race.db");
    let store = Arc::new(SqliteStore::open(&path).await.expect("open unbound store"));

    let first_id = SessionId::from("admission-race-a");
    let second_id = SessionId::from("admission-race-b");
    let first_binding = lash_core_execution::SessionBinding::root(first_id.clone());
    let second_binding = lash_core_execution::SessionBinding::root(second_id.clone());
    let (first, second) = tokio::join!(
        store.admit_and_bind_session(&first_binding),
        store.admit_and_bind_session(&second_binding),
    );

    let rejected_id = match (first, second) {
        (Ok(lash_core_execution::SessionAdmission::Created), Err(error)) => {
            assert!(
                matches!(error, StoreError::SessionBindingMismatch { .. }),
                "the losing admission must report SessionBindingMismatch, got {error:?}"
            );
            second_id
        }
        (Err(error), Ok(lash_core_execution::SessionAdmission::Created)) => {
            assert!(
                matches!(error, StoreError::SessionBindingMismatch { .. }),
                "the losing admission must report SessionBindingMismatch, got {error:?}"
            );
            first_id
        }
        (first, second) => panic!(
            "concurrent admissions must yield one Created and one SessionBindingMismatch, \
             got {first:?} and {second:?}"
        ),
    };

    let rejected = SqliteStore::open_bound_readonly(
        &crate::location::DatabaseLocation::standalone_file(&path),
        &rejected_id,
    )
    .await
    .expect("open rejected session read-only");
    assert!(
        rejected
            .load_session_meta()
            .await
            .expect("read rejected session metadata")
            .is_none(),
        "a refused admission must leave no durable session metadata"
    );
}
