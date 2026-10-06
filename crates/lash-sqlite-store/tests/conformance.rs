//! Runs the shared conformance suites against SQLite file backends.
//!
//! The suite proper lives in `conformance/suite.rs` and is registered twice
//! (ADR 0102): here over file store sets, and in `conformance_memory.rs` over
//! named memory store sets. What else stays here needs a database file by
//! nature: a legacy schema seeded before the first open, a path spelling, a
//! second OS process, or a WAL snapshot read.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use lash_core_execution::{
    ProcessCompletionAuthority, ProcessEventAppendRequest, ProcessEventLog as _,
    ProcessLifecycle as _, ProcessProvenance, ProcessRegistrar as _,
};
use lash_sqlite_store::{SqliteProcessRegistry, SqliteTriggerStore};

#[path = "conformance/backend_fixture.rs"]
mod backend_fixture;
#[path = "blob_probe.rs"]
mod blob_probe;
#[path = "conformance/session_history.rs"]
mod session_history;
#[path = "conformance/suite.rs"]
mod suite;

const SUBSTRATE: backend_fixture::Substrate = backend_fixture::Substrate::File;

#[path = "conformance/schema_refusal.rs"]
mod schema_refusal;

#[cfg(feature = "testing")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_event_page_identity_and_rows_share_one_read_snapshot() {
    let dir = tempfile::tempdir().expect("process-event snapshot tempdir");
    let path = dir.path().to_path_buf();
    let pauses = lash_sqlite_store::testing::SqlitePauses::default();
    let stores = lash_sqlite_store::SqliteStoreSet::open_with_options_and_clock(
        &path,
        lash_sqlite_store::SqliteStoreSetOptions {
            pauses: Some(pauses.clone()),
            ..Default::default()
        },
        Arc::new(lash_core_execution::facade_support::SystemClock),
    )
    .await
    .expect("open paused process registry reader");
    let reader = stores.process_registry();
    let process_id = reader
        .register_process(
            lash_core::testing::held_engine_registration(
                serde_json::Value::Null,
                ProcessProvenance::host(),
                lash_core_execution::Lifetime::Detached,
            )
            .with_extra_event_types([lash_core_execution::ProcessEventType {
                name: "snapshot.tail".to_string(),
                payload_schema: lash_core_execution::JsonSchema::any(),
                semantics: lash_core_execution::ProcessEventSemanticsSpec::default(),
            }]),
        )
        .await
        .expect("register snapshot process")
        .id;
    for sequence in 0..3 {
        reader
            .append_event(
                &process_id,
                ProcessEventAppendRequest::new(
                    "snapshot.tail",
                    serde_json::json!({ "sequence": sequence }),
                ),
            )
            .await
            .expect("append unread event tail");
    }
    let terminal = reader
        .complete_process(
            &process_id,
            lash_core_execution::ProcessAwaitOutput::from_tool_output(
                lash_core_execution::ToolCallOutput::success(serde_json::Value::Null),
            ),
            ProcessCompletionAuthority::workflow_key(&process_id),
        )
        .await
        .expect("complete snapshot process");
    let first = reader
        .event_page(
            &process_id,
            std::num::NonZeroUsize::MIN,
            lash_core_execution::ProcessEventQueryMode::Full,
        )
        .await
        .expect("read first event page");
    let lash_core_execution::ProcessEventReadOutcome::Retained(first) = first else {
        panic!("new process history must be retained");
    };
    let lash_core_execution::ProcessEventPageMore::More { after_sequence } = first.more else {
        panic!("fixture must leave a nonempty unread tail");
    };

    let pause = pauses.pause_process_event_page_after_identity();
    let read_task = tokio::spawn({
        let reader = Arc::clone(&reader);
        let process_id = process_id.clone();
        async move {
            reader
                .event_page_after(
                    &process_id,
                    after_sequence,
                    std::num::NonZeroUsize::new(16).expect("non-zero page size"),
                    lash_core_execution::ProcessEventQueryMode::Full,
                )
                .await
        }
    });
    pause.wait_until_reached().await;
    // The competing writer is another OS process's connection, as in a
    // deployment: it shares none of this process's SQLite gates, so a
    // checkpoint queued behind the paused read cannot hold the prune back.
    let cutoff_ms = terminal.updated_at_ms.saturating_add(1);
    let pruner = tokio::task::spawn_blocking({
        let path = path.clone();
        move || {
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "process_event_snapshot_competing_pruner",
                    "--include-ignored",
                    "--nocapture",
                ])
                .env(SNAPSHOT_PRUNER_ROOT, &path)
                .env(SNAPSHOT_PRUNER_CUTOFF_MS, cutoff_ms.to_string())
                .output()
                .expect("run the competing pruner")
        }
    })
    .await
    .expect("join the competing pruner");
    pause.release();
    let outcome = read_task
        .await
        .expect("join paused event-page read")
        .expect("event-page read result");
    let stdout = String::from_utf8_lossy(&pruner.stdout);
    assert!(
        pruner.status.success() && stdout.contains("pruned_processes=1"),
        "the competing connection prunes the process: {stdout}\n{}",
        String::from_utf8_lossy(&pruner.stderr)
    );
    assert!(
        !matches!(
            outcome,
            lash_core_execution::ProcessEventReadOutcome::Retained(lash_core_execution::ProcessEventPage {
                events: lash_core_execution::ProcessEventPageEvents::Full(ref events),
                more: lash_core_execution::ProcessEventPageMore::Complete,
            }) if events.is_empty()
        ),
        "a prune between identity lookup and page fetch must not become false empty completion: {outcome:?}"
    );
}

const SNAPSHOT_PRUNER_ROOT: &str = "LASH_SQLITE_SNAPSHOT_PRUNER_ROOT";
const SNAPSHOT_PRUNER_CUTOFF_MS: &str = "LASH_SQLITE_SNAPSHOT_PRUNER_CUTOFF_MS";

/// The competing writer of `process_event_page_identity_and_rows_share_one_read_snapshot`:
/// prune the law's store set from this process's own connections.
#[cfg(feature = "testing")]
#[tokio::test]
#[ignore = "spawned by process_event_page_identity_and_rows_share_one_read_snapshot"]
async fn process_event_snapshot_competing_pruner() {
    use lash_core_execution::ProcessRetention as _;

    let root = std::env::var_os(SNAPSHOT_PRUNER_ROOT).expect("the law's store root");
    let cutoff_ms = std::env::var(SNAPSHOT_PRUNER_CUTOFF_MS)
        .expect("the law's prune cutoff")
        .parse()
        .expect("a millisecond cutoff");
    let stores = lash_sqlite_store::SqliteStoreSet::open(&root)
        .await
        .expect("open competing process registry writer");
    let prune = stores
        .process_registry()
        .prune_terminal_processes(
            cutoff_ms,
            None,
            lash_core_execution::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune process through competing connection");
    println!("pruned_processes={}", prune.pruned_processes);
}

#[test]
fn trigger_subscription_owner_filter_is_pushed_down() {
    lash_conformance::trigger_subscription_owner_filter_is_pushed_down(
        "SQLite",
        lash_sqlite_store::testing::trigger_subscription_list_sql,
    );
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn fenced_process_and_trigger_registration_stays_typed() {
    let root = tempfile::tempdir().expect("store root");
    let stores = lash_sqlite_store::SqliteStoreSet::open(root.path())
        .await
        .expect("open older writer");
    // An epoch past this build's writable range: a newer release finalized.
    let newer = lash_core_execution::FleetFormat::writable().max() + 1;
    lash_sqlite_store::testing::finalize_fleet_format(stores.location(), newer)
        .expect("finalize newer fleet format");
    let snapshot = || {
        let mut rows = std::collections::BTreeMap::new();
        for database in [
            lash_sqlite_store::SqliteDatabase::DurableCore,
            lash_sqlite_store::SqliteDatabase::ProcessRegistry,
            lash_sqlite_store::SqliteDatabase::Triggers,
        ] {
            let connection = rusqlite::Connection::open(root.path().join(database.file_name()))
                .expect("open snapshot");
            let tables: Vec<String> = connection
                .prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")
                .expect("list tables")
                .query_map([], |row| row.get(0))
                .expect("query tables")
                .collect::<rusqlite::Result<_>>()
                .expect("table names");
            for table in tables {
                let count: i64 = connection
                    .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |row| {
                        row.get(0)
                    })
                    .expect("count rows");
                rows.insert(format!("{}.{table}", database.name()), count);
            }
        }
        rows
    };
    let before = snapshot();
    lash_conformance::fenced_process_and_trigger_registration_stays_typed(
        stores.process_registry(),
        stores.trigger_store(),
    )
    .await;
    assert_eq!(snapshot(), before, "fenced writers changed rows");
}
