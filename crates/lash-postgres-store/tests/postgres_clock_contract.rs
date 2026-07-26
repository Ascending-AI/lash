const RUNTIME_PERSISTENCE: &str = include_str!("../src/postgres/runtime_persistence.rs");
const PROCESS_REGISTRY: &str = include_str!("../src/postgres/process_registry.rs");

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
fn queued_work_and_pending_input_lease_decisions_use_the_postgres_clock() {
    let lease_sensitive_regions = [
        (
            "async fn claim_leading_ready_session_command(",
            "async fn claim_ready_queued_work(",
        ),
        (
            "async fn claim_ready_queued_work(",
            "async fn abandon_queued_work_claim(",
        ),
        (
            "async fn cancel_queued_work_batch(",
            "async fn list_queued_work(",
        ),
        (
            "async fn list_pending_queued_work(",
            "impl TurnInputStore for PostgresSessionStore",
        ),
        (
            "async fn list_pending_turn_inputs(",
            "async fn cancel_pending_turn_inputs(",
        ),
        (
            "async fn cancel_pending_turn_inputs(",
            "async fn cancel_pending_turn_input_suffix(",
        ),
        (
            "async fn cancel_pending_turn_input_suffix(",
            "async fn claim_active_turn_inputs(",
        ),
        (
            "async fn claim_pending_turn_inputs_postgres(",
            "struct SessionExecutionLeaseRow",
        ),
    ];

    for (start, end) in lease_sensitive_regions {
        let region = source_region(RUNTIME_PERSISTENCE, start, end);
        assert!(
            region.contains("postgres_transaction_epoch_ms"),
            "`{start}` must derive lease validity from PostgreSQL"
        );
        assert!(
            !region.contains("current_epoch_ms()"),
            "`{start}` must not derive lease validity from the client wall clock"
        );
    }
}

#[test]
fn process_lease_decisions_use_the_postgres_clock() {
    let lease_sensitive_regions = [
        (
            "async fn complete_process_with_lease(",
            "async fn record_first_started_with_authority(",
        ),
        (
            "async fn claim_process_lease(",
            "async fn reclaim_process_lease(",
        ),
        (
            "async fn reclaim_process_lease(",
            "async fn renew_process_lease(",
        ),
        (
            "async fn renew_process_lease(",
            "async fn get_process_lease(",
        ),
    ];

    for (start, end) in lease_sensitive_regions {
        let region = source_region(PROCESS_REGISTRY, start, end);
        assert!(
            region.contains("process_lease_now_epoch_ms_tx"),
            "`{start}` must derive lease validity from PostgreSQL"
        );
        assert!(
            !region.contains("current_epoch_ms()"),
            "`{start}` must not derive lease validity from the client wall clock"
        );
    }
}

#[test]
fn final_turn_commit_stamps_use_the_injected_store_clock() {
    let region = source_region(
        RUNTIME_PERSISTENCE,
        "async fn commit_runtime_state(",
        "async fn save_session_meta(",
    );
    assert!(
        region.contains("let now = self.clock.timestamp_ms();"),
        "final turn commit must sample the injected store clock"
    );
    assert!(
        !region.contains("current_epoch_ms()"),
        "final turn commit must not stamp from the client wall clock"
    );
}
