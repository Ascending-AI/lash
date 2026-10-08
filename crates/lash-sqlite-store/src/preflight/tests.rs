//! The preflight surface exists because opening the store answers the schema
//! question only by performing most of the open. Each test here first pins the
//! open path's side effect, then shows the preflight answering the same
//! question without it — the characterization is half the evidence, so it is
//! asserted rather than described.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::time::Duration;

use lash_core_execution::{StorePreflight, StoreSchemaOutcome, StoreSchemaVerdict};

use super::{SqliteStorePreflight, verify_schema_at};
use crate::{SqliteConnectionPolicy, SqliteStore, StoreOptions};

fn temp_root() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

#[tokio::test]
async fn open_creates_a_missing_database_and_preflight_does_not() {
    let root = temp_root();
    let path = root.path().join("lash.db");

    // Red side: today's only way to ask "will this open?" is to open, and the
    // open path carries `SQLITE_OPEN_CREATE`.
    assert!(!path.exists());
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("open creates the database");
    drop(store);
    assert!(
        path.exists(),
        "the open path provisions the database it was asked about"
    );

    // Green side: the same question where nothing exists yet, answered
    // without bringing one byte into existence. The database is `Absent` —
    // the next open provisions it — so nothing refuses.
    let empty = temp_root();
    let status = SqliteStorePreflight::for_database_file(empty.path().join("lash.db"))
        .schema_status()
        .await
        .expect("read schema status");
    assert_eq!(status.databases.len(), 1);
    assert_eq!(status.databases[0].verdict, StoreSchemaVerdict::Absent);
    assert!(
        std::fs::read_dir(empty.path())
            .expect("the root still exists")
            .next()
            .is_none(),
        "preflight must not create the database"
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
    let path = root.path().join("lash.db");
    SqliteStore::open_file_for_testing(&path)
        .await
        .expect("provision the database");
    let expected = crate::schema::expected_version();
    // Component 43 is a pre-1.0 SHA-256-era catalog. Both active tiers
    // refuse that retired stamp rather than running the deleted 43-to-44
    // upgrade. The reported target belongs to the opening build's tier.
    let descriptor =
        lash_core_execution::compat::descriptor(crate::schema::COMPONENT).expect("core descriptor");
    assert_eq!(expected, i64::from(descriptor.writes.max()));

    stamp_compat(&path, 43, 43);

    let found = verify_schema_at(&path).await;
    // Which side of the build's range 43 falls on is the admission rule's
    // to say; either way the stamp is refused typed.
    let refusal = lash_core_execution::compat::admit(
        descriptor,
        lash_core_execution::compat::StampRead::Present(lash_core_execution::compat::CompatStamp {
            version: 43,
            min_reader: 43,
        }),
    )
    .expect_err("a retired stamp is refused");
    assert_eq!(found.verdict, StoreSchemaVerdict::Refused { refusal });
    assert_eq!(found.expected, expected);
    assert!(
        found.verdict.refuses_open(),
        "every SHA-256-era generation must be refused"
    );
}

#[tokio::test]
async fn preflight_answers_while_another_connection_holds_the_write_lock() {
    let root = temp_root();
    let path = root.path().join("lash.db");
    SqliteStore::open_file_for_testing(&path)
        .await
        .expect("provision the database");
    let above = lash_core_execution::compat::descriptor(crate::schema::COMPONENT)
        .expect("core descriptor")
        .reads
        .max()
        + 1;
    stamp_compat(&path, i64::from(above), i64::from(above));

    let holder = rusqlite::Connection::open(&path).expect("open holder connection");
    holder
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("hold the write lock");

    // Red side: the open path takes `BEGIN IMMEDIATE` before it reads
    // the compatibility row, so with the write lock held it cannot even reach the
    // question. It blocks on the busy handler instead of reporting the version.
    let blocked = tokio::time::timeout(Duration::from_secs(2), async {
        SqliteStore::open_file_for_testing(&path)
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
    let answered = tokio::time::timeout(Duration::from_secs(5), verify_schema_at(&path))
        .await
        .expect("preflight answers while the write lock is held");
    assert!(matches!(
        answered.verdict,
        StoreSchemaVerdict::Refused {
            refusal: lash_core_execution::compat::CompatRefusal::ReaderFloorAbove { found, .. }
        } if found == above
    ));

    holder.execute_batch("ROLLBACK").expect("release the lock");
}

#[tokio::test]
async fn a_sqlite_memory_store_set_preflights_through_its_location() {
    // `SqliteLocation::Memory` names the same database the open pinned; the
    // probe reads it while the set's handles hold the anchor.
    let set = crate::SqliteStoreSet::memory()
        .await
        .expect("open a memory store set");

    let status = SqliteStorePreflight::for_location(set.location().clone())
        .schema_status()
        .await
        .expect("read schema status");

    assert_eq!(status.databases.len(), 1);
    assert_eq!(status.databases[0].verdict, StoreSchemaVerdict::Matches);
    assert_eq!(status.outcome(), StoreSchemaOutcome::Ready);
}

#[tokio::test]
async fn a_file_that_is_not_a_database_is_undecided_rather_than_refused() {
    let root = temp_root();
    let path = root.path().join("lash.db");
    std::fs::write(&path, b"this is not a SQLite database").expect("write junk");

    let found = verify_schema_at(&path).await;
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
    let path = root.path().join("lash.db");
    // The fixture's own checkpointing is off (FIG-4089): with the default
    // policy the provisioning commits wake the store's checkpoint worker, whose
    // PASSIVE checkpoint can land between the before/after reads and rewrite
    // the main file — a byte change nothing under test caused.
    let store = SqliteStore::open_file_with_options_for_testing(
        &path,
        StoreOptions {
            connection_policy: SqliteConnectionPolicy {
                wal_autocheckpoint_pages: 0,
                ..SqliteConnectionPolicy::standard(crate::SqliteSynchronous::Normal)
            },
            ..StoreOptions::standard(crate::SqliteSynchronous::Normal)
        },
    )
    .await
    .expect("provision the database");
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
    let found = verify_schema_at(&path).await;
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

fn stamp_compat(path: &std::path::Path, version: i64, min_reader: i64) {
    let conn = rusqlite::Connection::open(path).expect("open for stamp");
    conn.execute(
        "UPDATE lash_compat SET version = ?1, min_reader = ?2 WHERE singleton = 1",
        rusqlite::params![version, min_reader],
    )
    .expect("update compatibility stamp");
}

/// The durable-payload walk: what is parked, whose it is, and what the walk
/// refuses to do to find out.
///
/// The schema read above answers "would this open?". These tests pin the
/// question an operator asks next — "then what is stuck behind it?" — and the
/// two properties that make the answer usable: an item nobody can read is still
/// an item, and a surface nobody read is never an empty one.
mod walk {
    use lash_core_execution::{
        DurablePayload, DurableScan, DurableSurface, ScanCoverage, StorePreflight,
        store::EXECUTION_STATE_CHECKPOINT_COMPONENT,
    };
    use lash_core_execution::{ProcessLifecycle as _, ProcessRegistrar as _};
    use lash_sansio::ProcessId;

    use super::super::SqliteStorePreflight;
    use crate::{SqliteProcessRegistry, SqliteStore};

    const EVERY_SURFACE: [DurableSurface; 4] = [
        DurableSurface::ModuleArtifact,
        DurableSurface::StartedProcess,
        DurableSurface::SessionCheckpoint,
        DurableSurface::SessionExecutionState,
    ];

    fn registration() -> lash_core_execution::ProcessRegistration {
        lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core_execution::ProcessProvenance::session(
                lash_core_execution::SessionScope::new("session"),
            ),
            lash_core_execution::Lifetime::Detached,
        )
    }

    /// Register a live process and answer the id the registrar minted.
    async fn live_process(registry: &SqliteProcessRegistry) -> ProcessId {
        registry
            .register_process(registration())
            .await
            .expect("register process")
            .id
    }

    async fn complete(registry: &SqliteProcessRegistry, process_id: &ProcessId) {
        registry
            .complete_process(
                process_id,
                lash_core_execution::ProcessAwaitOutput::from_tool_output(
                    lash_core_execution::ToolCallOutput::success(serde_json::json!({"ok": true})),
                ),
                lash_core_execution::ProcessCompletionAuthority::workflow_key(process_id),
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
        let path = root.path().join("lash.db");
        let preflight = SqliteStorePreflight::for_database_file(&path);

        for surface in EVERY_SURFACE {
            let page = preflight
                .scan_durable(&DurableScan::first(surface, 10))
                .await
                .expect("scan an unprovisioned surface");
            assert_eq!(page.coverage, ScanCoverage::Scanned, "{surface:?}");
            assert!(page.items.is_empty(), "{surface:?}: {:?}", page.items);
            assert_eq!(page.next, None, "{surface:?}");
        }

        assert!(!path.exists(), "the walk must not create {path:?}");
    }

    #[tokio::test]
    async fn module_artifact_surface_reads_the_persisted_json() {
        let root = super::temp_root();
        let core = root.path().join("lash.db");
        let store = SqliteStore::open_file_for_testing(&core)
            .await
            .expect("provision durable core");
        let artifact = lashlang::ModuleArtifact::from_program(lashlang::Program::block(vec![
            lashlang::Expr::Finish(Box::new(lashlang::Expr::String("done".into()))),
        ]))
        .expect("a one-statement module forms an artifact");
        lash_core_execution::ModuleArtifactStore::publish_module_artifact(
            &store,
            &lash_core_execution::ReferrerClaim::unguarded(
                lash_core_execution::ArtifactReferrer::HostPin(
                    lash_core_execution::HostArtifactPin::mint(),
                ),
            )
            .expect("host pin claim"),
            artifact.module_ref().as_str(),
            &artifact.to_store_bytes().expect("encode module"),
        )
        .await
        .expect("persist module artifact");

        let page = SqliteStorePreflight::for_database_file(root.path().join("lash.db"))
            .scan_durable(&DurableScan::first(DurableSurface::ModuleArtifact, 10))
            .await
            .expect("walk module artifacts");
        assert_eq!(page.coverage, ScanCoverage::Scanned);
        assert_eq!(page.items.len(), 1, "{page:?}");
        assert_eq!(page.items[0].cursor, artifact.module_ref().as_str());
        match &page.items[0].payload {
            DurablePayload::Json(json) => assert!(
                json.contains("host_requirements_ref") && json.contains("\"ir\""),
                "{json}"
            ),
            other => panic!("expected module artifact JSON, got {other:?}"),
        }
    }

    /// C8 (FIG-3571): every live process is walked with its record, which
    /// carries the start stamp the probe judges; a terminal one is not.
    #[tokio::test]
    async fn a_live_process_is_walked_with_its_record_and_a_terminal_one_is_not() {
        let root = super::temp_root();
        let path = root.path().join("lash.db");
        let registry = SqliteProcessRegistry::open_standalone_for_testing(&path)
            .await
            .expect("open registry");
        let live = registry
            .register_process(registration())
            .await
            .expect("register process")
            .id;
        let done = registry
            .register_process(registration())
            .await
            .expect("register process")
            .id;
        complete(&registry, &done).await;
        drop(registry);

        let page = SqliteStorePreflight::for_database_file(root.path().join("lash.db"))
            .scan_durable(&DurableScan::first(DurableSurface::StartedProcess, 10))
            .await
            .expect("walk started processes");

        assert_eq!(page.coverage, ScanCoverage::Scanned);
        assert_eq!(page.next, None, "a short page ends the surface");
        assert_eq!(page.items.len(), 1, "{:?}", page.items);
        let item = &page.items[0];
        assert_eq!(item.surface, DurableSurface::StartedProcess);
        assert_eq!(item.process_id.as_ref(), Some(&live));
        assert_eq!(item.cursor, live.as_str());
        match &item.payload {
            DurablePayload::Json(json) => assert!(json.contains(live.as_str()), "{json}"),
            other => panic!("expected the stored record JSON, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn paging_returns_every_item_exactly_once() {
        let root = super::temp_root();
        let path = root.path().join("lash.db");
        let registry = SqliteProcessRegistry::open_standalone_for_testing(&path)
            .await
            .expect("open registry");
        let first_process = live_process(&registry).await;
        let second_process = live_process(&registry).await;
        drop(registry);

        let preflight = SqliteStorePreflight::for_database_file(root.path().join("lash.db"));

        let first = preflight
            .scan_durable(&DurableScan::first(DurableSurface::StartedProcess, 1))
            .await
            .expect("first page");
        assert_eq!(first.items.len(), 1);
        assert_eq!(
            first.items[0].process_id.as_deref(),
            Some(first_process.as_str())
        );
        let cursor = first
            .next
            .clone()
            .expect("a full page must offer a resume cursor");
        assert_eq!(cursor, first.items[0].cursor);

        let second = preflight
            .scan_durable(&DurableScan::after(
                DurableSurface::StartedProcess,
                cursor,
                1,
            ))
            .await
            .expect("second page");
        assert_eq!(second.items.len(), 1);
        assert_eq!(
            second.items[0].process_id.as_deref(),
            Some(second_process.as_str()),
            "resuming after a cursor must not repeat the item it names"
        );

        let third = preflight
            .scan_durable(&DurableScan::after(
                DurableSurface::StartedProcess,
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
        let core = root.path().join("lash.db");
        SqliteStore::open_file_for_testing(&core)
            .await
            .expect("provision durable core");
        let raw = rusqlite::Connection::open(&core).expect("open raw core");
        raw.execute(
            "INSERT INTO session_revisions (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
             VALUES ('orphaned', '{}', 0, NULL, 'missing-checkpoint-manifest')",
            [],
        ).expect("record the fixture revision");
        raw.execute(
            "INSERT INTO session_head (session_id, head_revision) VALUES ('orphaned', 0)",
            [],
        )
        .expect("install a dangling checkpoint reference");
        drop(raw);

        let page = SqliteStorePreflight::for_database_file(root.path().join("lash.db"))
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
        let core = root.path().join("lash.db");
        SqliteStore::open_file_for_testing(&core)
            .await
            .expect("provision durable core");
        let raw = rusqlite::Connection::open(&core).expect("open raw core");
        raw.execute(
            "INSERT INTO blobs (hash, content) VALUES ('bare-checkpoint', ?1)",
            rusqlite::params![b"bare blob body".to_vec()],
        )
        .expect("install a bare checkpoint blob");
        raw.execute(
            "INSERT INTO session_revisions (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
             VALUES ('bare-checkpoint-session', '{}', 0, NULL, 'bare-checkpoint')",
            [],
        ).expect("record the fixture revision");
        raw.execute("INSERT INTO session_head (session_id, head_revision) VALUES ('bare-checkpoint-session', 0)", [])
            .expect("install a session pointing at the bare blob");
        drop(raw);

        let page = SqliteStorePreflight::for_database_file(root.path().join("lash.db"))
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
        let core = root.path().join("lash.db");
        let store = SqliteStore::open_file_for_testing(&core)
            .await
            .expect("provision durable core");
        let body = rmp_serde::to_vec_named(&serde_json::json!({"execution": "state"}))
            .expect("encode an execution-state component");
        let mut components = std::collections::BTreeMap::new();
        components.insert(
            EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
            lash_core_execution::HydratedCheckpointComponent::changed(body.clone()),
        );
        let stored = store
            .put_checkpoint(&lash_core_execution::HydratedSessionCheckpoint {
                turn_state: lash_core_execution::PersistedTurnState::default(),
                components,
            })
            .await
            .expect("commit a checkpoint");
        drop(store);

        let raw = rusqlite::Connection::open(&core).expect("open raw core");
        raw.execute(
            "INSERT INTO session_revisions (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
             VALUES ('published', '{}', 0, NULL, ?1)",
            rusqlite::params![stored.checkpoint_ref.as_str()],
        ).expect("record the fixture revision");
        raw.execute(
            "INSERT INTO session_head (session_id, head_revision) VALUES ('published', 0)",
            [],
        )
        .expect("publish the checkpoint root");
        drop(raw);

        let preflight = SqliteStorePreflight::for_database_file(root.path().join("lash.db"));

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
        let core = root.path().join("lash.db");
        let store = SqliteStore::open_file_for_testing(&core)
            .await
            .expect("provision durable core");
        let mut components = std::collections::BTreeMap::new();
        components.insert(
            "something_else".to_string(),
            lash_core_execution::HydratedCheckpointComponent::changed(vec![1, 2, 3]),
        );
        let stored = store
            .put_checkpoint(&lash_core_execution::HydratedSessionCheckpoint {
                turn_state: lash_core_execution::PersistedTurnState::default(),
                components,
            })
            .await
            .expect("commit a checkpoint");
        drop(store);

        let raw = rusqlite::Connection::open(&core).expect("open raw core");
        raw.execute(
            "INSERT INTO session_revisions (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
             VALUES ('stateless', '{}', 0, NULL, ?1)",
            rusqlite::params![stored.checkpoint_ref.as_str()],
        ).expect("record the fixture revision");
        raw.execute(
            "INSERT INTO session_head (session_id, head_revision) VALUES ('stateless', 0)",
            [],
        )
        .expect("publish the checkpoint root");
        drop(raw);

        let page = SqliteStorePreflight::for_database_file(root.path().join("lash.db"))
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

/// FIG-5270: frozen version-1 counters cannot certify a pre-release shape.
#[tokio::test]
async fn release_build_refuses_pre_release_store_before_counters_without_mutation() {
    for writing in ["0.0.0-dev", "0.9.0", "1.0.0-rc.1"] {
        for counter in [1, 2, 99] {
            assert_release_admission(writing, "1.0.0", counter, true).await;
        }
    }
}

/// FIG-5270: the baseline release opens its own version-1 store.
#[tokio::test]
async fn release_build_admits_release_store() {
    assert_release_admission("1.0.0", "1.0.0", 1, false).await;
}

/// FIG-5270: the release cut rule does not change development admission.
#[tokio::test]
async fn pre_release_build_admits_pre_release_store() {
    assert_release_admission("0.0.0-dev", "0.0.0-dev", 1, false).await;
}

async fn assert_release_admission(writing: &str, build: &str, counter: u32, refuses: bool) {
    let root = temp_root();
    let path = root.path().join("lash.db");
    drop(
        SqliteStore::open_file_for_testing(&path)
            .await
            .expect("provision"),
    );
    let connection = rusqlite::Connection::open(&path).expect("open stamp");
    connection
        .execute("UPDATE release_stamp SET release_version = ?1", [writing])
        .expect("stamp release");
    connection
        .execute(
            "UPDATE lash_compat SET version = ?1, min_reader = ?1, fleet_format = ?1",
            [counter],
        )
        .expect("stamp counters");
    // The open guard must refuse before changing DELETE mode to WAL.
    connection
        .execute_batch("PRAGMA journal_mode = DELETE")
        .expect("checkpoint and switch journal mode");
    drop(connection);
    let before = std::fs::read(&path).expect("original bytes");
    let target = crate::location::DatabaseTarget::File(path.clone());
    let row =
        super::verify_schema_target(&target, build, crate::SqliteOperationalSettings::standard())
            .await;
    let early = crate::conn::SqliteConnection::check_release_before_open(&target, build).await;
    let mut connection = rusqlite::Connection::open(&path).expect("open for admission");
    let tx = connection.transaction().expect("open transaction");
    let admission =
        crate::compat::admit_with_release(&tx, lash_core_execution::FleetFormat::writable(), build);
    if refuses {
        let expected = lash_core_execution::compat::CompatRefusal::PreRelease {
            component: "sqlite-core".into(),
            writing_release: Some(writing.into()),
        };
        assert_eq!(
            row.verdict,
            StoreSchemaVerdict::Refused {
                refusal: expected.clone()
            }
        );
        for error in [
            crate::sqlite_async_error(early.expect_err("refuse before journal setup")),
            crate::sqlite_error(admission.expect_err("refuse in open transaction")),
        ] {
            assert!(
                matches!(error, lash_core_execution::StoreError::Incompatible { refusal } if refusal == expected)
            );
        }
    } else {
        assert_eq!(row.verdict, StoreSchemaVerdict::Matches);
        early.expect("release admitted before setup");
        assert_eq!(
            admission.expect("admit open").0,
            lash_core_execution::compat::CompatAdmission::Native
        );
    }
    tx.rollback().expect("rollback");
    drop(connection);
    assert_eq!(std::fs::read(&path).expect("unchanged bytes"), before);
}
