//! Regression tests for the storage review fixes:
//!
//! * head-revision CAS holds across two independent connections to the same
//!   file database (the `BEGIN IMMEDIATE` fix),
//! * a contended queued-work admission has exactly one winner instead of a
//!   false success (the rows-affected check),
//! * a poisoned connection mutex recovers instead of bricking the store,
//! * an unsupported compatibility floor reports the recorded version and reader range,
//! * concurrent first opens do not expose an unstamped store.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::ProcessId;
use std::future::Future;
use std::sync::Arc;

use lash_core_store::attachments::AttachmentRootSet;
use lash_core_store::store::AttachmentReferrers;
use lash_core_store::store::SessionCatalogStore;
use lash_core_store::store::StoreSchemaVerdict;
use lash_sqlite_store::{SqliteStore, verify_schema_at};

fn unique_db_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lash-storage-fixes-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("session.db")
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

#[test]
fn concurrent_first_open_never_observes_an_unstamped_schema() {
    let path = unique_db_path("concurrent-schema");
    let workers = 16;
    let barrier = Arc::new(std::sync::Barrier::new(workers));
    let handles = (0..workers)
        .map(|_| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                block_on(SqliteStore::open_file_for_testing(&path))
                    .map(|_| ())
                    .map_err(|err| err.to_string())
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle
            .join()
            .expect("schema-open worker")
            .expect("concurrent first open should succeed");
    }
    let conn = rusqlite::Connection::open(&path).expect("open initialized db");
    let stamp: (String, i64, i64, i64) = conn
        .query_row(
            "SELECT component, version, min_reader, fleet_format FROM lash_compat WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read compatibility stamp");
    // A provisioned database records the version this build writes, the
    // oldest it reads as the floor, and the epoch an installer seeds.
    let core = crate::sqlite_core();
    assert_eq!(
        stamp,
        (
            "sqlite-core".to_owned(),
            i64::from(core.writes.max()),
            i64::from(core.reads.min()),
            i64::from(
                lash_core_store::store::FleetFormat::seed(
                    lash_core_store::store::FleetFormat::writable()
                )
                .version()
            ),
        )
    );
}

#[tokio::test]
async fn process_record_is_a_root_without_registry_liveness() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SqliteStore::open(
        &dir.path().join("sessions.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .expect("open catalog");
    let request = lash_core_store::session_store_factory_types::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("unwired-process-owner"),
        relation: lash_core_store::session_identity::SessionRelation::default(),
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: lash_core_store::session_store_factory_types::SessionCreationHead::Config,
        retention: lash_core_store::store::Retention::UntilGc,
    };
    store.admit_session(&request).await.expect("admit session");
    let attachment_id = lash_protocol_types::attachment::AttachmentId::parse("a1".repeat(32))
        .expect("valid attachment id");
    let intent = lash_core_store::store::AttachmentWrite {
        attachment_id: attachment_id.clone(),
        claim: lash_core_store::artifact_referrer::ReferrerClaim::unguarded(
            lash_core_store::artifact_referrer::ArtifactReferrer::ProcessRecord(
                ProcessId::fixture("lasting-process"),
            ),
        )
        .expect("claim"),
    };
    let lash_core_store::store::AttachmentWriteFence::Granted(permit) = store
        .begin_attachment_write(&(intent.clone()))
        .await
        .expect("begin process-owned write")
    else {
        panic!("a free digest must grant its writer");
    };
    store
        .complete_attachment_write(&intent, permit)
        .await
        .expect("stamp process-owned upload");

    let refs = store
        .live_attachment_refs()
        .await
        .expect("unwired GC root scan");
    assert!(refs.contains(&attachment_id));
}

#[tokio::test]
async fn plugin_state_cutover_refuses_snapshot_predecessor_without_mutation() {
    let path = unique_db_path("plugin-state-predecessor");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("provision current schema");
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("UPDATE lash_compat SET version = 51, min_reader = 51", [])
        .unwrap();
    let before: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    drop(conn);
    let found = verify_schema_at(&path).await;
    assert_eq!(
        found.verdict,
        StoreSchemaVerdict::Refused {
            refusal: crate::retired_core_refusal(51),
        }
    );
    assert!(SqliteStore::open_file_for_testing(&path).await.is_err());
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("SELECT version FROM lash_compat", [], |row| row.get(0))
        .unwrap();
    let after: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 51);
    assert_eq!(before, after);
}
