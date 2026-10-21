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

use lash_execution_contracts::process::ProcessCompletionAuthority;
use lash_execution_contracts::process::ProcessEventLog as _;
use lash_execution_contracts::process::ProcessLifecycle as _;
use lash_execution_contracts::process::ProcessProvenance;
use lash_execution_contracts::process::ProcessRegistrar as _;
use lash_sqlite_store::SqliteProcessRegistry;

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

/// A file store set whose connections carry `pauses`, holding one terminal
/// process with a nonempty retained event tail: the process's id and the
/// prune cutoff that makes it eligible.
#[cfg(feature = "testing")]
async fn terminal_process_with_a_tail(
    path: &std::path::Path,
    pauses: &lash_sqlite_store::testing::SqlitePauses,
) -> (
    lash_sqlite_store::SqliteStoreSet,
    lash_vocabulary::identity::ProcessId,
    u64,
) {
    let stores = lash_sqlite_store::SqliteStoreSet::open_with_options_and_clock(
        path.join("lash.db"),
        lash_sqlite_store::SqliteStoreSetOptions {
            pauses: Some(pauses.clone()),
            ..lash_sqlite_store::SqliteStoreSetOptions::standard(
                lash_sqlite_store::SqliteSynchronous::Normal,
            )
        },
        Arc::new(lash_core_ids::clock::SystemClock),
    )
    .await
    .expect("open paused process registry reader");
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    let reader = stores.process_registry();
    let process_id = reader
        .register_process(lash_execution_contracts::testing::held_engine_registration(
            serde_json::Value::Null,
            ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        ))
        .await
        .expect("register snapshot process")
        .id;
    reader
        .request_process_cancel(
            &process_id,
            lash_protocol_types::tool_output::CancelOrigin::OperatorRequested,
            "snapshot-operator".to_owned(),
            None,
        )
        .await
        .expect("record lifecycle cancellation");
    let terminal = reader
        .complete_process(
            &process_id,
            lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                lash_protocol_types::tool_output::ToolCallOutput::success(serde_json::Value::Null),
            ),
            ProcessCompletionAuthority::workflow_key(&process_id),
        )
        .await
        .expect("complete snapshot process");
    (stores, process_id, terminal.updated_at_ms.saturating_add(1))
}

/// Prune the store set under `path` from another OS process's connection,
/// as in a deployment: it shares none of this process's SQLite gates, so a
/// checkpoint queued behind a paused read cannot hold the prune back.
#[cfg(feature = "testing")]
async fn prune_from_another_process(path: &std::path::Path, cutoff_ms: u64) {
    let pruner = tokio::task::spawn_blocking({
        let path = path.to_path_buf();
        move || {
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "lifecycle_snapshot_competing_pruner",
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
    let stdout = String::from_utf8_lossy(&pruner.stdout);
    assert!(
        pruner.status.success() && stdout.contains("pruned_processes=1"),
        "the competing connection prunes the process: {stdout}\n{}",
        String::from_utf8_lossy(&pruner.stderr)
    );
}

#[cfg(feature = "testing")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lifecycle_page_read_keeps_its_identity_snapshot_during_prune() {
    let dir = tempfile::tempdir().expect("process-event snapshot tempdir");
    let pauses = lash_sqlite_store::testing::SqlitePauses::default();
    let (stores, process_id, cutoff_ms) = terminal_process_with_a_tail(dir.path(), &pauses).await;
    let reader = stores.process_registry();
    let first = reader
        .event_page(
            &process_id,
            std::num::NonZeroUsize::MIN,
            lash_execution_contracts::process::ProcessEventQueryMode::Full,
        )
        .await
        .expect("read first event page");
    let lash_execution_contracts::process::ProcessEventReadOutcome::Retained(first) = first else {
        panic!("new process history must be retained");
    };
    let lash_execution_contracts::process::ProcessEventPageMore::More { after_sequence } =
        first.more
    else {
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
                    lash_execution_contracts::process::ProcessEventQueryMode::Full,
                )
                .await
        }
    });
    pause.wait_until_reached().await;
    prune_from_another_process(dir.path(), cutoff_ms).await;
    pause.release();
    let outcome = read_task
        .await
        .expect("join paused event-page read")
        .expect("event-page read result");
    assert!(
        !matches!(
            outcome,
            lash_execution_contracts::process::ProcessEventReadOutcome::Retained(lash_execution_contracts::process::ProcessEventPage {
                events: lash_execution_contracts::process::ProcessEventPageEvents::Full(ref events),
                more: lash_execution_contracts::process::ProcessEventPageMore::Complete,
            }) if events.is_empty()
        ),
        "a prune between identity lookup and page fetch must not become false empty completion: {outcome:?}"
    );
}

/// COMPOUND-READ-SNAPSHOT: the identity check and the event tail of
/// `recent_events` are one snapshot, so a prune committed between them
/// cannot turn a retained tail into an empty success.
#[cfg(feature = "testing")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recent_event_tail_keeps_its_identity_snapshot_during_prune() {
    let dir = tempfile::tempdir().expect("recent-events snapshot tempdir");
    let pauses = lash_sqlite_store::testing::SqlitePauses::default();
    let (stores, process_id, cutoff_ms) = terminal_process_with_a_tail(dir.path(), &pauses).await;
    let reader = stores.process_registry();
    let tail = reader
        .recent_events(&process_id, 16)
        .await
        .expect("read the retained tail");
    assert!(!tail.is_empty(), "fixture must retain a nonempty tail");

    let pause = pauses
        .pause_compound_read(lash_sqlite_store::testing::SqliteCompoundRead::RecentProcessEvents);
    let read_task = tokio::spawn({
        let reader = Arc::clone(&reader);
        let process_id = process_id.clone();
        async move { reader.recent_events(&process_id, 16).await }
    });
    pause.wait_until_reached().await;
    prune_from_another_process(dir.path(), cutoff_ms).await;
    pause.release();
    let read = read_task
        .await
        .expect("join paused recent-events read")
        .expect("a prune after the identity check is no failure");
    assert_eq!(
        read, tail,
        "the identity and its event tail come from one snapshot"
    );
    assert!(
        matches!(
            reader.recent_events(&process_id, 16).await,
            Err(
                lash_execution_contracts::plugin_error::PluginError::ProcessNoLongerRetained { .. }
            )
        ),
        "the next read sees the prune"
    );
}

const SNAPSHOT_PRUNER_ROOT: &str = "LASH_SQLITE_SNAPSHOT_PRUNER_ROOT";
const SNAPSHOT_PRUNER_CUTOFF_MS: &str = "LASH_SQLITE_SNAPSHOT_PRUNER_CUTOFF_MS";

/// The competing writer of `prune_from_another_process`: prune the law's
/// store set from this process's own connections.
#[cfg(feature = "testing")]
#[tokio::test]
#[ignore = "spawned by prune_from_another_process"]
async fn lifecycle_snapshot_competing_pruner() {
    use lash_execution_contracts::process::ProcessRetention as _;

    let root = std::env::var_os(SNAPSHOT_PRUNER_ROOT).expect("the law's store root");
    let cutoff_ms = std::env::var(SNAPSHOT_PRUNER_CUTOFF_MS)
        .expect("the law's prune cutoff")
        .parse()
        .expect("a millisecond cutoff");
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        std::path::Path::new(&root).join("lash.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .expect("open competing process registry writer");
    let prune = stores
        .process_registry()
        .prune_terminal_processes(
            cutoff_ms,
            None,
            lash_execution_contracts::process::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune process through competing connection");
    println!("pruned_processes={}", prune.pruned_processes);
}

#[path = "conformance/attachment_fail_closed.rs"]
mod attachment_fail_closed;
