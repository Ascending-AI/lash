use super::*;
use lash_core_execution::FleetFormat;
use lash_core_execution::store::{TurnInputStore as _, WindowSelector};

fn body_reads(window: &lash_core_execution::facade_support::sql::Window) -> u64 {
    window
        .snapshot()
        .shapes
        .iter()
        .filter(|(sql, _)| {
            sql.starts_with("select stored.hash,") && sql.contains("blobs as stored")
        })
        .map(|(_, work)| work.statements)
        .sum()
}

/// IMMUTABLE-CONFIG-REUSE: unchanged committed configuration needs no body
/// reads in admission, head commit, or a current/terminal history snapshot.
#[tokio::test]
async fn unchanged_configuration_is_not_rehydrated_by_transactional_or_history_reads() {
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("store");
    let session = SessionId::from("transaction-config-reuse");
    let state = durable_state(&store, &session).await;
    let head = store
        .load_session_head_meta(&session)
        .await
        .expect("head")
        .expect("present");
    let terminal_session = session.clone();
    let revision = head.head_revision;
    store.conn.write(move |tx| {
        tx.execute("INSERT INTO session_runs (session_id, run, terminal_kind, terminal_cause_json, terminal_at_ms, terminal_head_revision, terminal_window_revision) VALUES (?1, 'config-terminal', 'answered', '{\"cause\":\"commands_applied\"}', 42, ?2, ?2)", params![terminal_session.as_str(), revision])?;
        Ok(())
    }).await.expect("terminal at the retained revision");
    let window =
        lash_core_execution::facade_support::sql::Window::new("transaction-config-reuse", "sqlite");
    window
        .within(async {
            let current = store
                .load_session_window(&session, WindowSelector::Current)
                .await
                .expect("window")
                .expect("present");
            assert_eq!(current.config, head.config);
            let terminal = store
                .load_session_window(
                    &session,
                    WindowSelector::Terminal(lash_vocabulary::identity::TurnId::from(
                        "config-terminal",
                    )),
                )
                .await
                .expect("terminal window")
                .expect("present");
            assert_eq!(terminal.config, head.config);
            let revisions = store
                .revisions_in_catalog(&session)
                .await
                .expect("retained revisions");
            assert!(
                revisions
                    .iter()
                    .all(|revision| revision.config == head.config)
            );
            let batch = lash_core_execution::PendingTurnInputBatch::new(
                session.clone(),
                vec![
                    lash_core_execution::PendingTurnInputDraft::new(
                        session.clone(),
                        lash_core_execution::TurnInputIngress::NextTurn,
                        lash_core_execution::TurnInput::text("hello"),
                    )
                    .with_source_key("input:reuse"),
                ],
            )
            .expect("batch");
            let lash_core_execution::store::TurnInputAdmission::Fused { committed_head, .. } =
                store
                    .admit_pending_turn_inputs(batch)
                    .await
                    .expect("admission")
            else {
                panic!("fused admission")
            };
            assert_eq!(committed_head.expect("head").config, head.config);
            store
                .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
                .await
                .expect("commit");
        })
        .await;
    assert_eq!(
        body_reads(&window),
        0,
        "validated unchanged configuration is reused inside snapshots and writes"
    );
}

/// CONFIG-CORRUPTION-REFUSAL: cached material cannot change a row's session
/// identity, and cold transactional/history readers validate graph bodies.
#[tokio::test]
async fn configuration_reuse_keeps_history_and_admission_refusals_typed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("refused-config.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("store");
    let owner = SessionId::from("config-owner");
    let foreign = SessionId::from("config-foreign");
    let owner_state = durable_state(&store, &owner).await;
    let foreign_state = durable_state(&store, &foreign).await;
    store
        .load_session_head_meta(&owner)
        .await
        .expect("cache owner")
        .expect("head");
    let raw = Connection::open(&path).expect("raw connection");
    let root: String = raw
        .query_row(
            "SELECT lower(hex(config_ref)) FROM session_revisions WHERE session_id = ?1",
            [owner.as_str()],
            |row| row.get(0),
        )
        .expect("root");
    raw.execute(
        "UPDATE session_revisions SET config_ref = unhex(?1) WHERE session_id = ?2",
        params![root, foreign.as_str()],
    )
    .expect("foreign identity");
    let error = store
        .load_session_window(&foreign, WindowSelector::Current)
        .await
        .expect_err("foreign history");
    assert!(
        matches!(error, StoreError::StoredDataCorrupt { record_kind: "SessionHeadMeta", message } if message.contains("row is keyed on session `config-foreign`"))
    );
    let error = store
        .admit_pending_turn_inputs(batch(&foreign))
        .await
        .expect_err("foreign admission");
    assert!(
        matches!(error, StoreError::StoredDataCorrupt { record_kind: "SessionHeadMeta", message } if message.contains("row is keyed on session `config-foreign`"))
    );
    let error = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&foreign_state))
        .await
        .expect_err("foreign commit");
    assert!(
        matches!(error, StoreError::StoredDataCorrupt { record_kind: "SessionHeadMeta", message } if message.contains("row is keyed on session `config-foreign`"))
    );
    assert!(
        store
            .list_pending_turn_inputs(&foreign)
            .await
            .expect("rolled back admission")
            .is_empty()
    );
    raw.execute(
        "UPDATE blobs SET content = CAST('{' AS BLOB) WHERE hash = unhex(?1)",
        [root],
    )
    .expect("corrupt body");
    let cold = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("cold reader");
    let history = cold
        .load_session_window(&owner, WindowSelector::Current)
        .await
        .expect_err("cold history");
    let admission = cold
        .admit_pending_turn_inputs(batch(&owner))
        .await
        .expect_err("cold admission");
    let commit = cold
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&owner_state))
        .await
        .expect_err("cold commit");
    for error in [history, admission, commit] {
        assert!(matches!(
            error,
            StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                ..
            }
        ));
    }
    assert!(
        cold.list_pending_turn_inputs(&owner)
            .await
            .expect("rolled back admission")
            .is_empty()
    );
}

fn batch(session: &SessionId) -> lash_core_execution::PendingTurnInputBatch {
    lash_core_execution::PendingTurnInputBatch::new(
        session.clone(),
        vec![
            lash_core_execution::PendingTurnInputDraft::new(
                session.clone(),
                lash_core_execution::TurnInputIngress::NextTurn,
                lash_core_execution::TurnInput::text("hello"),
            )
            .with_source_key("input:refusal"),
        ],
    )
    .expect("batch")
}

/// CONFIG-FLEET-ADMISSION: a snapshot cache hit re-admits the recorded stamp
/// under the caller's fleet, rather than the fleet that populated the cache.
#[tokio::test]
async fn a_cached_configuration_is_readmitted_under_the_snapshot_fleet() {
    use lash_core_store::store::WriterPin;
    const PINS: &[WriterPin] = &[WriterPin {
        constant: "SESSION_HEAD_META_SCHEMA_VERSION",
        generation: 1,
        version: 7,
    }];
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("store");
    let session = SessionId::from("fleet-config");
    durable_state(&store, &session).await;
    let mut payload = store
        .load_session_head_meta(&session)
        .await
        .expect("head")
        .expect("present")
        .payload();
    payload.schema_version = 7;
    let json = serde_json::to_string(&payload).expect("payload");
    let root = store.conn.write(move |tx| {
        crate::revisions::record_revision_conn(tx, &session, 999, None, None, &json).map_err(crate::sqlite_conversion_error)?;
        tx.query_row("SELECT lower(hex(config_ref)) FROM session_revisions WHERE session_id = ?1 AND head_revision = 999", [session.as_str()], |row| row.get::<_, String>(0))
    }).await.expect("fixture stamped by another fleet");
    let configs = Arc::clone(&store.configs);
    store
        .read_connection()
        .read(move |tx| {
            let admitted = crate::codec::load_config_conn(
                tx,
                Some(&root),
                FleetFormat::from_version(1).with_writer_pins(PINS),
                Some(crate::codec::ConfigRead::Committed(&configs)),
            )
            .expect("fleet admitting stamp 7");
            assert_eq!(admitted.schema_version, 7);
            let error = crate::codec::load_config_conn(
                tx,
                Some(&root),
                FleetFormat::current(),
                Some(crate::codec::ConfigRead::Committed(&configs)),
            )
            .expect_err("new fleet refuses cached stamp");
            assert!(matches!(
                error,
                StoreError::UnsupportedRecordSchemaVersion {
                    record_kind: "SessionHeadMeta",
                    actual: 7,
                    ..
                }
            ));
            Ok(())
        })
        .await
        .expect("snapshot fleet admission");
}

// SNAPSHOT-CONFIG-VISIBILITY: neither a newer cached root nor a rolled-back
// write can supply material absent from the caller's snapshot.
#[tokio::test]
async fn configuration_reuse_keeps_snapshot_and_rollback_isolation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("snapshot-config.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("store");
    let session = SessionId::from("snapshot-config");
    let mut state = durable_state(&store, &session).await;
    let first = store
        .load_session_head_meta(&session)
        .await
        .expect("head")
        .expect("present");
    let mut snapshot = Connection::open(&path).expect("snapshot connection");
    let tx = snapshot.transaction().expect("read transaction");
    let before = crate::codec::try_load_session_head_meta_from_conn(
        &tx,
        &session,
        FleetFormat::current(),
        Some(crate::codec::ConfigRead::Committed(&store.configs)),
    )
    .expect("old head")
    .expect("present");
    assert_eq!(before.config, first.config);
    state.policy.max_tool_calls = lash_core_execution::MaxToolCalls::new(7);
    state.config_revision = 1;
    let receipt = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("publish new config");
    let next = store
        .load_session_head_meta(&session)
        .await
        .expect("new head")
        .expect("present");
    assert_eq!(next.head_revision, receipt.head_revision);
    assert_eq!(next.config.config_revision, 1);
    let latest = Connection::open(&path).expect("latest connection");
    let new_root: String = latest
        .query_row(
            "SELECT lower(hex(config_ref)) FROM session_revisions WHERE session_id = ?1 AND head_revision = ?2",
            params![session.as_str(), receipt.head_revision],
            |row| row.get(0),
        )
        .expect("new root");
    let old = crate::codec::try_load_session_head_meta_from_conn(
        &tx,
        &session,
        FleetFormat::current(),
        Some(crate::codec::ConfigRead::Committed(&store.configs)),
    )
    .expect("snapshot head")
    .expect("present");
    assert_eq!(old.head_revision, before.head_revision);
    assert_eq!(old.config, before.config);
    assert!(
        matches!(
            crate::codec::load_config_conn(
                &tx,
                Some(&new_root),
                FleetFormat::current(),
                Some(crate::codec::ConfigRead::Committed(&store.configs))
            ),
            Err(StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                ..
            })
        ),
        "a cached root committed after the snapshot is not readable in it"
    );
    tx.rollback().expect("end read snapshot");

    let tx = snapshot
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("write transaction");
    let mut payload = next.payload();
    payload.config.config_revision = 2;
    let json = serde_json::to_string(&payload).expect("payload");
    crate::revisions::record_revision_conn(&tx, &session, 999, None, None, &json)
        .expect("private config");
    tx.execute(
        "UPDATE session_head SET head_revision = 999 WHERE session_id = ?1",
        [session.as_str()],
    )
    .expect("private head");
    let root: String = tx.query_row("SELECT lower(hex(config_ref)) FROM session_revisions WHERE session_id = ?1 AND head_revision = 999", [session.as_str()], |row| row.get(0)).expect("private root");
    let read = crate::codec::try_load_session_head_meta_from_conn(
        &tx,
        &session,
        FleetFormat::current(),
        Some(crate::codec::ConfigRead::Uncommitted(&store.configs)),
    )
    .expect("read own writes")
    .expect("private head");
    assert_eq!(read.config.config_revision, 2);
    assert!(
        store
            .configs
            .get(
                lash_core_store::checkpoint_graph::NodeId::from_hex(&root).expect("root"),
                FleetFormat::current()
            )
            .expect("cache")
            .is_none(),
        "write misses never populate the shared cache"
    );
    assert_eq!(
        store
            .load_session_head_meta(&session)
            .await
            .expect("other read")
            .expect("committed head")
            .config
            .config_revision,
        1
    );
    tx.rollback().expect("rollback private config");
    assert!(matches!(
        crate::codec::load_config_conn(
            &snapshot,
            Some(&root),
            FleetFormat::current(),
            Some(crate::codec::ConfigRead::Committed(&store.configs))
        ),
        Err(StoreError::StoredDataCorrupt { .. })
    ));
    assert_eq!(
        store
            .load_session_head_meta(&session)
            .await
            .expect("after rollback")
            .expect("head")
            .head_revision,
        receipt.head_revision
    );
}

// COMPOUND-READ-SNAPSHOT: a cold head read selects its revision and hydrates
// that revision's config in one snapshot, so a commit that moves the head and
// peels the old config between the two cannot fail the read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_head_read_keeps_its_snapshot_while_the_old_config_is_peeled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("head-snapshot.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("store");
    let session = SessionId::from("head-snapshot");
    let mut state = durable_state(&store, &session).await;
    let selected = store
        .load_session_head_meta(&session)
        .await
        .expect("head")
        .expect("present");
    state.policy.max_tool_calls = lash_core_execution::MaxToolCalls::new(7);
    state.config_revision = 1;
    let next = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("publish the next config")
        .head_revision;
    drop(store);
    // The head stands at the selected revision again, with the next one
    // retained for the competing commit to move it to.
    let raw = Connection::open(&path).expect("fixture connection");
    raw.execute(
        "UPDATE session_head SET head_revision = ?2 WHERE session_id = ?1",
        params![session.as_str(), selected.head_revision],
    )
    .expect("stand the head at the selected revision");
    let old_root: String = raw
        .query_row(
            "SELECT lower(hex(config_ref)) FROM session_revisions WHERE session_id = ?1 AND head_revision = ?2",
            params![session.as_str(), selected.head_revision],
            |row| row.get(0),
        )
        .expect("selected config root");
    drop(raw);

    // A fresh store: its config cache is cold, so the read hydrates the graph.
    let pauses = crate::testing::SqlitePauses::default();
    let reader_store = crate::compound_read_tests::paused_file_store(&path, &pauses).await;
    let pause = pauses.pause_compound_read(crate::testing::SqliteCompoundRead::SessionHead);
    let reader = tokio::spawn({
        let store = Arc::clone(&reader_store);
        let session = session.clone();
        async move { store.load_session_head_meta(&session).await }
    });
    pause.wait_until_reached().await;
    crate::compound_read_tests::commit_competing(&path, |tx| {
        tx.execute(
            "UPDATE session_head SET head_revision = ?2 WHERE session_id = ?1",
            params![session.as_str(), next],
        )
        .expect("move the head");
        tx.execute(
            "DELETE FROM session_revisions WHERE session_id = ?1 AND head_revision = ?2",
            params![session.as_str(), selected.head_revision],
        )
        .expect("release the selected revision");
        crate::durable::peel(tx, std::collections::BTreeSet::from([old_root.clone()]))
            .expect("peel the released config");
        assert_eq!(
            crate::compound_read_tests::blob_rows(tx, &old_root),
            0,
            "the peel reclaims the config the paused read selected"
        );
    });
    pause.release();

    let read = reader
        .await
        .expect("reader task")
        .expect("a peel between head and config is no corruption")
        .expect("present");
    assert_eq!(read.head_revision, selected.head_revision);
    assert_eq!(
        read.config, selected.config,
        "the head and its config come from one snapshot"
    );
    let moved = reader_store
        .load_session_head_meta(&session)
        .await
        .expect("read after the move")
        .expect("present");
    assert_eq!(moved.head_revision, next, "the next read sees the move");
    assert_eq!(moved.config.config_revision, 1);
}

/// CONFIG-PUBLICATION-REUSE: an equal, already validated config keeps its
/// content identity without encoding it again; changed content is installed
/// even when the caller keeps its config revision unchanged.
#[tokio::test]
async fn head_publication_reuses_only_equal_validated_configuration() {
    let store = crate::test_support::sqlite_memory_store().await.unwrap();
    let session = SessionId::from("config-publication-reuse");
    let mut state = durable_state(&store, &session).await;
    let encodings = || async {
        store
            .conn
            .call(|_| Ok(crate::revisions::CONFIG_ENCODINGS.with(std::cell::Cell::get)))
            .await
            .unwrap()
    };
    let before = encodings().await;
    let original = store
        .load_session_head_meta(&session)
        .await
        .unwrap()
        .unwrap()
        .config;
    let receipt = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .unwrap();
    assert_eq!(
        encodings().await,
        before,
        "equal validated config must not be encoded again"
    );
    state.head_revision = receipt.head_revision;
    state.policy.max_tool_calls = lash_core_execution::MaxToolCalls::new(7);
    assert_eq!(
        state.config_revision, original.config_revision,
        "the config revision alone is not identity"
    );
    let receipt = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .unwrap();
    assert_eq!(encodings().await, before + 1);
    let next = store
        .load_session_head_meta(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.head_revision, receipt.head_revision);
    assert_ne!(next.config, original);
    assert_eq!(
        next.config.max_tool_calls,
        lash_core_execution::MaxToolCalls::new(7)
    );
    state.head_revision = receipt.head_revision;
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .unwrap();
    assert_eq!(
        encodings().await,
        before + 1,
        "the new config becomes reusable in its own right"
    );
}
