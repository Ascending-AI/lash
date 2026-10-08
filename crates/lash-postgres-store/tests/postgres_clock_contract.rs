//! Behavioral laws and lexical fences for PostgreSQL clock provenance.

use lash_sansio::{ProcessId, SessionId};
use std::sync::Arc;

use lash_core_execution::testing::TestClock;
use lash_core_execution::{
    Clock, PendingTurnInputCancelOutcome, PendingTurnInputCancelTarget, PendingTurnInputDraft,
    PendingTurnInputReadStatus, RuntimeCommit, RuntimeSessionState, SessionCatalogStore as _,
    SessionCommitStore, SessionCreationHead, SessionRelation, SessionStoreCreateRequest, TurnId,
    TurnInput, TurnInputIngress, TurnInputStore,
};
use lash_postgres_store::PostgresStorage;

// Keep subsequent lines stable for machine-checked public API evidence anchors.
// Shared test support now lives at the grouped integration-harness run.
use crate::support::{SharedDatabaseLock, database_url};

const CLOCK_SKEW_MS: u64 = 10 * 365 * 24 * 60 * 60 * 1_000;
const RUNTIME_PERSISTENCE_QUEUED_WORK_SOURCE: &str = concat!(
    include_str!("../src/postgres/runtime_persistence/queued_work.rs"),
    "\nimpl IngressStore for PostgresStore"
);
const RUNTIME_PERSISTENCE_ADMISSION_SOURCE: &str = concat!(
    include_str!("../src/postgres/runtime_persistence/admission.rs"),
    "\n// end of admission.rs"
);
const RUNTIME_PERSISTENCE_INGRESS_SETTLEMENT_SOURCE: &str = concat!(
    include_str!("../src/postgres/runtime_persistence/ingress_settlement.rs"),
    "\n// end of ingress_settlement.rs"
);
const RUNTIME_PERSISTENCE_TURN_INPUT_SOURCE: &str =
    include_str!("../src/postgres/runtime_persistence/turn_input.rs");
const RUNTIME_PERSISTENCE_SESSION_COMMIT_SOURCE: &str =
    include_str!("../src/postgres/runtime_persistence/session_commit.rs");
const PROCESS_HELPERS_SOURCE: &str = include_str!("../src/postgres/process_helpers.rs");
const PROCESS_REGISTRY_SOURCE: &str = include_str!("../src/postgres/process_registry.rs");
const PROCESS_LIFECYCLE_SOURCE: &str =
    include_str!("../src/postgres/process_registry/lifecycle.rs");

fn unique_id(prefix: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    format!("{prefix}-{}-{nonce}", std::process::id())
}

async fn db_now_ms(storage: &PostgresStorage) -> u64 {
    let now: i64 = sqlx::query_scalar(
        "SELECT floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint",
    )
    .fetch_one(storage.pool())
    .await
    .expect("read PostgreSQL transaction clock");
    now.max(0) as u64
}

async fn configured_storage(test_name: &str) -> Option<(SharedDatabaseLock, PostgresStorage)> {
    let Some(url) = database_url() else {
        eprintln!("skipping {test_name}: LASH_POSTGRES_DATABASE_URL is not set");
        return None;
    };
    let lock = SharedDatabaseLock::acquire(&url).await;
    let storage = lash_postgres_store::testing::connect(&url)
        .await
        .expect("connect PostgreSQL clock-contract storage");
    Some((lock, storage))
}

fn source_region<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start_index = source
        .find(start)
        .unwrap_or_else(|| panic!("missing source marker `{start}`"));
    let region = &source[start_index..];
    let end_index = region
        .find(end)
        .unwrap_or_else(|| panic!("missing source marker `{end}` after `{start}`"));
    &region[..end_index]
}

#[test]
fn lint_postgres_clock_contract_paths_never_use_client_wall_clock() {
    // This is deliberately a lexical fence, not a behavioral test: ADR-0044
    // recognizes that an in-process test cannot skew `SystemTime::now()`.
    let clock_sensitive_regions = [
        // Every admission path — run, checkpoint and the command run — and
        // the free functions that compose and bind their rows, through the
        // end of the file.
        (
            RUNTIME_PERSISTENCE_ADMISSION_SOURCE,
            "pub(crate) async fn admit_at_checkpoint_postgres(",
            "// end of admission.rs",
        ),
        // A commit's settlement of the rows its run admitted runs inside the
        // commit's transaction, on the same server clock.
        (
            RUNTIME_PERSISTENCE_INGRESS_SETTLEMENT_SOURCE,
            "async fn settle_commit_ingress_tx(",
            "// end of ingress_settlement.rs",
        ),
        (
            RUNTIME_PERSISTENCE_QUEUED_WORK_SOURCE,
            "async fn cancel_queued_work_batch_pg(",
            "async fn queued_work_batch_completion_pg(",
        ),
        (
            RUNTIME_PERSISTENCE_QUEUED_WORK_SOURCE,
            "async fn pending_session_work_ordering_pg(",
            "async fn list_open_queued_work_pg(",
        ),
        (
            RUNTIME_PERSISTENCE_QUEUED_WORK_SOURCE,
            "async fn list_open_queued_work_pg(",
            "impl IngressStore for PostgresStore",
        ),
        (
            RUNTIME_PERSISTENCE_TURN_INPUT_SOURCE,
            "async fn list_pending_turn_inputs(",
            "async fn cancel_pending_turn_inputs(",
        ),
        (
            RUNTIME_PERSISTENCE_TURN_INPUT_SOURCE,
            "async fn cancel_pending_turn_inputs(",
            "async fn cancel_pending_turn_input_suffix(",
        ),
        (
            RUNTIME_PERSISTENCE_TURN_INPUT_SOURCE,
            "async fn cancel_pending_turn_input_suffix(",
            "async fn enqueue_queued_work(",
        ),
        (
            RUNTIME_PERSISTENCE_SESSION_COMMIT_SOURCE,
            "async fn commit_runtime_state(",
            "async fn settle_observer_intents(",
        ),
        // The shared process-event append sequence stamps registry events
        // under the caller's store clock.
        (
            PROCESS_HELPERS_SOURCE,
            "async fn apply_process_event_append_tx(",
            "async fn append_process_event_tx(",
        ),
    ];

    // Every way this crate can read a host wall clock. `current_epoch_ms()` is
    // the crate's own helper; the other two are the ways around it.
    const CLIENT_CLOCK_READS: [&str; 3] =
        ["current_epoch_ms()", "SystemTime::now()", "SystemClock"];

    for (source, start, end) in clock_sensitive_regions {
        let region = source_region(source, start, end);
        for read in CLIENT_CLOCK_READS {
            assert!(
                !region.contains(read),
                "lexical clock fence: `{start}` must not use the client wall clock (`{read}`)"
            );
        }
    }
}

#[test]
fn lint_process_event_timestamps_use_the_injected_clock() {
    // Fence the complete entry-point files and shared append helpers so a
    // second timestamp source cannot hide in a new method or helper (ADR 0044).
    for (name, source) in [
        ("process_registry.rs", PROCESS_REGISTRY_SOURCE),
        ("process_registry/lifecycle.rs", PROCESS_LIFECYCLE_SOURCE),
        ("process_helpers.rs", PROCESS_HELPERS_SOURCE),
    ] {
        for read in [
            "process_registry_now_epoch_ms_tx",
            "select_statement_epoch_ms",
            "select_transaction_epoch_ms",
            "clock_timestamp()",
            "transaction_timestamp()",
            "statement_timestamp()",
            "current_epoch_ms()",
            "SystemTime::now()",
            "SystemClock",
        ] {
            assert!(
                !source.contains(read),
                "process-event clock fence: `{name}` must not read `{read}`"
            );
        }
    }
    for source in [PROCESS_REGISTRY_SOURCE, PROCESS_LIFECYCLE_SOURCE] {
        assert!(
            source.contains("self.clock.timestamp_ms()"),
            "process-event entry points must sample the injected registry clock"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_turn_commit_stamps_follow_the_injected_store_clock() {
    let Some((_lock, storage)) = configured_storage("final-turn injected-clock contract").await
    else {
        return;
    };
    const INJECTED_COMMIT_MS: u64 = 1_234_567_900_000;
    let session_id = unique_id("clock-contract-final-commit");
    let clock = Arc::new(TestClock::new(INJECTED_COMMIT_MS));
    let factory = storage
        .session_store_factory()
        .with_clock(clock as Arc<dyn Clock>);
    factory
        .admit_session(&SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::fixture(session_id.clone()),
            relation: SessionRelation::Root,
            config: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
                lash_core_execution::MaxToolCalls::new(1024),
            )
            .into(),
            head: SessionCreationHead::Config,
        })
        .await
        .expect("create final-commit session store");
    let store = factory;
    let state = RuntimeSessionState {
        session_id: SessionId::fixture(session_id.clone()),
        ..RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    };
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("commit runtime state with injected clock");
    let committed_at_ms: i64 = sqlx::query_scalar(
        "SELECT committed_at_ms FROM lash_runtime_turn_commits WHERE session_id = $1",
    )
    .bind(&session_id)
    .fetch_one(storage.pool())
    .await
    .expect("read persisted final-turn commit timestamp");
    assert_eq!(committed_at_ms, INJECTED_COMMIT_MS as i64);
}
