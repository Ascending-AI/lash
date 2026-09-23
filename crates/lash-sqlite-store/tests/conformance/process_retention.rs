use super::*;
use lash_core_execution::ProcessRetention as _;
use lash_sansio::ProcessId;

/// Drive one process into `waiting` and assert the retention contract: live rows
/// are listed as non-terminal and are never prune candidates.
async fn assert_waiting_process_is_live_not_prunable(
    registry: &dyn ProcessRegistry,
    process_id: &ProcessId,
) {
    registry
        .register_process(lash_core_execution::ProcessRegistration::new(
            process_id,
            lash_core_execution::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core_execution::RecoveryContract::Rerunnable,
            lash_core_execution::ProcessProvenance::host(),
            lash_core_execution::ProcessLifecyclePolicy::new(
                lash_core_execution::ParentScope::Host,
                lash_core_execution::OnParentEnd::Abandon,
            ),
        ))
        .await
        .expect("register waiting retention process");
    let authority = lash_core_execution::ProcessExecutionWriteAuthority::invocation(
        process_id,
        "waiting-retention-run",
    )
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
            lash_core_execution::WaitState {
                since_ms: 1,
                kind: lash_core_execution::WaitKind::Signal {
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
        .prune_terminal_processes(
            u64::MAX,
            None,
            lash_core_execution::ProjectionWatermark::NoProjector,
        )
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
/// `lash_core_execution::facade_support::registry_transitions::LIVE_PROCESS_STATUS_LABELS`
/// is the shared retention contract, and since FIG-2844 this backend's queries
/// build their predicates from `ProcessStatus` instead of respelling it. The law
/// test in core proves the constant partitions `ProcessStatus`; this is the
/// behavioural half, which is what fails if the generated predicates stop
/// agreeing with it and a live waiting process becomes prune-eligible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_waiting_processes_are_live_not_prunable() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry();
    let process_id = ProcessId::from(format!("waiting-retention:{}", uuid::Uuid::new_v4()));
    assert_waiting_process_is_live_not_prunable(registry.as_ref(), &process_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_prune_cleanup_evidence_survives_reopen_until_acknowledged() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry();
    let registered = registry
        .register_process(
            lash_core_execution::ProcessRegistration::new(
                "sqlite-prune-cleanup",
                lash_core_execution::ProcessInput::Engine {
                    kind: "test-engine".to_string(),
                    payload: serde_json::json!({"module_ref": "module-sqlite"}),
                },
                lash_core_execution::RecoveryContract::Rerunnable,
                lash_core_execution::ProcessProvenance::host(),
                lash_core_execution::ProcessLifecyclePolicy::new(
                    lash_core_execution::ParentScope::Host,
                    lash_core_execution::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(
                lash_core_execution::ProcessExecutionEnvRef::new("process-env:sqlite-cleanup"),
            )),
        )
        .await
        .expect("register cleanup process");
    registry
        .complete_process(
            &registered.id,
            lash_core_execution::ProcessAwaitOutput::from_tool_output(
                lash_core_execution::ToolCallOutput::success(serde_json::Value::Null),
            ),
            lash_core_execution::ProcessCompletionAuthority::workflow_key("sqlite-prune-cleanup"),
        )
        .await
        .expect("complete cleanup process");
    registry
        .prune_terminal_processes(
            u64::MAX,
            None,
            lash_core_execution::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune with atomic cleanup evidence");
    drop(registry);

    let reopened = backend.reopen().await.process_registry();
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
        lash_core_execution::ProcessArtifactCleanupAck::Acknowledged {
            process_ref: lash_core_execution::ProcessRef::from_record(&registered),
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
    let live_labels =
        lash_core_execution::facade_support::registry_transitions::LIVE_PROCESS_STATUS_LABELS;
    let retired_labels =
        lash_core_execution::facade_support::registry_transitions::RETIRED_PROCESS_STATUS_LABELS;
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
    const QUEUED_RUN_VOCABULARY_SITE: &str = "CONSTRAINT ck_queued_runs_status CHECK (";
    // FIG-3384 moved every process-family statement into the family's own
    // owner module, so that is where the query-site half of this inventory
    // now lives; the two modules it came from keep only call sites. The law
    // below is unchanged and still total over the statements.
    let sources = [
        (
            "process_registry.rs",
            include_str!("../../src/process_registry.rs"),
        ),
        (
            "process_registry/sql.rs",
            include_str!("../../src/process_registry/sql.rs"),
        ),
        (
            "process_registry_change.rs",
            include_str!("../../src/process_registry_change.rs"),
        ),
        ("schema.rs", include_str!("../../src/schema.rs")),
    ];
    // The third site kind: the pending-cancel index excludes the terminal
    // statuses rather than naming the live ones, because `caller_departed` is
    // neither live nor terminal and a departed caller's row still owes its
    // cancel. Its literal is generated, so the expectation here is the
    // generator's own output rather than a second spelling of it.
    let nonterminal =
        lash_core_execution::store_backend_support::nonterminal_process_status_predicate_sql(
            "status",
        );
    let terminal = nonterminal
        .strip_prefix("status NOT IN ")
        .expect("the nonterminal predicate is spelled as a NOT IN list")
        .to_string();
    let mut nonterminal_sites = 0usize;
    let mut live_sites = 0usize;
    let mut parameterized_sites = 0usize;
    let mut vocabulary_sites = 0usize;
    let mut foreign_sites = 0usize;
    let mut queued_run_sites = 0usize;
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
                if delimiter == "status NOT IN " && site.starts_with(terminal.as_str()) {
                    nonterminal_sites += 1;
                    continue;
                }
                let prefix = &source[..offset];
                if delimiter == "status IN " && prefix.ends_with(FOREIGN_VOCABULARY_SITE) {
                    foreign_sites += 1;
                    continue;
                }
                if delimiter == "status IN " && prefix.ends_with(QUEUED_RUN_VOCABULARY_SITE) {
                    assert!(site.starts_with("('pending', 'settled')"));
                    queued_run_sites += 1;
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
    // Fifteen bound status-set membership sites. Before FIG-3384 there were
    // six, because the two global listings and the two observer-scoped ones
    // were templates with an `{extra}` hole that the store filled per call.
    // The hole is gone: each combination of the optional, index-served
    // conjuncts is now its own named statement, so the same four listings are
    // spelled as four plain variants (one site each), four recently-retired
    // variants (two sites each, one per union arm), one observer listing and
    // one observer recently-retired listing (two sites). Every one of them is
    // still a bound parameter rather than a literal, which is what this count
    // exists to hold.
    assert_eq!(
        parameterized_sites,
        4 + 8 + 1 + 2,
        "bound status-set membership sites: four plain listings, four \
         recently-retired listings with a live and a retired arm each, and the \
         two observer-scoped listings"
    );
    // FIG-2844 generated every query-site predicate from `ProcessStatus`, so
    // the registry sources hold none: the only literals left are the three
    // partial-index predicates in the DDL - the live worklist, the retention
    // complement and the parent-end pending-cancel scan - whose vocabulary
    // FIG-2811 owns. `store_statements_never_retype_a_lifecycle_literal`
    // (lash-sim) is the gate that keeps a query-site literal from coming back;
    // this count is the inventory that keeps a new DDL literal from arriving
    // unnoticed.
    assert_eq!(
        live_sites, 3,
        "expected exactly three live-status list literal sites in the SQLite backend, \
         all partial indexes in schema.rs; a query-site literal belongs in a \
         generated fragment, not here"
    );
    assert_eq!(
        foreign_sites, 1,
        "expected exactly 1 `ck_runtime_effect_replay_status` vocabulary literal, \
         which the lash-sim congruence registry owns"
    );
    assert_eq!(
        queued_run_sites, 1,
        "expected one queued-run status vocabulary literal"
    );
    assert_eq!(
        vocabulary_sites, 1,
        "expected exactly one `ck_processes_status` vocabulary literal in the SQLite DDL"
    );
    assert_eq!(
        nonterminal_sites, 1,
        "expected exactly one nonterminal-status literal in the SQLite DDL: the \
         pending-cancel partial index, whose predicate must stay byte-identical to \
         the generated fragment the query uses"
    );
}
