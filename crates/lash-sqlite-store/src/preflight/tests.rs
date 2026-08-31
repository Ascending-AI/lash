//! The preflight surface exists because opening the store answers the schema
//! question only by performing most of the open. Each test here first pins the
//! open path's side effect, then shows the preflight answering the same
//! question without it — the characterization is half the evidence, so it is
//! asserted rather than described.

use std::time::Duration;

use lash_core::{StorePreflight, StoreSchemaOutcome, StoreSchemaVerdict};

use super::{SqliteDatabase, SqliteStorePreflight, verify_schema_at};
use crate::Store;

fn temp_root() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

#[tokio::test]
async fn open_creates_a_missing_database_and_preflight_does_not() {
    let root = temp_root();
    let path = root.path().join("durable-core.db");

    // Red side: today's only way to ask "will this open?" is to open, and the
    // open path carries `SQLITE_OPEN_CREATE`.
    assert!(!path.exists());
    let store = Store::open(&path).await.expect("open creates the database");
    drop(store);
    assert!(
        path.exists(),
        "the open path provisions the database it was asked about"
    );

    // Green side: the same question over a path that does not exist, answered
    // without bringing one into existence.
    let absent = root.path().join("not-here.db");
    let preflight = SqliteStorePreflight::for_durable_core(&absent);
    let status = preflight.schema_status().await.expect("read schema status");
    assert_eq!(status.databases.len(), 1);
    assert_eq!(status.databases[0].verdict, StoreSchemaVerdict::Absent);
    assert!(
        !absent.exists(),
        "preflight must not create the database it reports as absent"
    );
    assert_eq!(
        status.outcome(),
        StoreSchemaOutcome::Ready,
        "an unprovisioned deployment has nothing that refuses and nothing undecided"
    );
}

#[tokio::test]
async fn durable_core_generation_43_is_refused_at_the_blake3_boundary() {
    let root = temp_root();
    let path = root.path().join("durable-core.db");
    Store::open(&path).await.expect("provision the database");
    let expected = SqliteDatabase::DurableCore.expected_version();
    // Component 45 introduced BLAKE3 identities and 46 the durable vocabulary
    // CHECKs. Both are reject-and-recreate boundaries, so the pin tracks the
    // current target while the refusal below still names a SHA-256-era
    // generation: nothing older than 45 may ever open, whatever the target is.
    assert_eq!(expected, 46, "the pinned durable-core target changed");

    rewind_user_version(&path, 43);

    let found = verify_schema_at(&path, SqliteDatabase::DurableCore).await;
    assert_eq!(found.verdict, StoreSchemaVerdict::Mismatch { found: 43 });
    assert_eq!(found.expected, expected);
    assert!(
        found.verdict.refuses_open(),
        "every SHA-256-era generation must be refused"
    );
}

#[tokio::test]
async fn preflight_answers_while_another_connection_holds_the_write_lock() {
    let root = temp_root();
    let path = root.path().join("durable-core.db");
    Store::open(&path).await.expect("provision the database");
    let expected = SqliteDatabase::DurableCore.expected_version();
    rewind_user_version(&path, expected - 1);

    let holder = rusqlite::Connection::open(&path).expect("open holder connection");
    holder
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("hold the write lock");

    // Red side: the open path takes `BEGIN IMMEDIATE` before it reads
    // `user_version`, so with the write lock held it cannot even reach the
    // question. It blocks on the busy handler instead of reporting the version.
    let blocked = tokio::time::timeout(Duration::from_secs(2), async {
        Store::open(&path)
            .await
            .map(|_| ())
            .map_err(|err| err.to_string())
    })
    .await;
    assert!(
        blocked.is_err(),
        "open must still be waiting for the write lock, not answering: {blocked:?}"
    );

    // Green side: the read-only path takes a shared lock, so the same question
    // is answered under the same contention.
    let answered = tokio::time::timeout(
        Duration::from_secs(5),
        verify_schema_at(&path, SqliteDatabase::DurableCore),
    )
    .await
    .expect("preflight answers while the write lock is held");
    assert_eq!(
        answered.verdict,
        StoreSchemaVerdict::Mismatch {
            found: expected - 1
        }
    );

    holder.execute_batch("ROLLBACK").expect("release the lock");
}

#[tokio::test]
async fn every_declared_database_is_reported_and_undeclared_ones_are_not() {
    let root = temp_root();
    let core = root.path().join("durable-core.db");
    let registry = root.path().join("processes.db");
    Store::open(&core).await.expect("provision durable core");

    let status = SqliteStorePreflight::for_session_store_root(root.path())
        .with_process_registry(&registry)
        .schema_status()
        .await
        .expect("read schema status");

    let names: Vec<&str> = status
        .databases
        .iter()
        .map(|database| database.name.as_str())
        .collect();
    assert_eq!(names, vec!["durable core", "process registry"]);
    assert_eq!(status.databases[0].verdict, StoreSchemaVerdict::Matches);
    assert_eq!(status.databases[1].verdict, StoreSchemaVerdict::Absent);
    assert!(
        !registry.exists(),
        "reading a declared but unprovisioned database must not provision it"
    );
}

#[tokio::test]
async fn a_file_that_is_not_a_database_is_undecided_rather_than_refused() {
    let root = temp_root();
    let path = root.path().join("durable-core.db");
    std::fs::write(&path, b"this is not a SQLite database").expect("write junk");

    let found = verify_schema_at(&path, SqliteDatabase::DurableCore).await;
    match &found.verdict {
        StoreSchemaVerdict::Unreadable { reason } => assert!(!reason.is_empty()),
        other => panic!("expected an undecided verdict, got {other:?}"),
    }
    assert!(
        !found.verdict.refuses_open(),
        "an unreadable database is undecided; a refusal needs a version to name"
    );
}

#[tokio::test]
async fn reading_a_hot_wal_database_leaves_its_bytes_untouched() {
    // The deleted read-write fallback made this false: such a connection
    // checkpoints a hot WAL and deletes it on close, which rewrites the main
    // file. The invariant is byte equality of the database itself, asserted
    // rather than described.
    let root = temp_root();
    let path = root.path().join("durable-core.db");
    let store = Store::open(&path).await.expect("provision the database");
    // Leave the WAL hot: a live writer that has not checkpointed is precisely
    // the state a boot-time probe finds.
    store
        .conn
        .call(|c| {
            c.execute_batch("CREATE TABLE lash_preflight_probe (id INTEGER PRIMARY KEY)")?;
            Ok(())
        })
        .await
        .expect("write without checkpointing");
    assert!(
        path.with_extension("db-wal").exists(),
        "the test needs a hot WAL to be meaningful"
    );

    let before = std::fs::read(&path).expect("read the database before");
    let found = verify_schema_at(&path, SqliteDatabase::DurableCore).await;
    let after = std::fs::read(&path).expect("read the database after");

    assert_eq!(found.verdict, StoreSchemaVerdict::Matches);
    assert_eq!(
        before, after,
        "a preflight read must not rewrite the database it inspected"
    );
    assert!(
        path.with_extension("db-wal").exists(),
        "a preflight read must not checkpoint away the write-ahead log"
    );
}

#[tokio::test]
async fn a_preflight_connection_refuses_to_write_even_if_asked() {
    // `PRAGMA query_only` is the enforced form of the module's promise: the
    // engine rejects a write on this connection, so the guarantee does not
    // depend on which statements this module happens to send.
    let root = temp_root();
    let path = root.path().join("durable-core.db");
    Store::open(&path).await.expect("provision the database");

    let conn = crate::conn::SqliteConnection::open_readonly(&path)
        .await
        .expect("open read-only");
    let refusal = conn
        .call(|c| {
            c.pragma_update(None, "query_only", true)?;
            c.execute_batch("CREATE TABLE lash_should_not_exist (id INTEGER)")
        })
        .await
        .expect_err("a query_only connection must refuse a write");
    assert!(
        refusal.to_string().to_lowercase().contains("readonly"),
        "{refusal}"
    );
}

#[test]
fn every_database_publishes_the_version_its_open_enforces() {
    assert_eq!(
        SqliteDatabase::DurableCore.expected_version(),
        i64::from(crate::schema::SCHEMA_VERSION)
    );
    assert_eq!(
        SqliteDatabase::ProcessRegistry.expected_version(),
        i64::from(crate::schema::PROCESS_SCHEMA_VERSION)
    );
    assert_eq!(
        SqliteDatabase::Triggers.expected_version(),
        i64::from(crate::schema::TRIGGER_SCHEMA_VERSION)
    );
    assert_eq!(
        SqliteDatabase::EffectReplay.expected_version(),
        i64::from(crate::schema::EFFECT_SCHEMA_VERSION)
    );
}

fn rewind_user_version(path: &std::path::Path, version: i64) {
    let conn = rusqlite::Connection::open(path).expect("open for rewind");
    conn.pragma_update(None, "user_version", version)
        .expect("rewind user_version");
}

/// The durable-payload walk: what is parked, whose it is, and what the walk
/// refuses to do to find out.
///
/// The schema read above answers "would this open?". These tests pin the
/// question an operator asks next — "then what is stuck behind it?" — and the
/// two properties that make the answer usable: an item nobody can read is still
/// an item, and a surface nobody read is never an empty one.
mod walk {
    use lash_core::{
        DurablePayload, DurableScan, DurableSurface, ScanCoverage, StorePreflight,
        store::EXECUTION_STATE_CHECKPOINT_COMPONENT,
    };

    use super::super::SqliteStorePreflight;
    use crate::{SqliteProcessRegistry, Store};

    const EVERY_SURFACE: [DurableSurface; 5] = [
        DurableSurface::ModuleArtifact,
        DurableSurface::ParkedSegment,
        DurableSurface::PendingWake,
        DurableSurface::SessionCheckpoint,
        DurableSurface::SessionExecutionState,
    ];

    fn registration(id: &str) -> lash_core::ProcessRegistration {
        lash_core::ProcessRegistration::new(
            id,
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::session(lash_core::SessionScope::new("session")),
        )
        .with_wake_session_id(Some("wake-session".to_string()))
    }

    fn handover(segment_ordinal: u64) -> lash_core::PersistedSegmentHandover {
        lash_core::PersistedSegmentHandover {
            segment_ordinal,
            handover: lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: "program-v1".to_string(),
                engine_state: vec![segment_ordinal as u8],
            },
        }
    }

    /// Park one handover under a live process and, when asked, a second under a
    /// process that has already reached a terminal outcome.
    async fn park_segment(registry: &SqliteProcessRegistry, process_id: &str) {
        use lash_core::ProcessContinuationStore;
        use lash_core::ProcessRegistry;
        registry
            .register_process(registration(process_id))
            .await
            .expect("register process");
        registry
            .put_segment_handover(process_id, handover(1))
            .await
            .expect("park a segment handover");
    }

    async fn complete(registry: &SqliteProcessRegistry, process_id: &str) {
        use lash_core::ProcessRegistry;
        registry
            .complete_process(
                process_id,
                lash_core::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(serde_json::json!({"ok": true})),
                ),
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete process");
    }

    #[tokio::test]
    async fn an_unprovisioned_deployment_scans_every_surface_and_creates_nothing() {
        // The pairing that matters: every surface answers `Scanned` with nothing
        // in it — "we looked, there is nothing parked" — and the databases the
        // walk was pointed at still do not exist afterwards. A probe that
        // provisioned the deployment it was asked about would have answered a
        // different question.
        let root = super::temp_root();
        let core = root.path().join(crate::DURABLE_CORE_DB_FILE);
        let registry = root.path().join("processes.db");
        let preflight = SqliteStorePreflight::for_session_store_root(root.path())
            .with_process_registry(&registry);

        for surface in EVERY_SURFACE {
            let page = preflight
                .scan_durable(&DurableScan::first(surface, 10))
                .await
                .expect("scan an unprovisioned surface");
            assert_eq!(page.coverage, ScanCoverage::Scanned, "{surface:?}");
            assert!(page.items.is_empty(), "{surface:?}: {:?}", page.items);
            assert_eq!(page.next, None, "{surface:?}");
        }

        assert!(!core.exists(), "the walk must not create the durable core");
        assert!(
            !registry.exists(),
            "the walk must not create the process registry"
        );
    }

    #[tokio::test]
    async fn module_artifact_surface_reads_the_persisted_json() {
        use lashlang::LashlangArtifactStore;

        let root = super::temp_root();
        let core = root.path().join(crate::DURABLE_CORE_DB_FILE);
        let store = Store::open(&core).await.expect("provision durable core");
        let frozen: lashlang::ModuleArtifact = serde_json::from_slice(include_bytes!(
            "../../../lashlang/tests/fixtures/module-artifact-old.json"
        ))
        .expect("decode frozen artifact shape");
        let artifact = lashlang::ModuleArtifact::from_program(frozen.canonical_ir)
            .expect("mint the current identity generation");
        store
            .put_module_artifact(&artifact)
            .await
            .expect("persist module artifact");
        drop(store);

        let page = SqliteStorePreflight::for_session_store_root(root.path())
            .scan_durable(&DurableScan::first(DurableSurface::ModuleArtifact, 10))
            .await
            .expect("walk module artifacts");
        assert_eq!(page.coverage, ScanCoverage::Scanned);
        assert_eq!(page.items.len(), 1, "{page:?}");
        assert_eq!(page.items[0].cursor, artifact.module_ref.as_str());
        match &page.items[0].payload {
            DurablePayload::Json(json) => assert!(json.contains("compilation_dialect"), "{json}"),
            other => panic!("expected module artifact JSON, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_undeclared_process_registry_is_not_scanned() {
        // An empty page and an unwalked surface are the two answers a preflight
        // must never confuse. A deployment that declared no registry has one
        // nobody looked at, and the reason has to name that rather than leave a
        // host reading "nothing refuses" out of it.
        let root = super::temp_root();
        let preflight = SqliteStorePreflight::for_session_store_root(root.path());

        for surface in [DurableSurface::ParkedSegment, DurableSurface::PendingWake] {
            let page = preflight
                .scan_durable(&DurableScan::first(surface, 10))
                .await
                .expect("scan an undeclared surface");
            match &page.coverage {
                ScanCoverage::NotScanned { reason } => assert!(
                    reason.contains("declared no process registry"),
                    "{surface:?}: {reason}"
                ),
                other => panic!("{surface:?}: expected an unscanned surface, got {other:?}"),
            }
            assert!(page.items.is_empty(), "{surface:?}");
        }
    }

    #[tokio::test]
    async fn a_parked_segment_is_listed_with_its_owner_and_a_terminal_one_is_not() {
        let root = super::temp_root();
        let path = root.path().join("processes.db");
        let registry = SqliteProcessRegistry::open(&path, root.path().join("sessions"))
            .await
            .expect("open registry");
        park_segment(&registry, "proc-live").await;
        park_segment(&registry, "proc-done").await;
        complete(&registry, "proc-done").await;
        drop(registry);

        // The terminal process's handover row is still on disk — the exclusion
        // has to come from the status predicate, not from the row having been
        // cleaned up, or this test would pass without testing anything.
        let raw = rusqlite::Connection::open(&path).expect("open raw registry");
        let parked: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM process_segment_handovers WHERE process_id = 'proc-done'",
                [],
                |row| row.get(0),
            )
            .expect("count terminal handovers");
        assert_eq!(parked, 1, "the terminal process must still hold its row");
        drop(raw);

        let page = SqliteStorePreflight::for_session_store_root(root.path())
            .with_process_registry(&path)
            .scan_durable(&DurableScan::first(DurableSurface::ParkedSegment, 10))
            .await
            .expect("walk parked segments");

        assert_eq!(page.coverage, ScanCoverage::Scanned);
        assert_eq!(page.next, None, "a short page ends the surface");
        assert_eq!(page.items.len(), 1, "{:?}", page.items);
        let item = &page.items[0];
        assert_eq!(item.surface, DurableSurface::ParkedSegment);
        assert_eq!(item.process_id.as_deref(), Some("proc-live"));
        assert_eq!(item.session_id.as_deref(), Some("wake-session"));
        assert_eq!(item.status.as_deref(), Some("running"));
        assert!(
            item.owner_record
                .as_deref()
                .is_some_and(|record| record.contains("proc-live")),
            "the owner record travels with the item: {:?}",
            item.owner_record
        );
        match &item.payload {
            // Handed over as stored text: the walk reports the payload, it does
            // not parse it.
            DurablePayload::Json(json) => assert!(json.contains("program-v1"), "{json}"),
            other => panic!("expected the stored handover JSON, got {other:?}"),
        }
        assert!(
            item.cursor.starts_with("proc-live:"),
            "the cursor names its row: {}",
            item.cursor
        );
    }

    #[tokio::test]
    async fn paging_returns_every_item_exactly_once() {
        let root = super::temp_root();
        let path = root.path().join("processes.db");
        let registry = SqliteProcessRegistry::open(&path, root.path().join("sessions"))
            .await
            .expect("open registry");
        park_segment(&registry, "proc-a").await;
        park_segment(&registry, "proc-b").await;
        drop(registry);

        let preflight =
            SqliteStorePreflight::for_session_store_root(root.path()).with_process_registry(&path);

        let first = preflight
            .scan_durable(&DurableScan::first(DurableSurface::ParkedSegment, 1))
            .await
            .expect("first page");
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.items[0].process_id.as_deref(), Some("proc-a"));
        let cursor = first
            .next
            .clone()
            .expect("a full page must offer a resume cursor");
        assert_eq!(cursor, first.items[0].cursor);

        let second = preflight
            .scan_durable(&DurableScan::after(
                DurableSurface::ParkedSegment,
                cursor,
                1,
            ))
            .await
            .expect("second page");
        assert_eq!(second.items.len(), 1);
        assert_eq!(
            second.items[0].process_id.as_deref(),
            Some("proc-b"),
            "resuming after a cursor must not repeat the item it names"
        );

        let third = preflight
            .scan_durable(&DurableScan::after(
                DurableSurface::ParkedSegment,
                second.items[0].cursor.clone(),
                1,
            ))
            .await
            .expect("third page");
        assert!(third.items.is_empty(), "{:?}", third.items);
        assert_eq!(third.next, None, "an exhausted surface offers no cursor");
    }

    #[tokio::test]
    async fn a_dangling_checkpoint_ref_is_reported_rather_than_dropped() {
        // Hand-crafted because no store API can produce it: a published root
        // whose manifest blob is gone is precisely the state the commit path
        // exists to prevent. It is also the single most alarming thing a
        // preflight can find, so it must survive the walk as a named item
        // instead of vanishing or taking the page down with it.
        let root = super::temp_root();
        let core = root.path().join(crate::DURABLE_CORE_DB_FILE);
        Store::open(&core).await.expect("provision durable core");
        let raw = rusqlite::Connection::open(&core).expect("open raw core");
        raw.execute(
            "INSERT INTO session_head
             (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
             VALUES ('orphaned', '{}', 0, NULL, 'missing-checkpoint-manifest')",
            [],
        )
        .expect("install a dangling checkpoint reference");
        drop(raw);

        let page = SqliteStorePreflight::for_session_store_root(root.path())
            .scan_durable(&DurableScan::first(DurableSurface::SessionCheckpoint, 10))
            .await
            .expect("a dangling reference must not fail the page");

        assert_eq!(page.coverage, ScanCoverage::Scanned);
        assert_eq!(page.items.len(), 1, "{:?}", page.items);
        assert_eq!(page.items[0].session_id.as_deref(), Some("orphaned"));
        match &page.items[0].payload {
            DurablePayload::Missing { reason } => {
                assert!(reason.contains("missing-checkpoint-manifest"), "{reason}");
                assert!(reason.contains("orphaned"), "{reason}");
            }
            other => panic!("expected a Missing payload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_bare_checkpoint_blob_is_reported_missing_with_a_decode_reason() {
        let root = super::temp_root();
        let core = root.path().join(crate::DURABLE_CORE_DB_FILE);
        Store::open(&core).await.expect("provision durable core");
        let raw = rusqlite::Connection::open(&core).expect("open raw core");
        raw.execute(
            "INSERT INTO blobs (hash, content) VALUES ('bare-checkpoint', ?1)",
            rusqlite::params![b"bare blob body".to_vec()],
        )
        .expect("install a bare checkpoint blob");
        raw.execute(
            "INSERT INTO session_head
             (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
             VALUES ('bare-checkpoint-session', '{}', 0, NULL, 'bare-checkpoint')",
            [],
        )
        .expect("install a session pointing at the bare blob");
        drop(raw);

        let page = SqliteStorePreflight::for_session_store_root(root.path())
            .scan_durable(&DurableScan::first(DurableSurface::SessionCheckpoint, 10))
            .await
            .expect("a corrupt envelope must not fail the page");

        assert_eq!(page.coverage, ScanCoverage::Scanned);
        assert_eq!(page.items.len(), 1, "{page:?}");
        match &page.items[0].payload {
            DurablePayload::Missing { reason } => {
                assert!(reason.contains("artifact blob envelope"), "{reason}");
            }
            other => panic!("expected a Missing payload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_real_checkpoint_yields_logical_manifest_and_execution_state_bytes() {
        // Written through the store's own commit path so the bytes carry the
        // real envelope framing, which is what makes "logical bytes" a claim
        // worth asserting: the walk must strip this crate's storage wrapper and
        // hand back what the format manifest describes.
        let root = super::temp_root();
        let core = root.path().join(crate::DURABLE_CORE_DB_FILE);
        let store = Store::open(&core).await.expect("provision durable core");
        let body = rmp_serde::to_vec_named(&serde_json::json!({"execution": "state"}))
            .expect("encode an execution-state component");
        let mut components = std::collections::BTreeMap::new();
        components.insert(
            EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
            lash_core::HydratedCheckpointComponent::changed(body.clone()),
        );
        let stored = store
            .put_checkpoint(&lash_core::HydratedSessionCheckpoint {
                turn_state: lash_core::PersistedTurnState::default(),
                components,
                plugin_snapshot_revision: None,
            })
            .await
            .expect("commit a checkpoint");
        drop(store);

        let raw = rusqlite::Connection::open(&core).expect("open raw core");
        raw.execute(
            "INSERT INTO session_head
             (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
             VALUES ('published', '{}', 0, NULL, ?1)",
            rusqlite::params![stored.checkpoint_ref.as_str()],
        )
        .expect("publish the checkpoint root");
        drop(raw);

        let preflight = SqliteStorePreflight::for_session_store_root(root.path());

        let manifests = preflight
            .scan_durable(&DurableScan::first(DurableSurface::SessionCheckpoint, 10))
            .await
            .expect("walk session checkpoints");
        assert_eq!(manifests.items.len(), 1, "{:?}", manifests.items);
        match &manifests.items[0].payload {
            DurablePayload::MessagePack(bytes) => {
                // Logical bytes, not the stored envelope: the manifest decodes
                // on its own, which the wrapped form would not.
                let decoded: serde_json::Value =
                    rmp_serde::from_slice(bytes).expect("the manifest's logical bytes decode");
                assert!(
                    decoded
                        .get("components")
                        .and_then(|components| {
                            components.get(EXECUTION_STATE_CHECKPOINT_COMPONENT)
                        })
                        .is_some(),
                    "{decoded:?}"
                );
            }
            other => panic!("expected the manifest bytes, got {other:?}"),
        }

        let execution_state = preflight
            .scan_durable(&DurableScan::first(
                DurableSurface::SessionExecutionState,
                10,
            ))
            .await
            .expect("walk session execution state");
        assert_eq!(
            execution_state.items.len(),
            1,
            "{:?}",
            execution_state.items
        );
        assert_eq!(
            execution_state.items[0].payload,
            DurablePayload::MessagePack(body),
            "the component's logical bytes travel unchanged"
        );
        assert_eq!(
            execution_state.items[0].cursor, "published",
            "the execution-state cursor is the session, so paging stays stable \
             even when a session contributes no item"
        );
    }

    #[tokio::test]
    async fn a_checkpoint_without_execution_state_contributes_no_item() {
        // A session that genuinely stores no execution state is not a defect,
        // so it must not appear as a `Missing` item — the report's unreadable
        // list is for things that should be there and are not.
        let root = super::temp_root();
        let core = root.path().join(crate::DURABLE_CORE_DB_FILE);
        let store = Store::open(&core).await.expect("provision durable core");
        let mut components = std::collections::BTreeMap::new();
        components.insert(
            "something_else".to_string(),
            lash_core::HydratedCheckpointComponent::changed(vec![1, 2, 3]),
        );
        let stored = store
            .put_checkpoint(&lash_core::HydratedSessionCheckpoint {
                turn_state: lash_core::PersistedTurnState::default(),
                components,
                plugin_snapshot_revision: None,
            })
            .await
            .expect("commit a checkpoint");
        drop(store);

        let raw = rusqlite::Connection::open(&core).expect("open raw core");
        raw.execute(
            "INSERT INTO session_head
             (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
             VALUES ('stateless', '{}', 0, NULL, ?1)",
            rusqlite::params![stored.checkpoint_ref.as_str()],
        )
        .expect("publish the checkpoint root");
        drop(raw);

        let page = SqliteStorePreflight::for_session_store_root(root.path())
            .scan_durable(&DurableScan::first(
                DurableSurface::SessionExecutionState,
                10,
            ))
            .await
            .expect("walk session execution state");
        assert_eq!(page.coverage, ScanCoverage::Scanned);
        assert!(page.items.is_empty(), "{:?}", page.items);
    }
}
