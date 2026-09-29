//! Crate-root tests for the PostgreSQL store wiring.
//!
//! Split out of `lib.rs` as a sibling of the `#[path]`-declared test modules
//! beside it, so the crate root stays the wiring surface it describes rather
//! than carrying its own suite inline.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core_execution::DeploymentStore as _;
use lash_core_execution::TurnInputStore as _;
use lash_core_execution::store::RootStore as _;
use lash_core_execution::testing::store_fixtures::RuntimeStoreTestDriveExt as _;
use lash_core_execution::{LeaseOwnerIdentity, TurnId};
use lash_core_execution::{SessionCatalogStore as _, SessionHistoryStore as _};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, Registry};

async fn persisted_record_decode_store(
    storage: &PostgresStorage,
    label: &str,
) -> (SessionId, PostgresStore) {
    let session_id = SessionId::from(format!(
        "persisted-record-decode-{label}:{}",
        uuid::Uuid::new_v4()
    ));
    let store = storage.store();
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&session_id),
        )
        .await
        .expect("admit persisted-record decode session");
    let state = lash_core_execution::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    store
        .commit_runtime_state(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[]),
        )
        .await
        .expect("seed persisted-record decode session");
    (session_id, store)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_persisted_record_decode_classification_head_when_configured() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres head decode classification: database URL is not set");
        return;
    };
    let isolated_database = crate::testing::IsolatedDatabase::create(&database_url).await;
    let storage = PostgresStorage::connect(isolated_database.url())
        .await
        .expect("connect persisted-record decode storage");

    let (head_session_id, head_store) = persisted_record_decode_store(&storage, "head").await;
    assert_eq!(
        sqlx::query("UPDATE lash_sessions SET head_json = '{' WHERE session_id = $1")
            .bind(head_session_id.as_str())
            .execute(storage.pool())
            .await
            .expect("corrupt head JSON")
            .rows_affected(),
        1
    );
    let head_error = head_store
        .load_session_head_meta(&head_session_id)
        .await
        .expect_err("malformed head JSON must refuse");
    assert!(
        matches!(
            head_error,
            StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                ..
            }
        ),
        "malformed Postgres head JSON must be stored-data corruption, got {head_error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_persisted_record_decode_classification_checkpoint_when_configured() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres checkpoint decode classification: database URL is not set");
        return;
    };
    let isolated_database = crate::testing::IsolatedDatabase::create(&database_url).await;
    let storage = PostgresStorage::connect(isolated_database.url())
        .await
        .expect("connect persisted-record decode storage");

    let (checkpoint_session_id, checkpoint_store) =
        persisted_record_decode_store(&storage, "checkpoint").await;
    let checkpoint_ref: String =
        sqlx::query_scalar("SELECT checkpoint_ref FROM lash_sessions WHERE session_id = $1")
            .bind(checkpoint_session_id.as_str())
            .fetch_one(storage.pool())
            .await
            .expect("read checkpoint ref");
    assert_eq!(
        sqlx::query("UPDATE lash_blobs SET content = $1 WHERE hash = $2")
            .bind(vec![0xc1_u8])
            .bind(checkpoint_ref)
            .execute(storage.pool())
            .await
            .expect("corrupt checkpoint MessagePack")
            .rows_affected(),
        1
    );
    let checkpoint_error = checkpoint_store
        .load_session_window(
            &checkpoint_session_id,
            lash_core_execution::store::WindowSelector::Current,
        )
        .await
        .expect_err("malformed checkpoint MessagePack must refuse");
    assert!(
        matches!(
            checkpoint_error,
            StoreError::StoredDataCorrupt {
                record_kind: "SessionCheckpoint",
                ..
            }
        ),
        "malformed Postgres checkpoint MessagePack must be stored-data corruption, got {checkpoint_error:?}"
    );
}

#[test]
fn postgres_persisted_record_decode_classification_preserves_version_refusals() {
    let expected = lash_core_execution::store::SESSION_CHECKPOINT_SCHEMA_VERSION;
    let decode = |value: serde_json::Value| {
        let bytes = rmp_serde::to_vec_named(&value).expect("encode version-refusal fixture");
        decode_versioned_msgpack_record::<SessionCheckpoint>(&bytes, "SessionCheckpoint", expected)
            .expect_err("non-current checkpoint version must refuse")
    };

    assert!(matches!(
        decode(serde_json::json!({})),
        StoreError::MissingRecordSchemaVersion {
            record_kind: "SessionCheckpoint",
            expected: actual_expected,
        } if actual_expected == expected
    ));
    assert!(matches!(
        decode(serde_json::json!({"schema_version": "invalid"})),
        StoreError::InvalidRecordSchemaVersion {
            record_kind: "SessionCheckpoint",
            expected: actual_expected,
            ..
        } if actual_expected == expected
    ));
    assert!(matches!(
        decode(serde_json::json!({"schema_version": expected + 1})),
        StoreError::UnsupportedRecordSchemaVersion {
            record_kind: "SessionCheckpoint",
            actual,
            expected: actual_expected,
        } if actual == expected + 1 && actual_expected == expected
    ));
}

#[test]
fn turn_failure_settlement_query_filters_receipts_without_evidence() {
    assert!(
        crate::session_sql::session_sql()
            .turn_commits
            .select_failure_settlements
            .sql()
            .contains("AND failure_evidence"),
        "the SQL path must exclude receipts that cannot carry failure evidence"
    );
}

/// Seed one committed session carrying failure evidence, then splice an extra
/// receipt row into `lash_runtime_turn_commits` under the `bad-evidence-receipt`
/// operation key so a refusal can be asserted against that exact row.
async fn seed_failure_evidence_session(
    session_id: &str,
    bad_result_json: &str,
) -> (PostgresStorage, postgres_test_support::SharedDatabaseLock) {
    let database_url = postgres_test_support::database_url()
        .expect("receipt refusal tests require LASH_POSTGRES_DATABASE_URL");
    let database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect receipt-refusal storage");

    let store = storage.store();
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&SessionId::from(
                session_id,
            )),
        )
        .await
        .expect("bind receipt-refusal session");
    let state = lash_core_execution::RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    let mut commit = lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.failure_evidence = vec![lash_core_execution::TurnFailureEvidence {
        partial_output: Some(lash_core_execution::TurnFailurePartialOutput::Complete {
            text: "settled partial output".to_string(),
        }),
        billed_usage: lash_core_execution::llm::types::LlmUsage {
            output_tokens: 3,
            ..Default::default()
        },
        refusal: lash_core_execution::ChargeSafetyRefusalEvidence {
            code: "unsafe_retry_after_output_started".to_string(),
            denial_reason: lash_core_execution::ChargeSafetyDenialReason::GuaranteeRequired,
            protocol_position: lash_core_execution::ProtocolPosition::OutputStarted,
            attempt_number: 1,
            attempt_count: 1,
        },
    }];
    store
        .commit_runtime_state(commit)
        .await
        .expect("seed one failure-evidence receipt");

    sqlx::query(
        "INSERT INTO lash_runtime_turn_commits
         (session_id, turn_id, turn_commit_hash, result_json, committed_at_ms, failure_evidence)
         SELECT $1, 'bad-evidence-receipt', 'bad-evidence-hash', $2, committed_at_ms + 1, TRUE
         FROM lash_runtime_turn_commits
         WHERE session_id = $1",
    )
    .bind(session_id)
    .bind(bad_result_json)
    .execute(storage.pool())
    .await
    .expect("splice the bad receipt row");
    (storage, database_lock)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_failure_reopen_refuses_one_corrupt_evidence_receipt() {
    const SESSION_ID: &str = "failure-evidence-corrupt-receipt";
    let Some(_) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres receipt refusal: database URL is not set");
        return;
    };
    let (storage, _database_lock) =
        seed_failure_evidence_session(SESSION_ID, r#"{"failure_evidence":"#).await;

    let error = storage
        .store()
        .load_failure_evidence_page(
            &SessionId::from(SESSION_ID),
            None,
            std::num::NonZeroU32::new(100).expect("nonzero page limit"),
        )
        .await
        .expect_err("a corrupt evidence receipt must refuse the page");
    assert!(
        matches!(
            &error,
            StoreError::StoredDataCorrupt {
                record_kind: "RuntimeCommitReceipt",
                message,
            } if message.contains(SESSION_ID) && message.contains("bad-evidence-receipt")
        ),
        "the corrupt receipt refusal must name the session and operation: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_failure_reopen_refuses_a_preversioned_receipt() {
    const SESSION_ID: &str = "failure-evidence-preversioned-receipt";
    let Some(_) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres receipt refusal: database URL is not set");
        return;
    };
    let (storage, _database_lock) =
        seed_failure_evidence_session(SESSION_ID, r#"{"failure_evidence":[{}]}"#).await;

    let error = storage
        .store()
        .load_failure_evidence_page(
            &SessionId::from(SESSION_ID),
            None,
            std::num::NonZeroU32::new(100).expect("nonzero page limit"),
        )
        .await
        .expect_err("an unversioned receipt must refuse the page");
    assert!(
        matches!(
            &error,
            StoreError::MissingRecordSchemaVersion {
                record_kind: "RuntimeCommitReceipt",
                ..
            }
        ),
        "the pre-versioned receipt refusal must be typed: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_failure_reopen_refuses_a_newer_receipt_version() {
    const SESSION_ID: &str = "failure-evidence-newer-receipt";
    let Some(_) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres receipt refusal: database URL is not set");
        return;
    };
    let newer = lash_core_execution::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION + 1;
    let bad_json = format!(r#"{{"schema_version":{newer},"failure_evidence":[{{}}]}}"#);
    let (storage, _database_lock) = seed_failure_evidence_session(SESSION_ID, &bad_json).await;

    let error = storage
        .store()
        .load_failure_evidence_page(
            &SessionId::from(SESSION_ID),
            None,
            std::num::NonZeroU32::new(100).expect("nonzero page limit"),
        )
        .await
        .expect_err("a newer receipt version must refuse the page");
    assert!(
        matches!(
            &error,
            StoreError::UnsupportedRecordSchemaVersion {
                record_kind: "RuntimeCommitReceipt",
                actual,
                expected,
            } if *actual == newer
                && *expected == lash_core_execution::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION
        ),
        "the newer-version receipt refusal must be the typed version error: {error:?}"
    );
}

#[tokio::test]
async fn attachment_unwired_process_registry_factory_warns() {
    let storage = PostgresStorage {
        pool: sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap(),
        catalog_id: Arc::from("unused.public"),
        fleet_format: lash_core_execution::FleetFormat::current(),
    };
    for path in [
        "PostgresStorage::session_store_factory",
        "PostgresStore::new",
    ] {
        let warnings = AttachmentWarnings::default();
        let subscriber = Registry::default().with(warnings.clone());
        tracing::subscriber::with_default(subscriber, || {
            let factory = if path == "PostgresStorage::session_store_factory" {
                storage.session_store_factory()
            } else {
                PostgresStore::new(&storage)
            };
            assert!(
                !lash_core_execution::AttachmentRootSet::can_prove_process_owner_death(&factory)
            );
            let wired = storage.session_store_factory_with_shared_process_registry();
            assert!(lash_core_execution::AttachmentRootSet::can_prove_process_owner_death(&wired));
            let wired = PostgresStore::new_with_shared_process_registry(&storage);
            assert!(lash_core_execution::AttachmentRootSet::can_prove_process_owner_death(&wired));
        });
        let events = warnings.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["store"], "postgres");
        assert_eq!(events[0]["path"], path);
        assert_eq!(
            events[0]["consequence"],
            "process-owned uncommitted intents are never reclaimed"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_session_store_defers_missing_identity_validation() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping direct-session-store contract: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect direct-session-store contract storage");
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_catalog_identity')
         ORDER BY tablename",
    )
    .fetch_all(storage.pool())
    .await
    .expect("list Lash tables for direct-constructor test reset");
    let truncate = format!("TRUNCATE {} RESTART IDENTITY CASCADE", tables.join(", "));
    sqlx::query(&truncate)
        .execute(storage.pool())
        .await
        .expect("reset direct-constructor test tables");
    let missing = SessionId::from("missing");
    let store = storage.store();

    assert!(matches!(
        store.load_session_head_meta(&missing).await,
        Ok(None)
    ));
    assert!(matches!(store.load_session_meta(&missing).await, Ok(None)));
    assert_eq!(
        store
            .admit_session(
                &lash_core_execution::testing::store_fixtures::root_session_request(&missing)
            )
            .await
            .expect("admit missing direct-constructor session"),
        lash_core_execution::SessionAdmission::Created
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bulk_delete_over_fork_lineage_retires_the_same_nodes_in_either_candidate_order() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping bulk-delete candidate-order witness: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect bulk-delete candidate-order witness storage");

    async fn run_fixture(
        storage: &PostgresStorage,
        ancestor_sorts_first: bool,
    ) -> (Vec<&'static str>, std::collections::BTreeSet<&'static str>) {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let ancestor_session = format!(
            "bulk-order:{nonce}:{}-ancestor",
            if ancestor_sorts_first { "a" } else { "z" }
        );
        let child_session = format!(
            "bulk-order:{nonce}:{}-child",
            if ancestor_sorts_first { "z" } else { "a" }
        );
        let node = |role: &str| format!("bulk-order:{nonce}:{role}");
        let nodes = [
            ("ancestor-root", ancestor_session.as_str(), None, 0_i64),
            (
                "ancestor-leaf",
                ancestor_session.as_str(),
                Some("ancestor-root"),
                1_i64,
            ),
            (
                "child-root",
                child_session.as_str(),
                Some("ancestor-root"),
                0_i64,
            ),
            (
                "child-leaf",
                child_session.as_str(),
                Some("child-root"),
                1_i64,
            ),
        ];

        for session_id in [&ancestor_session, &child_session] {
            // Deletion reads the head's current frame to fence it (ADR 0113
            // §3.1), so the witness head is a well-formed frameless one.
            let head_json =
                serde_json::to_string(&lash_core_execution::store::SessionHeadPayload {
                    schema_version: lash_core_execution::store::SESSION_HEAD_META_SCHEMA_VERSION,
                    session_id: SessionId::from(session_id.as_str()),
                    config: lash_core_execution::PersistedSessionConfig::new(
                        lash_core_execution::TurnBudget::Unbounded,
                    ),
                    current_frame_node_id: None,
                })
                .expect("encode witness head");
            sqlx::query(
                "INSERT INTO lash_sessions (session_id, head_json, leaf_node_id)
                 VALUES ($1, $2, NULL)",
            )
            .bind(session_id)
            .bind(head_json)
            .execute(storage.pool())
            .await
            .unwrap_or_else(|error| panic!("seed witness session `{session_id}`: {error}"));
            sqlx::query(
                "INSERT INTO lash_session_meta
                 (session_id, relation_kind)
                 VALUES ($1, 'root')",
            )
            .bind(session_id)
            .execute(storage.pool())
            .await
            .unwrap_or_else(|error| panic!("seed witness metadata `{session_id}`: {error}"));
        }
        for (role, session_id, parent_role, generation) in nodes {
            sqlx::query(
                "INSERT INTO lash_graph_nodes
                 (session_id, node_id, parent_node_id, generation, frame_node_id, node_json, body_bytes)
                 VALUES ($1, $2, $3, $4, $2, '{}', 2)",
            )
            .bind(session_id)
            .bind(node(role))
            .bind(parent_role.map(&node))
            .bind(generation)
            .execute(storage.pool())
            .await
            .unwrap_or_else(|error| panic!("seed witness node `{role}`: {error}"));
        }
        sqlx::query(
            "INSERT INTO lash_fork_lineage
             (session_id, ancestor_session_id, fork_node_id, fork_generation)
             VALUES ($1, $2, $3, 0)",
        )
        .bind(&child_session)
        .bind(&ancestor_session)
        .bind(node("ancestor-root"))
        .execute(storage.pool())
        .await
        .expect("seed witness fork lineage");

        let session_ids = vec![
            SessionId::from(ancestor_session),
            SessionId::from(child_session),
        ];
        let ordered_candidates: Vec<String> = sqlx::query_scalar(
            "SELECT graph.node_id FROM lash_graph_nodes AS graph
             WHERE graph.session_id = ANY($1) AND graph.tombstoned = FALSE
               AND NOT EXISTS (
                   SELECT 1 FROM lash_graph_nodes AS child
                   WHERE child.parent_node_id = graph.node_id
                     AND child.tombstoned = FALSE
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lash_sessions AS head
                   WHERE head.leaf_node_id = graph.node_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lash_node_anchors AS anchor
                   WHERE anchor.node_id = graph.node_id
               )
             ORDER BY graph.session_id, graph.generation DESC",
        )
        .bind(
            session_ids
                .iter()
                .map(SessionId::as_str)
                .collect::<Vec<_>>(),
        )
        .fetch_all(storage.pool())
        .await
        .expect("read witness candidate order");
        let role = |node_id: &str| {
            if node_id.ends_with(":ancestor-root") {
                "ancestor-root"
            } else if node_id.ends_with(":ancestor-leaf") {
                "ancestor-leaf"
            } else if node_id.ends_with(":child-root") {
                "child-root"
            } else if node_id.ends_with(":child-leaf") {
                "child-leaf"
            } else {
                panic!("unknown witness node `{node_id}`")
            }
        };
        let candidate_roles = ordered_candidates
            .iter()
            .map(|node_id| role(node_id))
            .collect::<Vec<_>>();

        let before: std::collections::BTreeSet<String> = sqlx::query_scalar(
            "SELECT node_id FROM lash_graph_nodes WHERE node_id LIKE $1 ORDER BY node_id",
        )
        .bind(format!("bulk-order:{nonce}:%"))
        .fetch_all(storage.pool())
        .await
        .expect("read witness nodes before bulk delete")
        .into_iter()
        .collect();
        let mut tx = storage
            .pool()
            .begin()
            .await
            .expect("begin witness bulk delete");
        crate::session_factory::delete_process_sessions_tx(
            &mut tx,
            &session_ids,
            lash_core_execution::FleetFormat::current(),
        )
        .await
        .expect("bulk delete witness sessions");
        tx.commit().await.expect("commit witness bulk delete");
        let after: std::collections::BTreeSet<String> = sqlx::query_scalar(
            "SELECT node_id FROM lash_graph_nodes WHERE node_id LIKE $1 ORDER BY node_id",
        )
        .bind(format!("bulk-order:{nonce}:%"))
        .fetch_all(storage.pool())
        .await
        .expect("read witness nodes after bulk delete")
        .into_iter()
        .collect();
        let retired = before
            .difference(&after)
            .map(|node_id| role(node_id))
            .collect();
        (candidate_roles, retired)
    }

    let (ancestor_first_candidates, ancestor_first_retired) = run_fixture(&storage, true).await;
    let (child_first_candidates, child_first_retired) = run_fixture(&storage, false).await;
    assert_eq!(
        ancestor_first_candidates,
        ["ancestor-leaf", "child-leaf"],
        "the first fixture must exercise ancestor-session candidate order"
    );
    assert_eq!(
        child_first_candidates,
        ["child-leaf", "ancestor-leaf"],
        "the second fixture must reverse the cross-session candidate order"
    );
    assert_eq!(ancestor_first_retired, child_first_retired);
    assert_eq!(
        ancestor_first_retired,
        ["ancestor-leaf", "ancestor-root", "child-leaf", "child-root"]
            .into_iter()
            .collect(),
        "both candidate orders must retire the complete fork lineage"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_commits_return_one_typed_head_revision_conflict() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping concurrent first-commit proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect concurrent first-commit storage");
    let factory = storage.session_store_factory();
    let session_id = SessionId::from(format!(
        "postgres-first-commit-race:{}",
        uuid::Uuid::new_v4()
    ));
    let request = SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation: lash_core_execution::SessionRelation::Root,
        policy: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };
    factory
        .admit_session(&request)
        .await
        .expect("admit racing session");
    let first_store = factory.clone();
    let second_store = factory.clone();
    let mut first_state = lash_core_execution::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_execution::RuntimeSessionState::new(request.policy.clone())
    };
    first_state.ensure_agent_frame_initialized();
    let second_state = first_state.clone();
    let (first_commit, _) =
        lash_core_execution::RuntimeCommit::persisted_state_for_test(&first_state, &[])
            .with_operation(lash_core_execution::OperationId::turn(
                &session_id,
                "first-racer",
                "final",
            ))
            .expect("build first racing commit");
    let (second_commit, _) =
        lash_core_execution::RuntimeCommit::persisted_state_for_test(&second_state, &[])
            .with_operation(lash_core_execution::OperationId::turn(
                &session_id,
                "second-racer",
                "final",
            ))
            .expect("build second racing commit");
    let start = Arc::new(tokio::sync::Barrier::new(2));
    let first_start = Arc::clone(&start);
    let second_start = Arc::clone(&start);
    let (first, second) = tokio::join!(
        async move {
            first_start.wait().await;
            first_store.commit_runtime_state(first_commit).await
        },
        async move {
            second_start.wait().await;
            second_store.commit_runtime_state(second_commit).await
        }
    );
    let results = [first, second];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(StoreError::HeadRevisionConflict {
                    expected: 0,
                    actual: 1
                })
            ))
            .count(),
        1,
        "the losing first commit must fail with the typed CAS conflict: {results:?}"
    );
}

#[tokio::test]
async fn postgres_graph_generation_uniqueness_is_typed() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping graph-generation error proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect graph-generation error storage");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let session_id = SessionId::from(format!("postgres-generation-collision:{nonce}"));
    let first_node = format!("generation-node-a:{nonce}");
    let second_node = format!("generation-node-b:{nonce}");
    sqlx::query(
        "INSERT INTO lash_graph_nodes
         (session_id, node_id, parent_node_id, generation, frame_node_id, node_json, body_bytes)
         VALUES ($1, $2, NULL, 3, $2, '{}', 2)",
    )
    .bind(session_id.as_str())
    .bind(&first_node)
    .execute(storage.pool())
    .await
    .expect("seed graph-generation uniqueness fixture");
    let raw = sqlx::query(
        "INSERT INTO lash_graph_nodes
         (session_id, node_id, parent_node_id, generation, frame_node_id, node_json, body_bytes)
         VALUES ($1, $2, NULL, 3, $2, '{}', 2)",
    )
    .bind(session_id.as_str())
    .bind(&second_node)
    .execute(storage.pool())
    .await
    .expect_err("duplicate generation must violate Postgres uniqueness");
    let error = graph_node_insert_error(raw, &session_id, 3, &second_node);
    assert!(matches!(
        error,
        StoreError::GraphGenerationCollision {
            session_id: ref actual_session_id,
            generation: 3
        } if actual_session_id == session_id
    ));
    sqlx::query("DELETE FROM lash_graph_nodes WHERE session_id = $1")
        .bind(session_id.as_str())
        .execute(storage.pool())
        .await
        .expect("clean graph-generation uniqueness fixture");
}

#[tokio::test]
async fn postgres_delete_permanently_fences_stale_handles_and_session_id_reuse() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres delete fence proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect delete fence storage");
    let factory = storage.session_store_factory_with_shared_process_registry();
    let session_id = SessionId::from(format!("postgres-delete-fence:{}", uuid::Uuid::new_v4()));
    let request = SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation: lash_core_execution::SessionRelation::Root,
        policy: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };
    factory
        .admit_session(&request)
        .await
        .expect("admit stale session");
    let stale_store = factory.clone();
    let mut state = lash_core_execution::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    state.ensure_agent_frame_initialized();

    factory
        .delete_session(&session_id)
        .await
        .expect("delete before first commit");
    let error = stale_store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect_err("stale first commit must not resurrect the session");
    assert!(matches!(
        error,
        StoreError::SessionDeleted {
            ref session_id
        } if session_id == request.session_id
    ));

    let reuse_error = match factory.admit_session(&request).await {
        Ok(_) => panic!("deleted session id must never be reused"),
        Err(error) => error,
    };
    assert!(matches!(
        reuse_error,
        StoreError::SessionDeleted {
            ref session_id
        } if session_id == request.session_id
    ));
}

lash_conformance::checkpoint_admission_probe_tests!({
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres checkpoint counter: database URL is not set");
        return;
    };
    let database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect checkpoint counter storage");
    let session_id = SessionId::from(format!(
        "postgres-checkpoint-counter:{}",
        std::process::id()
    ));
    let store = Arc::new(storage.store());
    let counting_store = Arc::clone(&store);
    let teardown_session = session_id.clone();
    (
        database_lock,
        store as Arc<dyn lash_core_execution::RuntimeStore>,
        session_id,
        move || counting_store.checkpoint_admission_counts(),
        async move {
            storage
                .session_store_factory()
                .delete_session(&teardown_session)
                .await
                .expect("delete checkpoint counter session");
        },
    )
});

/// Arming a delete and a writer taking the digest back are the two halves of
/// the same CAS: run concurrently against PostgreSQL, at most one of them
/// may win.
///
/// This is the law that catches a transition running bare on the pool
/// instead of inside a transaction under the per-digest advisory key. With
/// `arm_attachment_delete` unfenced, its `UPDATE` can commit *inside* the
/// writer's open transaction — after the writer read `condemned` and before
/// it deleted the row — so the writer erases a `deleting` row, is granted,
/// and puts bytes into a delete that is already in flight. The post-delete
/// probe is no defence against that: it only fires once the bytes are gone.
///
/// Multi-threaded on purpose: the writer and the sweeper half must really
/// interleave, so each runs as its own task while the pool's IO driver keeps
/// running alongside them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arming_a_delete_and_a_concurrent_writer_never_both_win() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres attachment fence race proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect attachment fence database");
    let session_id = SessionId::from(format!(
        "postgres-attachment-fence-race:{}",
        std::process::id()
    ));
    let store = std::sync::Arc::new(storage.store());
    let factory = storage.session_store_factory();
    let attachment_id =
        lash_core_execution::AttachmentId::parse(format!("fence-race-{}", std::process::id()))
            .expect("valid attachment id");
    sqlx::query("DELETE FROM lash_attachment_condemnations WHERE attachment_id = $1")
        .bind(attachment_id.as_str())
        .execute(storage.pool())
        .await
        .expect("clear condemnation fixture");
    let intent = {
        let session_id = session_id.clone();
        let attachment_id = attachment_id.clone();
        move || lash_core_execution::AttachmentIntent {
            attachment_id: attachment_id.clone(),
            session_id: session_id.clone(),
            canonical_uri: format!("lash-attachment://blake3/{attachment_id}"),
            intent_at_epoch_ms: 1,
            owner: None,
        }
    };

    // Widen the writer's read-then-revoke window so the interleaving an
    // unfenced `arm` corrupts is reached on every odd round instead of once
    // in a blue moon. The fence does not care how wide the window is: a
    // concurrent `arm` waits on the per-digest advisory key either way.
    crate::attachments::FENCE_WRITER_WINDOW_DELAY_MS
        .store(20, std::sync::atomic::Ordering::Relaxed);

    // Both orderings, every round: the fixed code holds for all of them.
    for round in 0..12 {
        assert_eq!(
            lash_core_execution::AttachmentRootSet::condemn_attachment(&factory, &attachment_id, 0)
                .await
                .expect("condemn"),
            lash_core_execution::AttachmentCondemnation::Condemned,
            "round {round}: the digest must start each round rootless and free"
        );

        let writer = tokio::spawn({
            let store = std::sync::Arc::clone(&store);
            let intent = intent.clone();
            async move {
                lash_core_execution::AttachmentManifest::begin_attachment_write(&*store, intent())
                    .await
            }
        });
        if round % 2 == 1 {
            // Alternate the stagger so both orderings are exercised: on odd
            // rounds the writer reaches its condemnation read first (the
            // interleaving an unfenced `arm` corrupts), on even rounds the
            // sweeper arms first. The pacing widens a window; it decides no
            // outcome, and the invariant below holds for either ordering.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let armed =
            lash_core_execution::AttachmentRootSet::arm_attachment_delete(&factory, &attachment_id)
                .await
                .expect("arm");
        let fence = writer.await.expect("join writer").expect("fenced write");

        let contains_ref = lash_core_execution::AttachmentManifest::list_all_refs(&*store)
            .await
            .map(|refs| refs.contains(&attachment_id))
            .expect("contains_ref");
        match (armed, fence) {
            // The sweeper won: the delete is armed and the writer parked
            // without recording anything, so no bytes can land inside it.
            (
                lash_core_execution::AttachmentDeleteArming::Armed,
                lash_core_execution::AttachmentWriteFence::ReclamationInFlight,
            ) => {
                assert!(
                    !contains_ref,
                    "round {round}: a parked writer records no intent"
                );
            }
            // The writer won: it took the digest back before the delete was
            // armed, and the sweeper issues no delete at all.
            (
                lash_core_execution::AttachmentDeleteArming::Revoked,
                lash_core_execution::AttachmentWriteFence::Granted(permit),
            ) => {
                assert!(
                    contains_ref,
                    "round {round}: a granted writer records its intent"
                );
                let completed_intent = intent();
                lash_core_execution::AttachmentManifest::complete_attachment_write(
                    &*store,
                    &completed_intent,
                    permit,
                )
                .await
                .expect("complete the winning writer");
            }
            (armed, fence) => panic!(
                "round {round}: arming and the writer must never both win \
                 (arm = {armed:?}, writer = {fence:?}); bytes would land inside an \
                 in-flight delete"
            ),
        }

        lash_core_execution::AttachmentRootSet::release_attachment_condemnation(
            &factory,
            &attachment_id,
        )
        .await
        .expect("release");
        if contains_ref {
            lash_core_execution::AttachmentManifest::forget(&*store, &session_id, &attachment_id)
                .await
                .expect("forget the ref");
        }
    }

    crate::attachments::FENCE_WRITER_WINDOW_DELAY_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    factory
        .delete_session(&session_id)
        .await
        .expect("delete fence session");
}

#[tokio::test]
async fn attachment_gc_refuses_an_empty_postgres_root_database() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping empty Postgres attachment-root proof: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect empty attachment-root database");
    sqlx::query("DELETE FROM lash_attachment_manifest")
        .execute(storage.pool())
        .await
        .expect("make the configured Postgres manifest empty");
    let wrong_factory = storage.session_store_factory();

    let live_backend = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a SQLite memory store set");
    let live_store = live_backend.open_store().await.expect("open live catalog");
    let request = SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("postgres-wrong-database-live-attachment"),
        relation: lash_core_execution::SessionRelation::Root,
        policy: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };
    live_store
        .admit_session(&request)
        .await
        .expect("admit live root authority");
    let blobs = tempfile::tempdir().expect("attachment directory");
    let backend = lash_core_execution::attachments::FileAttachmentStore::new(blobs.path());
    let attachment = lash_core_execution::AttachmentStore::put(
        &backend,
        b"postgres-live-committed-blob".to_vec(),
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
            &*live_store,
            live_intent.clone(),
        )
        .await
        .expect("begin live attachment write")
    else {
        panic!("a free digest must grant its writer");
    };
    lash_core_execution::AttachmentManifest::complete_attachment_write(
        &*live_store,
        &live_intent,
        live_permit,
    )
    .await
    .expect("stamp live attachment upload");
    lash_core_execution::AttachmentManifest::commit_refs(
        &*live_store,
        &request.session_id,
        std::slice::from_ref(&attachment.id),
    )
    .await
    .expect("commit live attachment ref");

    let result = lash_core_execution::attachments::reclaim_unreferenced_attachments(
        &wrong_factory,
        &backend,
        lash_core_execution::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: lash_core_execution::EmptyRootSetPolicy::Refuse,
        },
    )
    .await;

    let failure = result.expect_err("an empty Postgres root database must refuse deletion");
    assert_eq!(
        failure.refusal(),
        Some(&lash_core_execution::MaintenanceRefusal::EmptyRootSetUnauthorized),
        "an empty Postgres root database must refuse deletion: {failure:?}"
    );
    assert_eq!(
        failure.partial.scanned_blob_count, 1,
        "the refusal must carry the report accumulated before it: {failure:?}"
    );
    lash_core_execution::AttachmentStore::get(&backend, &attachment.id)
        .await
        .expect("live committed blob survives the refused sweep");
}

#[derive(Clone, Default)]
struct AttachmentWarnings(Arc<std::sync::Mutex<Vec<std::collections::BTreeMap<String, String>>>>);
impl<S: tracing::Subscriber> Layer<S> for AttachmentWarnings {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        #[derive(Default)]
        struct Fields(std::collections::BTreeMap<String, String>);
        impl tracing::field::Visit for Fields {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

/// A session over `storage` with a committed head and one next-turn input
/// admitted to `root`: the fixture the settlement laws below start from.
async fn admitted_input_fixture(
    storage: &PostgresStorage,
    label: &str,
    root: &TurnId,
) -> (
    PostgresStore,
    lash_core_execution::store::DriveFence,
    lash_core_execution::RuntimeSessionState,
    lash_core_execution::store::RootAdmission,
) {
    let session_id = SessionId::from(format!("{label}:{}", uuid::Uuid::new_v4()));
    let store = storage.store();
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&session_id),
        )
        .await
        .expect("admit the fixture session");
    let mut state = lash_core_execution::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    let seeded = store
        .commit_runtime_state(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[]),
        )
        .await
        .expect("seed the fixture head");
    state.head_revision = seeded.head_revision;
    let input = store
        .enqueue_pending_turn_input(lash_core_execution::PendingTurnInputDraft::new(
            &session_id,
            lash_core_execution::TurnInputIngress::NextTurn,
            lash_core_execution::TurnInput::text("the admitted input"),
        ))
        .await
        .expect("enqueue the fixture input");
    let fence = store
        .seal_drive_epoch_for_test(
            &session_id,
            &LeaseOwnerIdentity::opaque(format!("{label}-owner"), format!("{label}-incarnation")),
            &format!("{label}-executor"),
            60_000,
        )
        .await
        .expect("seal the fixture drive")
        .acquired()
        .expect("the fixture drive is sealed");
    let admission = store
        .admit_root(
            &lash_core_execution::testing::store_fixtures::admit_root_request_for_test(
                &fence,
                root,
                lash_core_execution::store::AdmittedHead::Input(input.input_id),
            ),
        )
        .await
        .expect("admit the fixture root")
        .expect("the fixture root takes its input");
    (store, fence, state, admission)
}

/// A settlement locks the row it settles: the verdict reads the row under
/// `FOR UPDATE`, so no concurrent rebind can move it between that read and
/// the settling write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_settlement_locks_the_admitted_row() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres settlement row lock: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect settlement-lock storage");
    let root = TurnId::from("settlement-lock-root");
    let (_store, _fence, state, admission) =
        admitted_input_fixture(&storage, "postgres-settlement-lock", &root).await;
    let input_id = admission.input_ids()[0].clone();

    let mut settling = storage.pool().begin().await.expect("begin settling tx");
    sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs_postgres
            .select_by_id_for_update
            .sql(),
    )
    .bind(state.session_id.as_str())
    .bind(input_id.as_str())
    .fetch_one(&mut *settling)
    .await
    .expect("lock the admitted row");
    let mut rebinder = storage.pool().begin().await.expect("begin rebinding tx");
    sqlx::query("SET LOCAL lock_timeout = '50ms'")
        .execute(&mut *rebinder)
        .await
        .expect("bound the rebinder's lock wait");
    let blocked = sqlx::query(
        "UPDATE lash_pending_turn_inputs SET admitted_root = 'another-root'
         WHERE session_id = $1 AND input_id = $2",
    )
    .bind(state.session_id.as_str())
    .bind(input_id.as_str())
    .execute(&mut *rebinder)
    .await
    .expect_err("the settlement's row lock must block a rebind");
    assert_eq!(
        blocked.as_database_error().and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("55P03")),
        "the rebind must fail specifically on the held row lock: {blocked}"
    );
    settling
        .rollback()
        .await
        .expect("release the settling lock");
    rebinder
        .rollback()
        .await
        .expect("roll back the timed-out rebind");
}

/// FIG-3927: the settlement verdict runs first, and it — not the write's
/// rows-affected — is what refuses a row another root holds.
///
/// The two paths are distinguishable in the error itself. The verdict reads
/// the locked row, so its `IngressRowNotAdmitted` names the root that holds
/// the row. The write-only backstop has no row to read, so its refusal
/// carries `None`. Asserting the populated field is therefore proof of
/// ordering, not just of refusal. The refused commit moves no head.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_settlement_verdict_decides_before_the_settlement_write() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres settlement-order law: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect settlement-order storage");
    let root = TurnId::from("settlement-order-root");
    let (store, fence, state, admission) =
        admitted_input_fixture(&storage, "postgres-settle-order", &root).await;
    let input_id = admission.input_ids()[0].clone();
    // A fork rebinds a row to its own root; the commit of the root that
    // admitted it then names a row it no longer holds.
    sqlx::query(
        "UPDATE lash_pending_turn_inputs SET admitted_root = 'rebinding-root'
         WHERE session_id = $1 AND input_id = $2",
    )
    .bind(state.session_id.as_str())
    .bind(input_id.as_str())
    .execute(storage.pool())
    .await
    .expect("rebind the admitted row");

    let mut settlement = lash_core_execution::store::IngressSettlement::new(root.clone());
    settlement
        .completed_inputs
        .extend(admission.inputs.as_ref().map(|inputs| inputs.completion()));
    let error = store
        .commit_runtime_state(
            lash_core_execution::testing::store_fixtures::settling_commit_for_test(
                lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[]),
                &fence,
                settlement,
            ),
        )
        .await
        .expect_err("the verdict must refuse a row another root holds before any write");
    let StoreError::IngressRowNotAdmitted {
        root: ref refused_root,
        ref admitted_root,
        ..
    } = error
    else {
        panic!("unexpected variant: {error:?}");
    };
    assert_eq!(*refused_root, root);
    assert_eq!(
        admitted_root.as_ref().map(TurnId::as_str),
        Some("rebinding-root"),
        "the refusal must name the root the locked read observed, which only the verdict can see"
    );
    let head_revision = store
        .load_session_head_meta(&state.session_id)
        .await
        .expect("read the head after the refusal")
        .expect("the head exists")
        .head_revision;
    assert_eq!(
        head_revision, state.head_revision,
        "a refused settlement must not move the session head"
    );
}

/// FIG-4044: an admission holds the drive fence it checked until it commits.
///
/// A seal raises the epoch on the session's `session_meta` row. Under `READ
/// COMMITTED` a plain fence read sees the epoch the seal has not committed
/// yet, so an admission racing the seal would bind rows under a fence the
/// seal makes stale the moment it commits. The fence read locks the row: the
/// admission waits for the in-flight seal, reads the epoch it committed, and
/// is refused, binding nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_checkpoint_admission_holds_its_fence_against_a_concurrent_seal() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres admission fence lock: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect admission-fence storage");
    let root = TurnId::from("admission-fence-root");
    let (store, fence, state, _admission) =
        admitted_input_fixture(&storage, "postgres-admission-fence", &root).await;
    let input = store
        .enqueue_pending_turn_input(lash_core_execution::PendingTurnInputDraft::new(
            &state.session_id,
            lash_core_execution::TurnInputIngress::active_turn(
                root.clone(),
                lash_core_execution::TurnInputCheckpointBoundary::AfterWork,
            ),
            lash_core_execution::TurnInput::text("the checkpoint input"),
        ))
        .await
        .expect("enqueue the checkpoint input");

    // A seal in flight: the epoch is raised on the row and not yet committed.
    let mut sealing = storage.pool().begin().await.expect("begin the seal");
    let raised = sqlx::query(
        "UPDATE lash_session_meta SET drive_epoch = drive_epoch + 1 WHERE session_id = $1",
    )
    .bind(state.session_id.as_str())
    .execute(&mut *sealing)
    .await
    .expect("raise the drive epoch")
    .rows_affected();
    assert_eq!(raised, 1, "the seal raises the session's epoch");

    let request = lash_core_execution::store::CheckpointAdmissionRequest {
        fence,
        root: root.clone(),
        turn_id: root.clone(),
        checkpoint: lash_core_execution::CheckpointKind::AfterWork,
        step: "admission-fence-root:checkpoint".to_string(),
        max_inputs: 10,
        policy: lash_core_execution::testing::queued_work_admission_policy(10),
    };
    let admitting = tokio::spawn(async move { store.admit_at_checkpoint(&request).await });
    // Wait until the admission has either finished, having read the epoch
    // the seal has not committed, or is blocked on the seal's row lock.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if admitting.is_finished() {
            break;
        }
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity
             WHERE datname = current_database() AND pid <> pg_backend_pid()
               AND wait_event_type = 'Lock' AND query LIKE '%drive_epoch%'",
        )
        .fetch_one(storage.pool())
        .await
        .expect("read lock waits");
        if waiting > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the admission neither finished nor waited on the seal"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    sealing.commit().await.expect("commit the seal");
    let admitted = admitting.await.expect("join the admission");
    assert!(
        matches!(admitted, Err(StoreError::StaleDriveFence { .. })),
        "an admission racing a seal is refused once the seal commits, got {admitted:?}"
    );
    let bound: Option<String> = sqlx::query_scalar(
        "SELECT admitted_root FROM lash_pending_turn_inputs
         WHERE session_id = $1 AND input_id = $2",
    )
    .bind(state.session_id.as_str())
    .bind(input.input_id.as_str())
    .fetch_one(storage.pool())
    .await
    .expect("read the input's binding");
    assert_eq!(bound, None, "the refused admission binds nothing");
}

/// The per-operation PostgreSQL round trips, counted by normalized statement
/// text in `pg_stat_statements` (FIG-3412).
///
/// A statement-count pin is the only honest shape for this measurement: the
/// ticket's budget is per *statement name*, so a wall-clock or total-row probe
/// would pass while an extra round trip slipped in. The expected map is the
/// production count plus the `current_setting` probe the testing build runs
/// inside the admission transaction — `#[cfg(test)]` compiles them in here
/// exactly as the `testing` feature does for the integration targets.
async fn postgres_statement_calls_by_name(
    pool: &sqlx::PgPool,
) -> std::collections::BTreeMap<&'static str, i64> {
    let mut calls_by_name = std::collections::BTreeMap::new();
    // `pg_stat_statements` is database-scoped, not connection-scoped: on a
    // shared server the autovacuum daemon's work on this freshly created
    // database (`autovacuum: ANALYZE public.lash_*`) lands in a measured
    // window alongside the operation's own statements. Those rows are the
    // daemon's, not the operation's, so they are excluded here rather than
    // classified — the pin counts what the client connection issued.
    for (query, calls) in sqlx::query_as::<_, (String, i64)>(
        "SELECT query, calls
         FROM pg_stat_statements
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
           AND query NOT LIKE '%pg_stat_statements%'
           AND query NOT LIKE 'autovacuum:%'",
    )
    .fetch_all(pool)
    .await
    .expect("read PostgreSQL statement statistics")
    {
        *calls_by_name
            .entry(postgres_statement_name(&query))
            .or_default() += calls;
    }
    calls_by_name
}

/// The stable name a `pg_stat_statements` row is pinned under. Any statement
/// outside this catalogue is reported as `unrecognized` so the pin fails on a
/// new round trip instead of silently absorbing it.
fn postgres_statement_name(query: &str) -> &'static str {
    let collapsed: String = query.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.as_str() {
        "BEGIN" => "begin",
        "COMMIT" => "commit",
        // pg_stat_statements may report the literal text or the parameterised
        // form (`current_setting($1,$2)`); match on the function shape instead.
        q if q.starts_with("SELECT NULLIF(current_setting(") => "testing-lease-epoch-probe",
        q if q.starts_with("SELECT floor(extract(") && q.contains("transaction_timestamp()") => {
            "txn-clock-ms"
        }
        q if q.starts_with("SELECT pg_advisory_xact_lock(") => "advisory-lock",
        q if q.starts_with("SELECT drive_epoch, drive_admission_id") => "drive-epoch-read",
        q if q.contains("FROM lash_pending_turn_inputs") => "pending-inputs-lock",
        q if q.starts_with("UPDATE lash_pending_turn_inputs") => "pending-input-admit",
        // pg_stat_statements stores this statement's own text when the row is
        // created by a cached-plan execution, but its constant-normalized form
        // (`SELECT EXISTS( SELECT $2 FROM lash_deleted_sessions ...)`) when an
        // in-window re-analysis — e.g. after an autovacuum ANALYZE invalidated
        // the cached plan — creates the row first. Match the relation the
        // check is defined over; the `UNION ALL` exclusion keeps the
        // admission-path materialized-or-deleted probe out of this name.
        q if q.starts_with("SELECT EXISTS(")
            && q.contains("FROM lash_deleted_sessions")
            && !q.contains("UNION") =>
        {
            "deleted-session-check"
        }
        // The commit's admission probe: the session has a head or catalog
        // row. The fork path's probe that also asks about tombstones stays
        // out of this name.
        q if q.starts_with("SELECT EXISTS(")
            && q.contains("FROM lash_session_meta")
            && q.contains("UNION")
            && !q.contains("lash_deleted_sessions") =>
        {
            "session-admitted-check"
        }
        q if q.starts_with("SELECT pending_follow_on_json FROM lash_sessions") => {
            "pending-follow-on-read"
        }
        q if q.starts_with("SELECT head_json, head_revision") => "head-load",
        q if q.starts_with("SELECT head_revision") => "head-lock",
        q if q.starts_with("SELECT node_id FROM lash_graph_nodes") => "graph-nodes-exist",
        q if q.starts_with("SELECT hash FROM lash_blobs") => "blob-lock",
        // The receipt read carries the committing turn's park clear as a
        // data-modifying `WITH` (FIG-3586): one round trip, not two.
        q if q.starts_with("WITH settled_park AS ( DELETE FROM lash_turn_parks")
            && q.contains("SELECT turn_commit_hash, result_json") =>
        {
            "turn-commit-load"
        }
        q if q.starts_with("INSERT INTO lash_blobs") => "blob-insert",
        q if q.starts_with("INSERT INTO lash_checkpoint_blob_refs") => {
            "checkpoint-blob-refs-insert"
        }
        q if q.starts_with("INSERT INTO lash_runtime_turn_commits") => "turn-commit-insert",
        q if q.starts_with("INSERT INTO lash_session_meta") => "session-meta-insert",
        q if q.starts_with("INSERT INTO lash_sessions") => "head-upsert",
        q if q.starts_with("UPDATE lash_attachment_manifest") => "attachment-manifest-commit",
        q if q.starts_with("SELECT admission_json FROM lash_session_roots") => {
            "root-admission-read"
        }
        q if q.starts_with("SELECT root, admission_json FROM lash_session_roots") => {
            "unfinished-root-read"
        }
        q if q.starts_with("SELECT session_state_version FROM lash_session_meta") => {
            "session-state-version-read"
        }
        q if q.starts_with("UPDATE lash_session_meta SET admission_base_checkpoint_ref") => {
            "admission-base-retain"
        }
        q if q.starts_with("SELECT root FROM lash_session_root_inputs") => "root-binding-read",
        q if q.starts_with("INSERT INTO lash_session_roots") => "root-open",
        q if q.starts_with("INSERT INTO lash_session_root_inputs") => "root-input-bind",
        q if q.starts_with("UPDATE lash_session_roots SET admission_json") => {
            "root-admission-write"
        }
        q if q.starts_with("UPDATE lash_session_meta") => "session-meta-touch",
        _ => "unrecognized",
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_admission_and_head_commit_round_trips_are_pinned() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping statement round-trip pin: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let isolated_database = crate::testing::IsolatedDatabase::create(&database_url).await;
    // One pooled connection, opened before the first measurement and reused by
    // every statement after it. A pool free to grow may open a fresh connection
    // inside a measured window, because a released connection returns to the
    // pool asynchronously. That connection's `after_connect` `set_config` calls
    // then count against the operation, and its empty statement cache re-parses
    // each statement, so `pg_stat_statements` records the constant-normalized
    // text (`SELECT $2 FROM lash_deleted_sessions ...`) that the name catalogue
    // does not know. Both depend on scheduling rather than on the operation.
    let storage = PostgresStorage::connect_with(
        isolated_database.url(),
        PostgresStoreConfig {
            max_connections: 1,
            min_connections: 1,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("connect statement round-trip pin storage");
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .execute(storage.pool())
        .await
        .expect("enable pg_stat_statements for the round-trip pin");

    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let session_id = SessionId::from(format!("statement-pin-session:{nonce}"));
    let store = storage.store();
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&session_id),
        )
        .await
        .expect("admit statement-pin session");
    let lease = store
        .seal_drive_epoch_for_test(
            &session_id,
            &LeaseOwnerIdentity::opaque(
                "statement-pin-owner",
                format!("statement-pin-owner:{nonce}"),
            ),
            "statement-pin-executor",
            60_000,
        )
        .await
        .expect("seal statement-pin session drive")
        .acquired()
        .expect("statement-pin drive sealed");
    let input = store
        .enqueue_pending_turn_input(lash_core_execution::PendingTurnInputDraft::new(
            &session_id,
            lash_core_execution::TurnInputIngress::NextTurn,
            lash_core_execution::TurnInput::text("statement-pin input"),
        ))
        .await
        .expect("enqueue statement-pin input");
    // Seed a committed head so the measured commit is the steady-state write
    // path the production number describes, not the first-commit arm.
    let mut state = lash_core_execution::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    let (seed_commit, _) =
        lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[])
            .with_operation(lash_core_execution::OperationId::turn(
                &session_id,
                "statement-pin-seed",
                "final",
            ))
            .expect("build statement-pin seed commit");
    let seed_receipt = store
        .commit_runtime_state(seed_commit)
        .await
        .expect("seed statement-pin head");

    sqlx::query(
        "SELECT pg_stat_statements_reset(0, (SELECT oid FROM pg_database WHERE datname = current_database()), 0)",
    )
        .execute(storage.pool())
        .await
        .expect("reset statement statistics before the admission measurement");
    let admission = store
        .admit_root(
            &lash_core_execution::testing::store_fixtures::admit_root_request_for_test(
                &lease,
                &TurnId::from("statement-pin-root"),
                lash_core_execution::store::AdmittedHead::Input(input.input_id),
            ),
        )
        .await
        .expect("admit statement-pin root")
        .expect("statement-pin input is admissible");
    assert_eq!(admission.input_ids().len(), 1);
    let admission_statements = postgres_statement_calls_by_name(storage.pool()).await;
    // AdmitRoot is one write transaction: it checks the drive fence, reads
    // any recorded admission and the pending follow-on, composes and binds
    // the rows, retains the admission base and records the root. Binding the
    // input to its root checks the input's existing binding, opens the root's
    // row and writes the binding.
    assert_eq!(
        admission_statements,
        std::collections::BTreeMap::from([
            ("begin", 1),
            ("commit", 1),
            ("testing-lease-epoch-probe", 1),
            ("drive-epoch-read", 1),
            ("root-admission-read", 1),
            ("pending-follow-on-read", 1),
            ("unfinished-root-read", 1),
            ("txn-clock-ms", 1),
            ("pending-inputs-lock", 1),
            ("pending-input-admit", 1),
            ("session-state-version-read", 1),
            ("admission-base-retain", 1),
            ("root-binding-read", 1),
            ("root-open", 1),
            ("root-input-bind", 1),
            ("root-admission-write", 1),
        ]),
        "admission round trips changed",
    );

    sqlx::query(
        "SELECT pg_stat_statements_reset(0, (SELECT oid FROM pg_database WHERE datname = current_database()), 0)",
    )
        .execute(storage.pool())
        .await
        .expect("reset statement statistics before the head-commit measurement");
    state.head_revision = seed_receipt.head_revision;
    let (measured_commit, _) =
        lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[])
            .with_operation(lash_core_execution::OperationId::turn(
                &session_id,
                "statement-pin-commit",
                "final",
            ))
            .expect("build statement-pin commit");
    store
        .commit_runtime_state(measured_commit)
        .await
        .expect("measured statement-pin commit");
    let commit_statements = postgres_statement_calls_by_name(storage.pool()).await;
    // The pending follow-on read (ADR 0101 §3, FIG-3542) adds one read to the
    // previous 16-round-trip head commit: the head-write invariant decides
    // against the locked fact. The commit refuses a session the catalog never
    // admitted (ADR 0112) with one probe, where it used to insert the meta row.
    // This fixture does not pass through the testing lease-epoch probe.
    let expected_commit: std::collections::BTreeMap<&'static str, i64> =
        std::collections::BTreeMap::from([
            ("begin", 1),
            ("commit", 1),
            ("advisory-lock", 1),
            ("deleted-session-check", 1),
            ("head-lock", 1),
            ("head-load", 1),
            ("pending-follow-on-read", 1),
            ("turn-commit-load", 1),
            ("graph-nodes-exist", 1),
            ("blob-lock", 1),
            ("blob-insert", 1),
            ("checkpoint-blob-refs-insert", 1),
            ("turn-commit-insert", 1),
            ("session-admitted-check", 1),
            ("head-upsert", 1),
            ("attachment-manifest-commit", 1),
            ("session-meta-touch", 1),
        ]);
    assert_eq!(
        commit_statements, expected_commit,
        "head-commit round trips changed",
    );
}

/// The process-prune batch delete parks a `Cancelled{SessionDeleted}` event
/// per deleted park through one clock bump: `first_seq + row_number - 1`
/// must hand each event a distinct, contiguous sequence (FIG-3659).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_batch_session_delete_writes_one_cancel_event_per_park() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping batch-delete park feed test: database URL is not set");
        return;
    };
    let isolated_database = crate::testing::IsolatedDatabase::create(&database_url).await;
    let storage = PostgresStorage::connect(isolated_database.url())
        .await
        .expect("connect batch-delete storage");
    let factory = storage.session_store_factory();
    let nonce = uuid::Uuid::new_v4();

    let mut session_ids = Vec::new();
    for label in ["batch-park-a", "batch-park-b"] {
        let session_id = SessionId::from(format!("{label}:{nonce}"));
        let request = SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core_execution::SessionRelation::Root,
            policy: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
            ),
        };
        factory
            .admit_session(&request)
            .await
            .expect("admit the parked session");
        let store = factory.clone();
        let state = lash_core_execution::RuntimeSessionState {
            session_id: session_id.clone(),
            ..lash_core_execution::RuntimeSessionState::new(request.policy.clone())
        };
        store
            .commit_runtime_state(
                lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[]),
            )
            .await
            .expect("seed the parked session");
        store
            .record_turn_park(&lash_core_execution::store::TurnParkWrite {
                session_id: session_id.clone(),
                turn_id: lash_core_execution::TurnId::from(format!("{label}-turn")),
                reason: lash_core_execution::store::ParkReason::ReplayDivergence {
                    message: format!("{label} diverged"),
                },
                at_ms: 1_700_000_000_000,
                engine: None,
                after_redrive: None,
                build_generation: None,
            })
            .await
            .expect("park the session's turn");
        session_ids.push(session_id);
    }

    let before = factory
        .turn_park_feed(
            lash_core_execution::store::ParkFeedCursor::initial(),
            std::num::NonZeroUsize::new(100).expect("a nonzero page size"),
        )
        .await
        .expect("read the feed before the batch delete");
    let head = before
        .events
        .last()
        .expect("the two parks precede the delete")
        .seq;

    let mut tx = storage
        .pool()
        .begin()
        .await
        .expect("begin the batch delete");
    crate::session_factory::delete_process_sessions_tx(
        &mut tx,
        &session_ids,
        lash_core_execution::FleetFormat::current(),
    )
    .await
    .expect("batch delete the parked sessions");
    tx.commit().await.expect("commit the batch delete");

    let after = factory
        .turn_park_feed(
            lash_core_execution::store::ParkFeedCursor::from_store_sequence(head),
            std::num::NonZeroUsize::new(100).expect("a nonzero page size"),
        )
        .await
        .expect("read the feed after the batch delete");
    assert_eq!(
        after.events.len(),
        2,
        "one Cancelled event per deleted park: {:?}",
        after.events
    );
    assert_eq!(
        after.events[0].seq + 1,
        after.events[1].seq,
        "the batch's event sequences are contiguous"
    );
    for (event, session_id) in after.events.iter().zip(session_ids.iter()) {
        assert_eq!(
            event.kind,
            lash_core_execution::store::ParkEventKind::Cancelled {
                cause: lash_core_execution::store::ParkCancelCause::SessionDeleted,
            },
            "each deleted park closes as session-deleted"
        );
        assert_eq!(&event.target.session_id, session_id);
    }
}
