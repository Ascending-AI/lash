use super::*;
use lash_core::ProcessRetention as _;
use lash_sansio::ProcessId;

/// Drive one process into `waiting` and assert the retention contract: live rows
/// are listed as non-terminal and are never prune candidates.
async fn assert_waiting_process_is_live_not_prunable(
    registry: &dyn ProcessRegistry,
    process_id: &ProcessId,
) {
    registry
        .register_process(lash_core::ProcessRegistration::new(
            process_id,
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::Rerunnable,
            lash_core::ProcessProvenance::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
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
    let process_id = ProcessId::from(format!("waiting-retention:{}", uuid::Uuid::new_v4()));
    assert_waiting_process_is_live_not_prunable(&registry, &process_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_prune_cleanup_evidence_survives_reopen_until_acknowledged() {
    let dir = tempfile::tempdir().expect("process cleanup tempdir");
    let database = dir.path().join("processes.db");
    let sessions = dir.path().join("sessions");
    let registry = SqliteProcessRegistry::open(&database, &sessions)
        .await
        .expect("open process registry");
    let registered = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "sqlite-prune-cleanup",
                lash_core::ProcessInput::Engine {
                    kind: "test-engine".to_string(),
                    payload: serde_json::json!({"module_ref": "module-sqlite"}),
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(lash_core::ProcessExecutionEnvRef::new(
                "process-env:sqlite-cleanup",
            ))),
        )
        .await
        .expect("register cleanup process");
    registry
        .complete_process(
            &registered.id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::workflow_key("sqlite-prune-cleanup"),
        )
        .await
        .expect("complete cleanup process");
    registry
        .prune_terminal_processes(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune with atomic cleanup evidence");
    drop(registry);

    let reopened = SqliteProcessRegistry::open(&database, &sessions)
        .await
        .expect("reopen process registry");
    let pending = reopened
        .pending_process_artifact_cleanup()
        .await
        .expect("read cleanup evidence after reopen");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].process_id, registered.id);
    assert_eq!(pending[0].env_ref, registered.env_ref);
    assert_eq!(pending[0].input, registered.input);
    let acknowledgement = reopened
        .complete_process_artifact_cleanup(&registered.id, registered.incarnation)
        .await
        .expect("ack cleanup evidence");
    assert_eq!(
        acknowledgement,
        lash_core::ProcessArtifactCleanupAck::Acknowledged {
            process_ref: lash_core::ProcessRef::from_record(&registered),
        }
    );
    assert!(
        reopened
            .pending_process_artifact_cleanup()
            .await
            .expect("read acknowledged cleanup")
            .is_empty()
    );
}

/// Lexical half of the retention contract: every `status IN`/`status NOT IN`
/// literal in this backend's SQL must spell exactly the label list its site
/// calls for. The partition law proves the constants track `ProcessStatus`; the
/// behavioural referee above proves today's labels retain; this closes the
/// remaining gap where a future label grows a constant while a stale SQL
/// literal silently prunes live rows.
///
/// Two site kinds, deliberately distinguished rather than merged. A retention
/// query selects **live** rows, so its literal renders
/// `LIVE_PROCESS_STATUS_LABELS`. The DDL's `ck_processes_status` admits the
/// **whole** durable vocabulary (ADR 0081), so its literal renders live plus
/// retired — the same partition, unioned. Holding them to one list would either
/// let retention prune a live row or make the CHECK reject a legal status.
#[test]
fn sqlite_status_list_literals_derive_from_the_shared_constant() {
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
            include_str!("../../src/process_registry.rs"),
        ),
        (
            "process_registry_change.rs",
            include_str!("../../src/process_registry_change.rs"),
        ),
        ("schema.rs", include_str!("../../src/schema.rs")),
    ];
    let mut live_sites = 0usize;
    let mut parameterized_sites = 0usize;
    let mut vocabulary_sites = 0usize;
    let mut foreign_sites = 0usize;
    for (name, source) in sources {
        for delimiter in ["status IN ", "status NOT IN "] {
            for (offset, _) in source.match_indices(delimiter) {
                let site = &source[offset + delimiter.len()..];
                // Caller-selected status sets are a bound query expression,
                // not a hard-coded live-status or DDL vocabulary literal.
                if delimiter == "status IN "
                    && (site.starts_with("(SELECT value FROM json_each(?1))")
                        || site.starts_with("(SELECT value FROM json_each(?2))"))
                {
                    parameterized_sites += 1;
                    continue;
                }
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
        parameterized_sites, 6,
        "six bound status-set membership sites: three global and three observer-scoped"
    );
    assert_eq!(
        live_sites, 8,
        "expected exactly eight live-status list literal sites in the SQLite backend; \
         update this count (and the derivation check) when adding one"
    );
    assert_eq!(
        foreign_sites, 1,
        "expected exactly 1 `ck_runtime_effect_replay_status` vocabulary literal, \
         which the lash-sim congruence registry owns"
    );
    assert_eq!(
        vocabulary_sites, 1,
        "expected exactly one `ck_processes_status` vocabulary literal in the SQLite DDL"
    );
}
