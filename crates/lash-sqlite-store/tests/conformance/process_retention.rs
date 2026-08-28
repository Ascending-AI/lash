use super::*;

/// Drive one process into `waiting` and assert the retention contract: live rows
/// are listed as non-terminal and are never prune candidates.
async fn assert_waiting_process_is_live_not_prunable(
    registry: &dyn ProcessRegistry,
    process_id: &str,
) {
    registry
        .register_process(lash_core::ProcessRegistration::new(
            process_id,
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::Rerunnable,
            lash_core::ProcessProvenance::host(),
        ))
        .await
        .expect("register waiting retention process");
    let authority =
        lash_core::ProcessExecutionWriteAuthority::invocation(process_id, "waiting-retention-run")
            .bind_attempt(1);
    let started = authority
        .invocation_started()
        .expect("invocation authority carries its start fact");
    registry
        .record_first_started_with_authority(process_id, started, &authority)
        .await
        .expect("start waiting retention process");
    let waiting = registry
        .set_process_wait_with_authority(
            process_id,
            lash_core::WaitState {
                since_ms: 1,
                kind: lash_core::WaitKind::Signal {
                    name: "retention".to_string(),
                    event_type: "retention.signal".to_string(),
                    key: format!("{process_id}:wait"),
                    ordinal: 1,
                },
            },
            &authority,
        )
        .await
        .expect("enter wait");
    assert_eq!(
        waiting.status.label(),
        "waiting",
        "the wait must land in the persisted status label the retention SQL reads"
    );
    assert!(!waiting.is_terminal(), "a waiting process is not terminal");

    let live = registry
        .list_non_terminal_page(
            std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
            None,
        )
        .await
        .expect("list non-terminal processes")
        .records;
    assert!(
        live.iter().any(|record| record.id == process_id),
        "a waiting process must be listed as live"
    );

    let report = registry
        .prune_terminal_processes(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune terminal processes");
    assert_eq!(
        report.pruned_processes, 0,
        "a waiting process must never be a prune candidate, whatever the cutoff"
    );
    assert!(
        registry
            .get_process(process_id)
            .await
            .expect("read waiting retention process")
            .is_some(),
        "the waiting process row must survive the prune"
    );
}

/// A waiting process is live, not prunable.
///
/// `lash_core::facade_support::registry_transitions::LIVE_PROCESS_STATUS_LABELS`
/// is the shared retention contract, but this backend's SQL spells the label set
/// out as `status IN ('running', 'waiting')` and `status NOT IN (…)`. The law test
/// in core proves the constant partitions `ProcessStatus`; this is the
/// behavioural half, which is what fails if the SQL literals stop agreeing with
/// it and a live waiting process becomes prune-eligible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_waiting_processes_are_live_not_prunable() {
    let dir = tempfile::tempdir().expect("waiting retention tempdir");
    let registry = SqliteProcessRegistry::open(
        &dir.path().join("processes.db"),
        dir.path().join("sessions"),
    )
    .await
    .expect("open waiting retention registry");
    let process_id = format!("waiting-retention:{}", uuid::Uuid::new_v4());
    assert_waiting_process_is_live_not_prunable(&registry, &process_id).await;
}

/// Lexical half of the retention contract: every `status IN`/`status NOT IN`
/// literal in this backend's SQL must spell exactly the label list rendered
/// from `LIVE_PROCESS_STATUS_LABELS`. The partition law proves the
/// constant tracks `ProcessStatus`; the behavioural referee above proves
/// today's labels retain; this closes the remaining gap where a future label
/// grows the constant while a stale SQL literal silently prunes live rows.
#[test]
fn sqlite_status_list_literals_derive_from_the_shared_constant() {
    let expected = format!(
        "({})",
        lash_core::facade_support::registry_transitions::LIVE_PROCESS_STATUS_LABELS
            .iter()
            .map(|label| format!("'{label}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let sources = [
        (
            "process_registry.rs",
            include_str!("../../src/process_registry.rs"),
        ),
        (
            "process_registry_change.rs",
            include_str!("../../src/process_registry_change.rs"),
        ),
    ];
    let mut total = 0usize;
    for (name, source) in sources {
        for delimiter in ["status IN ", "status NOT IN "] {
            for site in source.split(delimiter).skip(1) {
                assert!(
                    site.starts_with(&expected),
                    "{name}: a `{delimiter}` list literal diverged from \
                     LIVE_PROCESS_STATUS_LABELS: expected {expected}, found {}",
                    &site[..site.len().min(40)]
                );
                total += 1;
            }
        }
    }
    assert_eq!(
        total, 1,
        "expected exactly one status-list literal site in the SQLite backend; \
         update this count (and the derivation check) when adding one"
    );
}
