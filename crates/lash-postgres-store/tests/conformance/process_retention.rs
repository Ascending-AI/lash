//! Process-retention laws: the live-status contract and the SQL literals
//! that spell it.

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

/// A waiting process is live, not prunable. The PostgreSQL half of
/// `sqlite_waiting_processes_are_live_not_prunable`: both backends spell
/// `LIVE_PROCESS_STATUS_LABELS` out as SQL literals, so both need the
/// behavioural referee.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_waiting_processes_are_live_not_prunable_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping PostgreSQL waiting-retention regression: database URL is not set");
        return;
    };
    reset(&storage).await;
    let registry = storage.process_registry();
    let process_id = format!("waiting-retention:{}", uuid::Uuid::new_v4());
    assert_waiting_process_is_live_not_prunable(&registry, &process_id).await;
}

/// Lexical half of the retention contract, mirroring
/// `sqlite_status_list_literals_derive_from_the_shared_constant`: every
/// `status IN`/`status NOT IN` literal in this backend's SQL must spell
/// exactly the label list its site calls for, so a grown constant with a stale
/// SQL literal fails here instead of silently pruning live rows.
///
/// Retention queries render `LIVE_PROCESS_STATUS_LABELS`; the DDL's
/// `ck_processes_status` renders live plus retired, because the constraint
/// admits the whole durable vocabulary (ADR 0081) rather than the live subset.
#[test]
fn postgres_status_list_literals_derive_from_the_shared_constant() {
    let render = |labels: &[&str]| {
        format!(
            "({})",
            labels
                .iter()
                .map(|label| format!("'{label}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let live_labels = lash_core::facade_support::registry_transitions::LIVE_PROCESS_STATUS_LABELS;
    let retired_labels =
        lash_core::facade_support::registry_transitions::RETIRED_PROCESS_STATUS_LABELS;
    let live = render(&live_labels);
    let vocabulary = render(
        &live_labels
            .iter()
            .chain(retired_labels.iter())
            .copied()
            .collect::<Vec<_>>(),
    );
    // Two DDL constraints spell a `status IN` list. `ck_processes_status` is this
    // law's subject. `ck_runtime_effect_replay_status` is a different column's
    // vocabulary (`EffectRowStatus`), pinned by the lash-sim congruence registry
    // and its writer-vocabulary law, so it is counted and skipped here rather
    // than silently swept into the process-status expectation.
    const VOCABULARY_SITE: &str = "CONSTRAINT ck_processes_status CHECK (";
    const FOREIGN_VOCABULARY_SITE: &str = "CONSTRAINT ck_runtime_effect_replay_status CHECK (";
    let sources = [
        (
            "process_registry.rs",
            include_str!("../../src/postgres/process_registry.rs"),
        ),
        ("schema.sql", include_str!("../../schema.sql")),
    ];
    let mut live_sites = 0usize;
    let mut vocabulary_sites = 0usize;
    let mut foreign_sites = 0usize;
    for (name, source) in sources {
        for delimiter in ["status IN ", "status NOT IN "] {
            for (offset, _) in source.match_indices(delimiter) {
                let site = &source[offset + delimiter.len()..];
                let prefix = &source[..offset];
                if delimiter == "status IN " && prefix.ends_with(FOREIGN_VOCABULARY_SITE) {
                    foreign_sites += 1;
                    continue;
                }
                let is_vocabulary = delimiter == "status IN " && prefix.ends_with(VOCABULARY_SITE);
                let (expected, constant) = if is_vocabulary {
                    (&vocabulary, "the live-plus-retired vocabulary")
                } else {
                    (&live, "LIVE_PROCESS_STATUS_LABELS")
                };
                assert!(
                    site.starts_with(expected.as_str()),
                    "{name}: a `{delimiter}` list literal diverged from {constant}: \
                     expected {expected}, found {}",
                    &site[..site.len().min(80)]
                );
                if is_vocabulary {
                    vocabulary_sites += 1;
                } else {
                    live_sites += 1;
                }
            }
        }
    }
    assert_eq!(
        live_sites, 3,
        "expected exactly three live-status list literal sites in the PostgreSQL backend \
         (two registry queries plus the partial process index in schema.sql); \
         update this count when adding one"
    );
    assert_eq!(
        foreign_sites, 1,
        "expected exactly 1 `ck_runtime_effect_replay_status` vocabulary literal, \
         which the lash-sim congruence registry owns"
    );
    assert_eq!(
        vocabulary_sites, 1,
        "expected exactly one `ck_processes_status` vocabulary literal in schema.sql"
    );
}
