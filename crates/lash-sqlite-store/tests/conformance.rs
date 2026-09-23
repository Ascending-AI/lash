//! Runs the shared conformance suites against SQLite file deployments.
//!
//! The suite proper lives in `conformance/suite.rs` and is registered twice
//! (ADR 0102): here over file deployments, and in `conformance_memory.rs` over
//! named in-memory ones. What else stays here needs a database file by
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

use lash_sansio::ProcessId;
use std::path::Path;
use std::sync::Arc;

use lash_core_execution::{
    ProcessCompletionAuthority, ProcessEventAppendRequest, ProcessEventLog as _, ProcessInput,
    ProcessLifecycle as _, ProcessProvenance, ProcessRegistrar as _, ProcessRegistration,
    RecoveryContract,
};
use lash_sqlite_store::{
    SqliteEffectHost, SqliteProcessRegistry, SqliteRuntimeEffectController, SqliteTriggerStore,
};

#[path = "blob_probe.rs"]
mod blob_probe;
#[path = "conformance/deployment_fixture.rs"]
mod deployment_fixture;
#[path = "conformance/suite.rs"]
mod suite;

const SUBSTRATE: deployment_fixture::Substrate = deployment_fixture::Substrate::File;

#[path = "conformance/attachment_owner_kind.rs"]
mod attachment_owner_kind;
#[path = "conformance/cold_process_await_event.rs"]
mod cold_process_await_event;
#[path = "conformance/schema_refusal.rs"]
mod schema_refusal;
#[path = "conformance/turn_cancel_closure.rs"]
mod turn_cancel_closure;

use deployment_fixture::durable_turn_scope;
use lash_conformance::cold_process_turn_parent;

#[cfg(feature = "testing")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_event_page_identity_and_rows_share_one_read_snapshot() {
    use lash_core_execution::ProcessRetention as _;

    let dir = tempfile::tempdir().expect("process-event snapshot tempdir");
    let path = dir.path().join("process-event-snapshot.db");
    let sessions = dir.path().join("sessions");
    let injector = lash_sqlite_store::testing::SqliteFaultInjector::default();
    let reader = Arc::new(
        SqliteProcessRegistry::open_with_fault_injector_for_testing(
            &path,
            &sessions,
            injector.clone(),
        )
        .await
        .expect("open paused process registry reader"),
    );
    let writer = Arc::new(
        SqliteProcessRegistry::open(&path, &sessions)
            .await
            .expect("open competing process registry writer"),
    );
    let process_id = ProcessId::from("event-page-snapshot");
    reader
        .register_process(
            ProcessRegistration::new(
                process_id.clone(),
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                RecoveryContract::ExternallyOwned,
                ProcessProvenance::host(),
                lash_core_execution::ProcessLifecyclePolicy::new(
                    lash_core_execution::ParentScope::Host,
                    lash_core_execution::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([lash_core_execution::ProcessEventType {
                name: "snapshot.tail".to_string(),
                payload_schema: lash_core_execution::LashSchema::any(),
                semantics: lash_core_execution::ProcessEventSemanticsSpec::default(),
            }]),
        )
        .await
        .expect("register snapshot process");
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
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete snapshot process");
    let first = reader
        .event_page(
            &process_id,
            std::num::NonZeroUsize::MIN,
            lash_core_execution::ProcessEventQueryMode::Full,
            None,
        )
        .await
        .expect("read first event page");
    let lash_core_execution::ProcessEventReadOutcome::Retained(first) = first else {
        panic!("new process history must be retained");
    };
    let lash_core_execution::ProcessEventPageMore::More { continuation } = first.more else {
        panic!("fixture must leave a nonempty unread tail");
    };

    let pause = injector.pause_process_event_page_after_identity();
    let read_task = tokio::spawn({
        let reader = Arc::clone(&reader);
        let process_id = process_id.clone();
        async move {
            reader
                .event_page(
                    &process_id,
                    std::num::NonZeroUsize::new(16).expect("non-zero page size"),
                    lash_core_execution::ProcessEventQueryMode::Full,
                    Some(continuation),
                )
                .await
        }
    });
    pause.wait_until_reached().await;
    let prune = writer
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            lash_core_execution::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune process through competing connection");
    assert_eq!(prune.pruned_processes, 1);
    pause.release();

    let outcome = read_task
        .await
        .expect("join paused event-page read")
        .expect("event-page read result");
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

#[test]
fn conformance_invocation_lifecycle_control_is_consumable_cross_crate() {
    use lash_conformance::{ConformanceEffectRedrive, ConformanceInvocation};

    let invocation = ConformanceInvocation::native();
    assert_eq!(
        ConformanceInvocation::effect_redrive(&invocation),
        ConformanceEffectRedrive::ReexecutesUncommitted
    );
    let _journaled_redrive = ConformanceEffectRedrive::ReplaysJournal;
    let _controller = ConformanceInvocation::controller(&invocation);
    let _controller_handle = ConformanceInvocation::controller_handle(&invocation);
    let successor = ConformanceInvocation::redrive(invocation);
    ConformanceInvocation::end(successor);
}

#[test]
fn trigger_subscription_owner_filter_is_pushed_down() {
    lash_conformance::trigger_subscription_owner_filter_is_pushed_down(
        "SQLite",
        lash_sqlite_store::testing::trigger_subscription_list_sql,
    );
}

#[tokio::test]
async fn sqlite_effect_controller_rejects_pre_intent_journal_schema_before_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-canonical-envelope-effects.db");
    let conn = rusqlite::Connection::open(&path).expect("open legacy effect db");
    conn.pragma_update(None, "user_version", 8)
        .expect("stamp legacy effect schema");
    drop(conn);

    let error =
        match SqliteRuntimeEffectController::open(&path, durable_turn_scope("session", "turn"))
            .await
        {
            Ok(_) => panic!("pre-intent effect stores must be recreated"),
            Err(error) => error,
        };
    let message = error.to_string();
    assert!(message.contains("Unsupported lash effect replay schema"));
    assert!(message.contains("supports schema version 34"));
    assert!(message.contains("database reports version 8"));
    assert!(message.contains(
        "drain affected sessions and recreate the whole Lash trust domain with this version"
    ));
}

#[tokio::test]
async fn sqlite_effect_controller_rejects_retained_generation_21_schema_before_serving() {
    // Generation 21 is the pre-SleepSpec-cutover journal this fixture retains;
    // the boundary has since moved, and every stale stamp is refused alike.
    const RETAINED_PRIOR_EFFECT_GENERATION: i32 = 21;
    assert!(
        i64::from(RETAINED_PRIOR_EFFECT_GENERATION)
            < lash_sqlite_store::SqliteDatabase::EffectReplay.expected_version()
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("retained-generation-21-effects.db");
    let conn = rusqlite::Connection::open(&path).expect("open retained effect db");
    conn.pragma_update(None, "user_version", RETAINED_PRIOR_EFFECT_GENERATION)
        .expect("stamp retained prior effect schema");
    drop(conn);

    let error =
        match SqliteRuntimeEffectController::open(&path, durable_turn_scope("session", "turn"))
            .await
        {
            Ok(_) => panic!("retained prior effect stores must be recreated"),
            Err(error) => error,
        };
    let message = error.to_string();
    assert!(message.contains("Unsupported lash effect replay schema"));
    assert!(message.contains("supports schema version 34"));
    assert!(message.contains("database reports version 21"));
}

#[tokio::test]
async fn sqlite_effect_host_and_controller_reject_non_file_backed_path_spellings() {
    for path in [
        "",
        ":memory:",
        "file::memory:?cache=shared",
        "file:temporary",
    ] {
        let error = match SqliteEffectHost::open(Path::new(path)).await {
            Ok(_) => panic!("effect hosts must reject non-file-backed path {path:?}"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("requires a file-backed database path"),
            "unexpected error for {path:?}: {error}"
        );

        let error = match SqliteRuntimeEffectController::open(
            Path::new(path),
            durable_turn_scope("guard-session", "guard-turn"),
        )
        .await
        {
            Ok(_) => panic!("effect controllers must reject non-file-backed path {path:?}"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("requires a file-backed database path"),
            "unexpected controller error for {path:?}: {error}"
        );
    }
}

#[tokio::test]
async fn sqlite_effect_replay_satisfies_cold_process_crash_conformance() {
    use tokio::process::Command;

    let dir = tempfile::tempdir().expect("cold-process effect replay tempdir");
    let database = dir.path().join("cold-process-effect-replay.db");
    let marker = dir.path().join("external-effect.log");
    let nonce = uuid::Uuid::new_v4().to_string();
    let run = |action: &'static str| {
        let database = database.clone();
        let marker = marker.clone();
        let nonce = nonce.clone();
        async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                Command::new(lash_conformance::helper_executable(
                    "sqlite-await-event-helper",
                ))
                .arg(database)
                .arg(action)
                .arg(nonce)
                .arg(marker)
                .output(),
            )
            .await
            .unwrap_or_else(|_| panic!("{action} helper timed out"))
            .unwrap_or_else(|error| panic!("spawn {action} helper: {error}"))
        }
    };

    let crashed = run("effect_crash").await;
    assert_eq!(crashed.status.code(), Some(86));
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read crashed effect marker")
            .lines()
            .count(),
        1,
        "the external effect ran before the owner crashed"
    );

    let completed = run("effect_complete").await;
    assert!(
        completed.status.success(),
        "successor helper failed: {}",
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read re-executed effect marker")
            .lines()
            .count(),
        2,
        "an unrecorded external effect is honestly re-executed"
    );

    let replayed = run("effect_replay").await;
    assert!(
        replayed.status.success(),
        "replay helper failed: {}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read replay effect marker")
            .lines()
            .count(),
        2,
        "a recorded outcome replays without another external effect"
    );
}

#[tokio::test]
async fn sqlite_real_turn_satisfies_cold_process_crash_matrix() {
    let dir = tempfile::tempdir().expect("SQLite cold-process real-turn tempdir");
    let database = dir.path().join("cold-process-real-turn.db");
    cold_process_turn_parent::assert_real_turn_kill_recovery(
        dir.path(),
        |action, nonce, marker| {
            let mut command = tokio::process::Command::new(lash_conformance::helper_executable(
                "sqlite-await-event-helper",
            ));
            command.arg(&database).arg(action).arg(nonce).arg(marker);
            command
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_queued_run_satisfies_cold_process_persistence_boundaries() {
    let dir = tempfile::tempdir().expect("SQLite queued-run cold process tempdir");
    let database = dir.path().join("queued-run-cold.db");
    lash_conformance::assert_queued_run_cold_process_recovery(
        dir.path(),
        |action, nonce, marker| {
            let mut command = tokio::process::Command::new(lash_conformance::helper_executable(
                "sqlite-await-event-helper",
            ));
            command.arg(&database).arg(action).arg(nonce).arg(marker);
            command
        },
    )
    .await;
}
