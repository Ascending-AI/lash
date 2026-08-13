//! Cross-backend conformance for the durable process registry.

use super::process_change_feed::process_change_feed_never_misses_concurrent_terminal_writers;
use super::process_filters::list_processes_filters_by_enriched_fields;
use super::process_references::{
    ProcessCountConservation, assert_process_count_conservation,
    live_reference_summary_tracks_non_terminal_reference_counts,
};
use super::*;
use crate::{ProcessRecord, TestProcessRegistryWriteExt};

// The shared registry fixture performs 45 successful registrations and one
// prune; the cold refold fixture below adds the 46th registration.
const REOPEN_BASELINE_SPAWNS: usize = 46;
const REOPEN_BASELINE_PRUNED: usize = 1;

/// Run the process-registry contract against a fresh backend.
pub async fn process_registry<F>(make: F)
where
    F: Fn() -> Arc<dyn ProcessRegistry>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "process_registry");
    drop((first, second));
    process_registry_conformance(make()).await;
}

/// Run the process-registry contract and verify durable state through a reopen.
pub async fn process_registry_reopenable<F>(make: F)
where
    F: Fn() -> ReopenableProcessRegistry,
{
    let handles = make();
    assert_fresh_instances(
        &handles.open,
        &handles.reopen,
        "process_registry_reopenable",
    );
    process_registry_conformance(Arc::clone(&handles.open)).await;
    reopen_conformance(handles).await;
}

/// Prove that leased terminal replay repairs a stale record projection from
/// the persisted tail event on the backend under test.
pub async fn leased_completion_replay_repairs_projection<C, Fut>(
    registry: Arc<dyn ProcessRegistry>,
    corrupt_projection: C,
) where
    C: FnOnce(ProcessRecord) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let process_id = "leased-completion-replay-repair";
    let base = registry
        .register_process(ProcessRegistration::new(
            process_id,
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryDisposition::Rerunnable,
            ProcessProvenance::host(),
        ))
        .await
        .expect("register leased replay repair process");
    let lease = registry
        .claim_process_lease(
            process_id,
            &crate::LeaseOwnerIdentity::opaque("repair-owner", "repair-incarnation"),
            60_000,
        )
        .await
        .expect("claim leased replay repair process")
        .acquired()
        .expect("leased replay repair lease acquired");
    let output = ProcessAwaitOutput::Success {
        value: serde_json::json!({"repaired": true}),
        control: None,
    };
    let committed = registry
        .complete_process_with_lease(&lease, output.clone())
        .await
        .expect("commit leased terminal event");
    assert!(matches!(
        committed,
        crate::ProcessCompletionOutcome::Committed(ref stored) if stored.is_terminal()
    ));

    corrupt_projection(base).await;
    assert!(
        !registry
            .get_process(process_id)
            .await
            .expect("read deliberately stale projection")
            .expect("stale process exists")
            .is_terminal(),
        "fixture must expose a stale non-terminal projection before replay"
    );

    let replayed = registry
        .complete_process_with_lease(&lease, output)
        .await
        .expect("replay leased terminal event");
    assert!(matches!(
        replayed,
        crate::ProcessCompletionOutcome::AlreadyApplied { ref stored }
            if stored.is_terminal()
    ));
    assert!(
        registry
            .get_process(process_id)
            .await
            .expect("read repaired leased replay projection")
            .expect("repaired process exists")
            .is_terminal(),
        "leased completion replay must persist the repaired terminal projection"
    );
}

/// Prove that the retention filter scopes a prune (ADR 0023): a host pruning
/// one originator's terminal work reclaims exactly those rows and leaves every
/// other originator's row, and its own live row, in place.
pub async fn process_prune_scoped_by_originator(registry: Arc<dyn ProcessRegistry>) {
    async fn register_for(
        registry: &Arc<dyn ProcessRegistry>,
        process_id: &str,
        scope: &SessionScope,
    ) {
        registry
            .register_process(
                registration(process_id)
                    .with_process_provenance(ProcessProvenance::session(scope.clone())),
            )
            .await
            .expect("register scoped prune process");
    }

    async fn complete(registry: &Arc<dyn ProcessRegistry>, process_id: &str) {
        registry
            .complete_process(
                process_id,
                ProcessAwaitOutput::Success {
                    value: serde_json::Value::Null,
                    control: None,
                },
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete scoped prune process");
    }

    async fn retained(registry: &Arc<dyn ProcessRegistry>, process_id: &str) -> bool {
        match registry.get_process(process_id).await {
            Ok(record) => record.is_some(),
            Err(crate::PluginError::ProcessNoLongerRetained { .. }) => false,
            Err(err) => panic!("read scoped prune process: {err:?}"),
        }
    }

    let deleted = SessionScope::for_agent_frame("scoped-prune-deleted", "scoped-prune-frame");
    let surviving = SessionScope::for_agent_frame("scoped-prune-surviving", "scoped-prune-frame");
    register_for(&registry, "scoped-prune-deleted-terminal", &deleted).await;
    complete(&registry, "scoped-prune-deleted-terminal").await;
    register_for(&registry, "scoped-prune-deleted-live", &deleted).await;
    register_for(&registry, "scoped-prune-surviving-terminal", &surviving).await;
    complete(&registry, "scoped-prune-surviving-terminal").await;

    let report = registry
        .prune_terminal_processes(
            u64::MAX,
            Some(ProcessListFilter {
                status: ProcessStatusFilter::Any,
                originator_id: Some(deleted.session_id.clone()),
                ..ProcessListFilter::default()
            }),
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune one originator's terminal processes");
    assert_eq!(
        report.pruned_processes, 1,
        "only the filtered originator's terminal row is reclaimed"
    );
    assert_eq!(report.pruned_events, 1);
    assert!(
        !retained(&registry, "scoped-prune-deleted-terminal").await,
        "the filtered originator's terminal row must be gone"
    );
    assert!(
        retained(&registry, "scoped-prune-deleted-live").await,
        "a live row is never a prune candidate, whatever the filter matches"
    );
    assert!(
        retained(&registry, "scoped-prune-surviving-terminal").await,
        "another originator's terminal row must survive a scoped prune"
    );
    assert_eq!(
        registry
            .filter_tombstoned_process_ids(&[
                "scoped-prune-deleted-terminal".to_string(),
                "scoped-prune-surviving-terminal".to_string(),
            ])
            .await
            .expect("classify scoped prune history"),
        vec!["scoped-prune-deleted-terminal".to_string()],
        "only the reclaimed row becomes a tombstone"
    );

    // A terminal status narrows the same lever further, and the unfiltered
    // sweep still reaches the row a scoped prune deliberately skipped.
    let report = registry
        .prune_terminal_processes(
            u64::MAX,
            Some(ProcessListFilter {
                status: ProcessStatusFilter::Failed,
                originator_id: Some(surviving.session_id.clone()),
                ..ProcessListFilter::default()
            }),
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune with a terminal status filter");
    assert_eq!(
        report.pruned_processes, 0,
        "a completed row does not match a `failed` retention filter"
    );
    let report = registry
        .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
        .await
        .expect("prune every terminal process");
    assert_eq!(report.pruned_processes, 1);
    assert!(!retained(&registry, "scoped-prune-surviving-terminal").await);
    assert!(retained(&registry, "scoped-prune-deleted-live").await);
}

/// Prove that one SQL prune batch allocates complete, process-id-ordered
/// tombstone sequences and reports every removed process event.
pub async fn process_prune_batch_tombstones(registry: Arc<dyn ProcessRegistry>) {
    let cases = [
        (
            "batch-prune-a",
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            "completed",
        ),
        (
            "batch-prune-b",
            ProcessAwaitOutput::Failure {
                class: crate::ToolFailureClass::External,
                code: "batch_failure".to_string(),
                message: "batch failure".to_string(),
                raw: None,
                control: None,
            },
            "failed",
        ),
        (
            "batch-prune-c",
            ProcessAwaitOutput::Cancelled {
                message: "batch cancellation".to_string(),
                raw: None,
                control: None,
            },
            "cancelled",
        ),
    ];
    for (process_id, output, _) in &cases {
        registry
            .register_process(registration(process_id))
            .await
            .expect("register batch-prune process");
        registry
            .complete_process(
                process_id,
                output.clone(),
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete batch-prune process");
    }
    registry
        .register_process(registration("batch-prune-live-sentinel"))
        .await
        .expect("register live sequence sentinel");

    let (_, projection_cursor) = registry
        .processes_changed_since(crate::ProcessChangeCursor::initial(), 100)
        .await
        .expect("project batch terminals before pruning");
    assert_eq!(
        projection_cursor.store_sequence(),
        7,
        "three registrations, three completions, and the live sentinel must advance the clock to 7"
    );

    let report = registry
        .prune_terminal_processes(
            u64::MAX,
            None,
            crate::ProjectionWatermark::UpTo(projection_cursor),
        )
        .await
        .expect("prune three terminal processes in one batch");
    assert_eq!(report.pruned_processes, 3);
    assert_eq!(report.pruned_events, 3);
    assert_eq!(report.pruned_trigger_deliveries, 0);

    let mut cursor = projection_cursor;
    let mut sequences = Vec::new();
    for ((expected_id, _, expected_label), expected_sequence) in cases.iter().zip([8_u64, 9, 10]) {
        let (changes, next_cursor) = registry
            .processes_changed_since(cursor, 1)
            .await
            .expect("page one batch tombstone");
        let [crate::ProcessChange::Deleted { tombstone }] = changes.as_slice() else {
            panic!("expected exactly one tombstone page, got {changes:?}");
        };
        assert_eq!(tombstone.process_id, *expected_id);
        assert_eq!(tombstone.terminal_label, *expected_label);
        assert_eq!(tombstone.pruned_change_seq, expected_sequence);
        sequences.push(tombstone.pruned_change_seq);
        cursor = next_cursor;
    }
    assert_eq!(sequences, [8, 9, 10]);
    let (remaining, _) = registry
        .processes_changed_since(cursor, 1)
        .await
        .expect("read after complete batch tombstone feed");
    assert!(remaining.is_empty(), "batch deletion feed must be complete");
}

pub(super) fn registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryDisposition::ExternallyOwned,
        ProcessProvenance::host(),
    )
    .with_identity(
        ProcessIdentity::new("conformance")
            .with_label(Some(id))
            .with_definition(Some(serde_json::json!({"suite": "process_registry"}))),
    )
}

pub(super) fn wake_event_type(name: &str) -> ProcessEventType {
    ProcessEventType {
        name: name.to_string(),
        payload_schema: LashSchema::any(),
        semantics: ProcessEventSemanticsSpec {
            wake: Some(ProcessWakeSpec {
                when: Some(ProcessValueSelector::Present("/wake_input".to_string())),
                input: ProcessValueSelector::Pointer("/wake_input".to_string()),
            }),
            ..ProcessEventSemanticsSpec::default()
        },
    }
}

pub(super) fn plain_event_type(name: &str) -> ProcessEventType {
    ProcessEventType {
        name: name.to_string(),
        payload_schema: LashSchema::any(),
        semantics: ProcessEventSemanticsSpec::default(),
    }
}

async fn process_registry_conformance(registry: Arc<dyn ProcessRegistry>) {
    live_reference_summary_tracks_non_terminal_reference_counts(Arc::clone(&registry)).await;
    registration_and_observers_are_atomic(Arc::clone(&registry)).await;
    observer_events_are_auditable_and_transfer_is_atomic(Arc::clone(&registry)).await;
    generic_append_rejects_reserved_edge_audit_events(Arc::clone(&registry)).await;
    canonical_process_event_payload_replay(Arc::clone(&registry)).await;
    long_cancellation_reason_replay_is_backend_safe(Arc::clone(&registry)).await;
    wake_subscription_is_indexed_and_retargetable(Arc::clone(&registry)).await;
    lifecycle_status_and_outcome_fold(Arc::clone(&registry)).await;
    producer_terminal_status_must_match_materialized_outcome(Arc::clone(&registry)).await;
    list_filters_match_extracted_and_json_fields(Arc::clone(&registry)).await;
    process_registry_pagination(Arc::clone(&registry)).await;
    waiting_processes_remain_in_the_recovery_worklist(Arc::clone(&registry)).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    list_processes_filters_by_enriched_fields(Arc::clone(&registry)).await;
    process_change_feed_never_misses_concurrent_terminal_writers(Arc::clone(&registry)).await;
    process_lease_fencing_contract(Arc::clone(&registry)).await;
    session_delete_preserves_process_bytes(Arc::clone(&registry)).await;
    refolded_process_record_matches_stored_projection(
        Arc::clone(&registry),
        Arc::clone(&registry),
        "process-refold-hot",
    )
    .await;
    process_attempt_budget_is_typed(Arc::clone(&registry)).await;
    tombstones_make_pruned_processes_distinguishable(Arc::clone(&registry)).await;
    terminal_completion_atomically_retains_parent_end_plan(registry).await;
}

async fn terminal_completion_atomically_retains_parent_end_plan(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = "process-parent-end-plan";
    registry
        .register_process(ProcessRegistration::new(
            process_id,
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryDisposition::Rerunnable,
            ProcessProvenance::host(),
        ))
        .await
        .expect("register parent-end-plan process");
    let lease = registry
        .claim_process_lease(
            process_id,
            &crate::LeaseOwnerIdentity::opaque("parent-end-owner", "parent-end-owner:i"),
            60_000,
        )
        .await
        .expect("claim parent-end-plan process")
        .acquired()
        .expect("parent-end-plan lease acquired");
    let action = crate::ToolIntentParentEndAction {
        identity: crate::derive_tool_intent_identity(
            "parent-end-session",
            process_id,
            Some("parent-end-call"),
            0,
        )
        .expect("parent-end identity"),
        parent_end: crate::ToolIntentParentEnd {
            process_id: "parent-end-child".to_string(),
            policy: crate::ProcessParentEndPolicy::Cancel,
        },
    };
    let completion = registry
        .complete_process_with_lease_and_parent_end(
            &lease,
            ProcessAwaitOutput::Success {
                value: serde_json::json!({"parent": "done"}),
                control: None,
            },
            vec![action.clone()],
        )
        .await
        .expect("terminal write and parent-end plan commit atomically");
    assert!(matches!(
        completion,
        crate::ProcessCompletionOutcome::Committed(_)
    ));
    assert_eq!(
        registry
            .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
            .await
            .expect("list pending parent-end plan"),
        vec![crate::ProcessParentEndPlan {
            process_id: process_id.to_string(),
            actions: vec![action],
        }]
    );
    registry
        .complete_parent_end_plan(process_id)
        .await
        .expect("complete parent-end plan");
    registry
        .complete_parent_end_plan(process_id)
        .await
        .expect("parent-end plan completion is idempotent");
    assert!(
        registry
            .list_pending_parent_end_plans(std::num::NonZeroUsize::MIN)
            .await
            .expect("parent-end plan cleared")
            .is_empty()
    );
}

/// Prove bounded keyset pagination and its page-boundary completion contract.
#[doc(hidden)]
pub async fn process_registry_pagination(registry: Arc<dyn ProcessRegistry>) {
    let process_ids = (0..7)
        .map(|index| format!("000-paged-worklist-{index:02}"))
        .collect::<Vec<_>>();
    for process_id in &process_ids {
        registry
            .register_process(registration(process_id))
            .await
            .expect("register paged worklist process");
    }

    let limit = std::num::NonZeroUsize::new(2).expect("non-zero test page size");
    let first = registry
        .list_non_terminal_page(limit, None)
        .await
        .expect("read first recovery worklist page");
    assert_eq!(
        first.records.len(),
        2,
        "the first page must honor its bound"
    );
    let boundary_id = first
        .records
        .last()
        .expect("the first page is non-empty")
        .id
        .clone();
    registry
        .complete_process(
            &boundary_id,
            ProcessAwaitOutput::Success {
                value: serde_json::json!({"completed_between_pages": true}),
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete page-boundary process");

    let mut page_count = 1;
    let mut returned_ids = first
        .records
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    let mut continuation = first.continuation;
    while let Some(cursor) = continuation {
        let page = registry
            .list_non_terminal_page(limit, Some(cursor))
            .await
            .expect("read recovery worklist continuation");
        page_count += 1;
        returned_ids.extend(page.records.into_iter().map(|record| record.id));
        continuation = page.continuation;
    }

    assert!(
        page_count >= 3,
        "the fixture must span at least three pages"
    );
    for process_id in &process_ids {
        assert_eq!(
            returned_ids.iter().filter(|id| *id == process_id).count(),
            1,
            "each scan-start row must be returned exactly once"
        );
    }
    assert_eq!(
        returned_ids.iter().filter(|id| *id == &boundary_id).count(),
        1,
        "a process completed after its page must not be dispatched again"
    );
    worklist_excludes_rows_terminalized_before_a_later_page(Arc::clone(&registry)).await;
    worklist_next_scan_recovers_insert_behind_cursor(Arc::clone(&registry)).await;
    worklist_captured_boundary_defers_beyond_bound_insert(registry).await;
}

async fn collect_worklist_ids(registry: &dyn ProcessRegistry) -> Vec<String> {
    let limit = std::num::NonZeroUsize::new(128).expect("non-zero test page size");
    let mut continuation = None;
    let mut ids = Vec::new();
    loop {
        let page = registry
            .list_non_terminal_page(limit, continuation)
            .await
            .expect("scan complete recovery worklist");
        ids.extend(page.records.into_iter().map(|record| record.id));
        let Some(next) = page.continuation else {
            return ids;
        };
        continuation = Some(next);
    }
}

/// A row that terminalizes before its not-yet-read page is no longer recovery work.
#[doc(hidden)]
pub async fn worklist_excludes_rows_terminalized_before_a_later_page(
    registry: Arc<dyn ProcessRegistry>,
) {
    let first_id = "!!!worklist-terminal-later-a";
    let terminalized_id = "!!!worklist-terminal-later-b";
    let last_id = "!!!worklist-terminal-later-c";
    for process_id in [first_id, terminalized_id, last_id] {
        registry
            .register_process(registration(process_id))
            .await
            .expect("register later-page terminalization fixture");
    }
    let limit = std::num::NonZeroUsize::new(1).expect("non-zero test page size");
    let first = registry
        .list_non_terminal_page(limit, None)
        .await
        .expect("read first later-page terminalization page");
    assert_eq!(first.records[0].id, first_id);
    registry
        .complete_process(
            terminalized_id,
            ProcessAwaitOutput::Success {
                value: serde_json::json!({"terminalized_before_page": true}),
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("terminalize row before its page");

    let mut ids = Vec::new();
    let mut continuation = first.continuation;
    while let Some(cursor) = continuation {
        let page = registry
            .list_non_terminal_page(limit, Some(cursor))
            .await
            .expect("read later-page terminalization continuation");
        ids.extend(page.records.into_iter().map(|record| record.id));
        continuation = page.continuation;
    }
    assert!(!ids.iter().any(|id| id == terminalized_id));
    assert!(ids.iter().any(|id| id == last_id));
}

/// An in-range insert behind the keyset cursor is guaranteed on the next scan.
#[doc(hidden)]
pub async fn worklist_next_scan_recovers_insert_behind_cursor(registry: Arc<dyn ProcessRegistry>) {
    let first_id = "!!worklist-behind-cursor-a";
    let bound_id = "!!worklist-behind-cursor-z";
    let inserted_id = "!!!!worklist-behind-cursor-insert";
    for process_id in [first_id, bound_id] {
        registry
            .register_process(registration(process_id))
            .await
            .expect("register behind-cursor fixture");
    }
    let first = registry
        .list_non_terminal_page(
            std::num::NonZeroUsize::new(1).expect("non-zero test page size"),
            None,
        )
        .await
        .expect("capture behind-cursor scan");
    let cursor_id = &first.records[0].id;
    assert!(
        inserted_id < cursor_id.as_str(),
        "the concurrent insert fixture must sort behind the captured cursor"
    );
    registry
        .register_process(registration(inserted_id))
        .await
        .expect("insert in-range row behind cursor");

    let next_scan = collect_worklist_ids(registry.as_ref()).await;
    assert!(
        next_scan.iter().any(|id| id == inserted_id),
        "the next scan must recover an insert behind the prior cursor"
    );
}

/// An insert beyond the captured upper bound waits for the next scan.
#[doc(hidden)]
pub async fn worklist_captured_boundary_defers_beyond_bound_insert(
    registry: Arc<dyn ProcessRegistry>,
) {
    let limit = std::num::NonZeroUsize::new(1).expect("non-zero test page size");
    let first = registry
        .list_non_terminal_page(limit, None)
        .await
        .expect("capture bounded worklist scan");
    let inserted_id = "~~~~~worklist-after-captured-bound";
    registry
        .register_process(registration(inserted_id))
        .await
        .expect("insert beyond captured bound");

    let mut current_scan_ids = first
        .records
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    let mut continuation = first.continuation;
    while let Some(cursor) = continuation {
        let page = registry
            .list_non_terminal_page(limit, Some(cursor))
            .await
            .expect("read captured-bound continuation");
        current_scan_ids.extend(page.records.into_iter().map(|record| record.id));
        continuation = page.continuation;
    }
    assert!(
        !current_scan_ids.iter().any(|id| id == inserted_id),
        "an insert beyond the captured bound must not leak into the current scan"
    );
    assert!(
        collect_worklist_ids(registry.as_ref())
            .await
            .iter()
            .any(|id| id == inserted_id),
        "the next scan must include the beyond-bound insert"
    );
}

async fn canonical_process_event_payload_replay(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "canonical-process-event-payload-replay";
    registry
        .register_process(
            registration(process_id).with_extra_event_types([plain_event_type("signal.zero")]),
        )
        .await
        .expect("register canonical-payload process");
    let replay_key = format!("process:{process_id}:signal.zero:1");
    let first = registry
        .append_event(
            process_id,
            ProcessEventAppendRequest::new("signal.zero", serde_json::json!({"value": -0.0}))
                .with_replay_key(&replay_key),
        )
        .await
        .expect("append negative-zero payload");
    let replay = registry
        .append_event(
            process_id,
            ProcessEventAppendRequest::new("signal.zero", serde_json::json!({"value": 0.0}))
                .with_replay_key(replay_key),
        )
        .await
        .expect("canonical positive-zero retry must be idempotent");
    assert_eq!(
        replay.event.sequence, first.event.sequence,
        "canonically equal zero payloads must share the replayed event"
    );
}

async fn long_cancellation_reason_replay_is_backend_safe(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "long-cancellation-reason-replay";
    registry
        .register_process(registration(process_id))
        .await
        .expect("register long-cancellation process");
    let reason = (0..800)
        .map(|index| format!("{index:08x}"))
        .collect::<String>();
    let request = ProcessEventAppendRequest::cancel_requested(process_id, Some(reason));
    let first = registry
        .append_event(process_id, request.clone())
        .await
        .expect("append cancellation with long reason");
    let replay = registry
        .append_event(process_id, request)
        .await
        .expect("replay cancellation with long reason");
    assert_eq!(
        replay.event.sequence, first.event.sequence,
        "long cancellation reason retries must remain idempotent on every backend"
    );
}

async fn refolded_process_record_matches_stored_projection(
    writer: Arc<dyn ProcessRegistry>,
    reader: Arc<dyn ProcessRegistry>,
    process_id: &str,
) {
    let base = writer
        .register_process(
            ProcessRegistration::new(
                process_id,
                ProcessInput::Engine {
                    kind: "refold-conformance".to_string(),
                    payload: serde_json::json!({"case": process_id}),
                },
                RecoveryDisposition::Rerunnable,
                ProcessProvenance::host(),
            )
            .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
                "process-env:{process_id}"
            ))))
            .with_extra_event_types([plain_event_type("signal.ready")]),
        )
        .await
        .expect("register refold process");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "registration").await;
    writer
        .record_first_started(
            process_id,
            crate::ProcessStarted {
                owner: crate::LeaseOwnerIdentity::opaque(
                    "refold-worker",
                    format!("refold-worker:{process_id}"),
                ),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: base.created_at_ms,
            },
        )
        .await
        .expect("record refold first start");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "first start").await;
    let wait = WaitState {
        since_ms: base.created_at_ms,
        kind: WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: format!("process:{process_id}:signal.ready:1"),
            ordinal: 1,
        },
    };
    writer
        .set_process_wait(process_id, wait)
        .await
        .expect("enter refold wait");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "wait entered").await;
    writer
        .clear_process_wait(process_id)
        .await
        .expect("clear refold wait");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "wait cleared").await;
    writer
        .set_external_ref(
            process_id,
            crate::ProcessExternalRef {
                backend: "refold-conformance".to_string(),
                id: format!("external:{process_id}"),
                metadata: Some(serde_json::json!({"cold": !Arc::ptr_eq(&writer, &reader)})),
            },
        )
        .await
        .expect("set refold external reference");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "external ref set").await;
    let signal =
        ProcessEventAppendRequest::new("signal.ready", serde_json::json!({"signal": "ready"}))
            .with_replay_key(format!("process:{process_id}:signal.ready:1"));
    let first_signal = writer
        .append_event(process_id, signal.clone())
        .await
        .expect("append refold signal");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "signal appended").await;
    let replayed_signal = writer
        .append_event(process_id, signal)
        .await
        .expect("replay refold signal");
    assert_eq!(
        replayed_signal.event.sequence, first_signal.event.sequence,
        "a replayed duplicate must not add another event to the fold"
    );
    assert_refold_matches_stored_projection(&reader, &base, process_id, "signal replayed").await;
    writer
        .add_observer(
            "refold-observer",
            process_id,
            crate::ProcessObserverBy::host("refold-add"),
        )
        .await
        .expect("add refold observer");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "observer added").await;
    writer
        .remove_observer(
            "refold-observer",
            process_id,
            crate::ProcessObserverBy::host("refold-remove"),
        )
        .await
        .expect("remove refold observer");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "observer removed").await;
    writer
        .complete_process(
            process_id,
            ProcessAwaitOutput::Success {
                value: serde_json::json!({"refolded": true}),
                control: None,
            },
            ProcessCompletionAuthority::workflow_key(format!("refold:{process_id}")),
        )
        .await
        .expect("complete refold process");

    assert_refold_matches_stored_projection(&reader, &base, process_id, "terminal completion")
        .await;
}

async fn assert_refold_matches_stored_projection(
    reader: &Arc<dyn ProcessRegistry>,
    base: &ProcessRecord,
    process_id: &str,
    transition: &str,
) {
    let events = reader
        .events_after(process_id, 0)
        .await
        .expect("load refold event log");
    let stored = reader
        .get_process(process_id)
        .await
        .expect("load stored refold projection")
        .expect("refold process remains stored");
    let refolded = crate::fold_process_record(base.clone(), &events).expect("refold event log");
    assert_eq!(
        refolded, stored,
        "folding the event log after {transition} from the registration base must reproduce the stored record field-for-field"
    );
}

async fn process_attempt_budget_is_typed(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "process-attempt-budget";
    registry
        .register_process(
            ProcessRegistration::new(
                process_id,
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                RecoveryDisposition::Rerunnable,
                ProcessProvenance::host(),
            )
            .with_max_attempts(Some(1)),
        )
        .await
        .expect("register attempt-budget process");

    let first_owner = crate::LeaseOwnerIdentity::opaque("attempt-owner-1", "attempt-owner-1:i");
    let first_lease = registry
        .claim_process_lease(process_id, &first_owner, 60_000)
        .await
        .expect("claim first attempt lease")
        .acquired()
        .expect("first attempt lease acquired");
    let first_started = crate::ProcessStarted {
        owner: first_owner,
        fencing_token: first_lease.fencing_token,
        attempt: 1,
        started_at_ms: first_lease.claimed_at_epoch_ms,
    };
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                process_id,
                first_started,
                &crate::ProcessExecutionWriteAuthority::lease(first_lease.clone()),
            )
            .await
            .expect("record first attempt"),
        crate::ProcessStartOutcome::Started(_)
    ));
    registry
        .complete_process_lease(&crate::ProcessLeaseCompletion::from_lease(&first_lease))
        .await
        .expect("release first attempt lease");

    let next_owner = crate::LeaseOwnerIdentity::opaque("attempt-owner-2", "attempt-owner-2:i");
    let next_lease = registry
        .claim_process_lease(process_id, &next_owner, 60_000)
        .await
        .expect("claim next attempt lease")
        .acquired()
        .expect("next attempt lease acquired");
    let outcome = registry
        .record_first_started_with_authority(
            process_id,
            crate::ProcessStarted {
                owner: next_owner,
                fencing_token: next_lease.fencing_token,
                attempt: 2,
                started_at_ms: next_lease.claimed_at_epoch_ms,
            },
            &crate::ProcessExecutionWriteAuthority::lease(next_lease.clone()),
        )
        .await
        .expect("attempt exhaustion is a typed start outcome");
    match outcome {
        crate::ProcessStartOutcome::AttemptsExhausted {
            current,
            attempts,
            max_attempts,
        } => {
            assert_eq!(attempts, 1);
            assert_eq!(max_attempts, 1);
            assert_eq!(
                current.first_started.as_deref().map(|start| start.attempt),
                Some(1)
            );
            assert!(!current.is_terminal());
        }
        other => panic!("expected AttemptsExhausted, got {other:?}"),
    }
    registry
        .complete_process_lease(&crate::ProcessLeaseCompletion::from_lease(&next_lease))
        .await
        .expect("release exhausted-attempt lease");
}

async fn producer_terminal_status_must_match_materialized_outcome(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = "producer-terminal-outcome-mismatch";
    let record = registry
        .register_process(
            registration(process_id).with_extra_event_types([ProcessEventType {
                name: "producer.failed".to_string(),
                payload_schema: LashSchema::any(),
                semantics: ProcessEventSemanticsSpec {
                    terminal: Some(crate::ProcessTerminalSpec {
                        status: ProcessStatus::Failed,
                        await_output: Some(ProcessValueSelector::Pointer("/out".to_string())),
                    }),
                    ..ProcessEventSemanticsSpec::default()
                },
            }]),
        )
        .await
        .expect("register producer terminal event");
    let before = serde_json::to_vec(&record).expect("serialize producer before rejected append");
    let error = registry
        .append_event(
            process_id,
            ProcessEventAppendRequest::new(
                "producer.failed",
                serde_json::json!({
                    "out": {
                        "type": "success",
                        "value": 1
                    }
                }),
            )
            .with_replay_key(format!("{process_id}:producer.failed")),
        )
        .await
        .expect_err("declared terminal status must match the selected structured outcome");
    assert!(matches!(
        error,
        crate::PluginError::ProcessTerminalOutcomeMismatch {
            declared_status: ProcessStatus::Failed,
            outcome_status: Some(ProcessStatus::Completed),
        }
    ));
    let after = registry
        .get_process(process_id)
        .await
        .expect("read producer after rejected append")
        .expect("producer remains");
    assert_eq!(
        serde_json::to_vec(&after).expect("serialize producer after rejected append"),
        before,
        "rejected core terminal semantics must not mutate the producer record"
    );
    assert!(
        registry
            .events_after(process_id, 0)
            .await
            .expect("read events after rejected append")
            .is_empty(),
        "rejected core terminal semantics must not append an event"
    );
}

async fn generic_append_rejects_reserved_edge_audit_events(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "reserved-edge-audit";
    registry
        .register_process(registration(process_id))
        .await
        .expect("register reserved-audit process");
    let by = crate::ProcessObserverBy::host("generic-append");
    let requests = [
        ProcessEventAppendRequest::observer_added(process_id, "observer", &by),
        ProcessEventAppendRequest::observer_removed(process_id, "observer", &by),
        ProcessEventAppendRequest::subscription_retargeted(process_id, Some("target")),
    ];
    for request in requests {
        let event_type = request.event_type.clone();
        assert!(
            matches!(
                registry.append_event(process_id, request).await,
                Err(crate::PluginError::ReservedProcessEvent {
                    event_type: rejected
                }) if rejected == event_type
            ),
            "generic append must reject reserved edge audit event `{event_type}`"
        );
    }
    assert!(
        !registry
            .is_observer("observer", process_id)
            .await
            .expect("observer query remains available")
    );
}

async fn waiting_processes_remain_in_the_recovery_worklist(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "waiting-recovery-worklist";
    let definition = serde_json::json!({"suite": "waiting-recovery-worklist"});
    let env_ref = ProcessExecutionEnvRef::new("process-env:waiting-recovery-worklist");
    let count_before = registry
        .count_non_terminal_processes()
        .await
        .expect("count existing non-terminal processes");
    let record = registry
        .register_process(
            ProcessRegistration::new(
                process_id,
                ProcessInput::Engine {
                    kind: "waiting-recovery-worklist".to_string(),
                    payload: serde_json::Value::Null,
                },
                RecoveryDisposition::Rerunnable,
                ProcessProvenance::host(),
            )
            .with_identity(
                ProcessIdentity::new("waiting-recovery-worklist")
                    .with_definition(Some(definition.clone())),
            )
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await
        .expect("register waiting process");
    registry
        .set_process_wait(
            process_id,
            WaitState {
                since_ms: record.created_at_ms,
                kind: WaitKind::Signal {
                    name: "resume".to_string(),
                    event_type: "signal.resume".to_string(),
                    key: format!("{process_id}:signal.resume:1"),
                    ordinal: 1,
                },
            },
        )
        .await
        .expect("park process");

    let non_terminal = registry
        .list_non_terminal_page(
            std::num::NonZeroUsize::new(128).expect("non-zero test page size"),
            None,
        )
        .await
        .expect("list recovery work")
        .records;
    assert!(
        non_terminal.iter().any(|record| record.id == process_id),
        "a waiting process must remain claimable by crash recovery"
    );
    assert_eq!(
        registry
            .count_non_terminal_processes()
            .await
            .expect("count waiting non-terminal process"),
        count_before + 1,
        "a waiting process must pin the deployment as non-drained"
    );
    let references = registry
        .live_reference_summary()
        .await
        .expect("summarize waiting live references");
    assert!(
        references.iter().any(|summary| {
            summary.definition.as_ref() == Some(&definition)
                && summary.env_ref.as_ref() == Some(&env_ref)
                && summary.process_count == 1
        }),
        "live-reference accounting must retain waiting process rows"
    );
}

fn process_lease_owner(owner_id: &str) -> crate::LeaseOwnerIdentity {
    crate::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

async fn claim_after_expiry(
    registry: &dyn ProcessRegistry,
    process_id: &str,
    owner: &crate::LeaseOwnerIdentity,
) -> crate::ProcessLease {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match registry
            .claim_process_lease(process_id, owner, 60_000)
            .await
            .expect("claim after expiry")
        {
            crate::ProcessLeaseClaimOutcome::Acquired(lease) => return lease,
            crate::ProcessLeaseClaimOutcome::Busy { .. }
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            crate::ProcessLeaseClaimOutcome::Busy { holder } => {
                panic!("lease remained busy after expiry: {holder:?}")
            }
        }
    }
}

async fn process_lease_fencing_contract(registry: Arc<dyn ProcessRegistry>) {
    const SHORT_TTL_MS: u64 = 20;

    // A lease is authority over a retained registry row, so a claim for a
    // process the store never registered must be refused, never materialized.
    // FIG-953: the in-memory registry used to invent a lease here, which the
    // cross-backend differential caught at seed 852 as raw-state divergence
    // from the SQL backends.
    match registry
        .claim_process_lease(
            "lease-never-registered",
            &process_lease_owner("owner-a"),
            60_000,
        )
        .await
    {
        Err(crate::PluginError::Session(_)) => {}
        other => {
            panic!("claiming a lease for an unregistered process must be refused, got {other:?}")
        }
    }
    assert!(
        registry
            .get_process_lease("lease-never-registered")
            .await
            .expect("read lease for unregistered process")
            .is_none(),
        "a refused claim must not persist a lease row"
    );

    registry
        .register_process(registration("lease-active"))
        .await
        .expect("register active lease process");
    let first = registry
        .claim_process_lease("lease-active", &process_lease_owner("owner-a"), 60_000)
        .await
        .expect("claim active lease")
        .acquired()
        .expect("active lease acquired");
    let conflict = registry
        .claim_process_lease("lease-active", &process_lease_owner("owner-b"), 60_000)
        .await
        .expect("competing claim");
    assert!(
        matches!(
            conflict,
            crate::ProcessLeaseClaimOutcome::Busy { ref holder }
                if holder.lease_token == first.lease_token
        ),
        "a live lease must fence a competing owner"
    );
    let reentered = registry
        .claim_process_lease("lease-active", &process_lease_owner("owner-a"), 120_000)
        .await
        .expect("re-enter lease")
        .acquired()
        .expect("same owner re-enters");
    assert_eq!(reentered.lease_token, first.lease_token);
    assert_eq!(reentered.fencing_token, first.fencing_token);

    registry
        .register_process(registration("lease-renew"))
        .await
        .expect("register renewal process");
    let short = registry
        .claim_process_lease("lease-renew", &process_lease_owner("owner-a"), 60_000)
        .await
        .expect("claim short lease")
        .acquired()
        .expect("short lease acquired");
    let renewed = registry
        .renew_process_lease(&short, 120_000)
        .await
        .expect("renew lease");
    assert!(
        renewed.expires_at_epoch_ms > short.expires_at_epoch_ms,
        "renewal must extend the persisted lease expiry"
    );
    let persisted_renewed = registry
        .get_process_lease("lease-renew")
        .await
        .expect("read renewed lease")
        .expect("renewed lease remains persisted");
    assert_eq!(
        persisted_renewed.expires_at_epoch_ms, renewed.expires_at_epoch_ms,
        "renewal must write the returned expiry to the persisted lease"
    );
    registry
        .renew_process_lease(&renewed, 120_000)
        .await
        .expect("extended lease remains renewable");

    registry
        .register_process(registration("lease-release"))
        .await
        .expect("register release process");
    let released = registry
        .claim_process_lease("lease-release", &process_lease_owner("owner-a"), 60_000)
        .await
        .expect("claim releasable lease")
        .acquired()
        .expect("releasable lease acquired");
    registry
        .complete_process_lease(&crate::ProcessLeaseCompletion::from_lease(&released))
        .await
        .expect("release lease");
    let reclaimed = registry
        .claim_process_lease("lease-release", &process_lease_owner("owner-b"), 60_000)
        .await
        .expect("claim released lease")
        .acquired()
        .expect("released lease acquired");
    assert!(reclaimed.fencing_token > released.fencing_token);

    registry
        .register_process(registration("lease-stale-release"))
        .await
        .expect("register stale-release process");
    let stale_release = registry
        .claim_process_lease("lease-stale-release", &process_lease_owner("owner-a"), 0)
        .await
        .expect("claim immediately expiring lease")
        .acquired()
        .expect("immediately expiring lease acquired");
    let live = registry
        .claim_process_lease(
            "lease-stale-release",
            &process_lease_owner("owner-b"),
            60_000,
        )
        .await
        .expect("claim successor lease")
        .acquired()
        .expect("successor lease acquired");
    registry
        .complete_process_lease(&crate::ProcessLeaseCompletion::from_lease(&stale_release))
        .await
        .expect("stale release is idempotently ignored");
    let still_busy = registry
        .claim_process_lease(
            "lease-stale-release",
            &process_lease_owner("owner-c"),
            60_000,
        )
        .await
        .expect("claim against live successor");
    assert!(matches!(
        still_busy,
        crate::ProcessLeaseClaimOutcome::Busy { .. }
    ));
    registry
        .renew_process_lease(&live, 60_000)
        .await
        .expect("successor remains renewable");

    registry
        .register_process(registration("lease-stale-completion"))
        .await
        .expect("register stale-completion process");
    let stale = registry
        .claim_process_lease(
            "lease-stale-completion",
            &process_lease_owner("owner-a"),
            SHORT_TTL_MS,
        )
        .await
        .expect("claim stale candidate")
        .acquired()
        .expect("stale candidate acquired");
    let current = claim_after_expiry(
        registry.as_ref(),
        "lease-stale-completion",
        &process_lease_owner("owner-b"),
    )
    .await;
    assert!(
        registry
            .complete_process_with_lease(
                &stale,
                ProcessAwaitOutput::Success {
                    value: serde_json::json!({"writer": "stale"}),
                    control: None,
                },
            )
            .await
            .is_err(),
        "a superseded lease must not complete the process"
    );
    let record = registry
        .get_process("lease-stale-completion")
        .await
        .expect("read stale-completion process")
        .expect("stale-completion process exists");
    assert!(!record.is_terminal());
    let lease_after_stale_completion = registry
        .get_process_lease("lease-stale-completion")
        .await
        .expect("read current lease")
        .expect("current lease remains");
    assert_eq!(
        lease_after_stale_completion.lease_token,
        current.lease_token
    );
    assert_eq!(
        lease_after_stale_completion.fencing_token,
        current.fencing_token
    );

    registry
        .register_process(registration("lease-expired-completion"))
        .await
        .expect("register expired-completion process");
    let expired = registry
        .claim_process_lease(
            "lease-expired-completion",
            &process_lease_owner("owner-a"),
            SHORT_TTL_MS,
        )
        .await
        .expect("claim expiring lease")
        .acquired()
        .expect("expiring lease acquired");
    tokio::time::sleep(std::time::Duration::from_millis(SHORT_TTL_MS + 100)).await;
    assert!(
        registry
            .complete_process_with_lease(
                &expired,
                ProcessAwaitOutput::Success {
                    value: serde_json::json!({"writer": "expired"}),
                    control: None,
                },
            )
            .await
            .is_err(),
        "an expired lease must not complete the process"
    );
    assert!(
        !registry
            .get_process("lease-expired-completion")
            .await
            .expect("read expired-completion process")
            .expect("expired-completion process exists")
            .is_terminal()
    );
}

async fn registration_and_observers_are_atomic(registry: Arc<dyn ProcessRegistry>) {
    let record = registry
        .register_process_with_observers(
            registration("observer-registration"),
            &["observer-a".to_string(), "observer-b".to_string()],
        )
        .await
        .expect("register process with observers");
    assert_eq!(record.status, ProcessStatus::Running);
    assert_eq!(
        registry
            .observers_for_process(&record.id)
            .await
            .expect("read process observers"),
        vec!["observer-a".to_string(), "observer-b".to_string()]
    );
    assert!(
        registry
            .is_observer("observer-a", &record.id)
            .await
            .expect("check observer")
    );

    let replay = registry
        .register_process_with_observers(
            registration("observer-registration"),
            &["observer-b".to_string(), "observer-a".to_string()],
        )
        .await
        .expect("replay registration");
    assert_eq!(
        record.registration_fingerprint,
        replay.registration_fingerprint
    );
    assert!(
        registry
            .register_process_with_observers(
                registration("observer-registration"),
                &["observer-a".to_string()],
            )
            .await
            .is_err(),
        "changing the atomic initial observer set must conflict"
    );

    registry
        .register_process(
            registration("wake-without-observer")
                .with_wake_session_id(Some("wake-only-session".to_string())),
        )
        .await
        .expect("register wake-only process");
    assert!(
        !registry
            .is_observer("wake-only-session", "wake-without-observer")
            .await
            .expect("wake target must not imply observer"),
        "no observer edge may be minted from an embedded wake target"
    );
}

async fn observer_events_are_auditable_and_transfer_is_atomic(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "observer-transfer";
    registry
        .register_process_with_observers(registration(process_id), &["observer-source".to_string()])
        .await
        .expect("register observer transfer process");
    registry
        .add_observer(
            "observer-extra",
            process_id,
            crate::ProcessObserverBy::host("add-operation"),
        )
        .await
        .expect("add observer");
    registry
        .remove_observer(
            "observer-extra",
            process_id,
            crate::ProcessObserverBy::host("remove-operation"),
        )
        .await
        .expect("remove observer");
    registry
        .transfer_observers(
            "observer-source",
            "observer-target",
            &[process_id.to_string()],
            crate::ProcessObserverBy::host("transfer-operation"),
        )
        .await
        .expect("transfer observers");

    assert!(
        !registry
            .is_observer("observer-source", process_id)
            .await
            .expect("source observer removed")
    );
    assert!(
        registry
            .is_observer("observer-target", process_id)
            .await
            .expect("target observer added")
    );
    let event_types = registry
        .events_after(process_id, 0)
        .await
        .expect("observer audit log")
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(
        event_types
            .iter()
            .any(|kind| kind == "process.observer_added")
    );
    assert!(
        event_types
            .iter()
            .any(|kind| kind == "process.observer_removed")
    );
}

async fn wake_subscription_is_indexed_and_retargetable(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "wake-retarget";
    registry
        .register_process(
            registration(process_id)
                .with_extra_event_types([wake_event_type("producer.wake")])
                .with_wake_session_id(Some("wake-old".to_string())),
        )
        .await
        .expect("register wake process");
    registry
        .append_event(
            process_id,
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "old"}),
            ),
        )
        .await
        .expect("append old-target wake");
    registry
        .retarget_subscription(process_id, Some("wake-new"))
        .await
        .expect("retarget wake subscription");

    let deliveries = registry
        .list_wake_deliveries(None)
        .await
        .expect("list wake deliveries");
    assert!(deliveries.iter().any(|delivery| {
        delivery.wake.process_id == process_id
            && delivery.discard_reason == Some(crate::WakeDiscardReason::Retargeted)
    }));
    assert!(
        registry
            .events_after(process_id, 0)
            .await
            .expect("retarget audit log")
            .iter()
            .any(|event| event.event_type == "process.subscription_retargeted")
    );
}

async fn lifecycle_status_and_outcome_fold(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "terminal-outcome";
    registry
        .register_process(registration(process_id))
        .await
        .expect("register terminal process");
    let expected = ProcessAwaitOutput::Success {
        value: serde_json::json!({"done": true}),
        control: None,
    };
    let terminal = registry
        .complete_process(
            process_id,
            expected.clone(),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete process");
    assert_eq!(terminal.status, ProcessStatus::Completed);
    assert_eq!(terminal.outcome, Some(expected));
}

async fn list_filters_match_extracted_and_json_fields(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "filter-target";
    let record = registry
        .register_process(
            ProcessRegistration::new(
                process_id,
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                RecoveryDisposition::ExternallyOwned,
                ProcessProvenance::session(SessionScope::new("filter-origin")).with_caused_by(
                    Some(crate::CausalRef::TriggerOccurrence {
                        occurrence_id: "indexed-occurrence-target".to_string(),
                        subscription_id: Some("indexed-subscription-target".to_string()),
                        subscription_incarnation: None,
                        subscription_revision: None,
                    }),
                ),
            )
            .with_identity(
                ProcessIdentity::new("indexed-filter-kind")
                    .with_label(Some("filter-label"))
                    .with_definition(Some(serde_json::json!({"definition": "target"}))),
            ),
        )
        .await
        .expect("register filter target");
    registry
        .set_process_wait(
            process_id,
            WaitState {
                since_ms: record.created_at_ms,
                kind: WaitKind::Signal {
                    name: "ready".to_string(),
                    event_type: "signal.ready".to_string(),
                    key: "filter-target:signal.ready:1".to_string(),
                    ordinal: 1,
                },
            },
        )
        .await
        .expect("set filter target waiting");
    registry
        .register_process(registration("filter-decoy"))
        .await
        .expect("register filter decoy");

    let matches = registry
        .list_processes(&ProcessListFilter {
            definition: Some(serde_json::json!({"definition": "target"})),
            status: ProcessStatusFilter::Waiting,
            waiting: Some(true),
            originator_id: Some(record.originator_id()),
            identity_kind: Some("indexed-filter-kind".to_string()),
            identity_label: Some("filter-label".to_string()),
            caused_by_occurrence_id: Some("indexed-occurrence-target".to_string()),
            caused_by_subscription_id: Some("indexed-subscription-target".to_string()),
            created_at_start_ms: Some(record.created_at_ms),
            created_at_end_ms: Some(record.created_at_ms.saturating_add(1)),
        })
        .await
        .expect("list with all filters");
    assert_eq!(
        matches
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![process_id.to_string()]
    );
}

async fn session_delete_preserves_process_bytes(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "session-delete-bytes";
    registry
        .register_process_with_observers(
            registration(process_id).with_wake_session_id(Some("deleted-session".to_string())),
            &["deleted-session".to_string()],
        )
        .await
        .expect("register session-delete process");
    let before = serde_json::to_vec(
        &registry
            .get_process(process_id)
            .await
            .expect("read before delete")
            .expect("process before delete"),
    )
    .expect("serialize before delete");
    let events_before = serde_json::to_vec(
        &registry
            .events_after(process_id, 0)
            .await
            .expect("read events before delete"),
    )
    .expect("serialize events before delete");
    let report = registry
        .delete_session_process_state("deleted-session")
        .await
        .expect("delete session process state");
    assert_eq!(report.removed_observer_count, 1);
    assert_eq!(report.cleared_subscription_count, 1);
    let after = serde_json::to_vec(
        &registry
            .get_process(process_id)
            .await
            .expect("read after delete")
            .expect("process after delete"),
    )
    .expect("serialize after delete");
    assert_eq!(
        before, after,
        "session delete changed lifecycle record bytes"
    );
    let events_after = serde_json::to_vec(
        &registry
            .events_after(process_id, 0)
            .await
            .expect("read events after delete"),
    )
    .expect("serialize events after delete");
    assert_eq!(
        events_before, events_after,
        "session delete changed process event bytes"
    );
}

async fn tombstones_make_pruned_processes_distinguishable(registry: Arc<dyn ProcessRegistry>) {
    let process_id = "pruned-tombstone";
    registry
        .register_process(registration(process_id))
        .await
        .expect("register prunable process");
    let terminal = registry
        .complete_process(
            process_id,
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("create terminal process");
    let (_, projection_cursor) = registry
        .processes_changed_since(crate::ProcessChangeCursor::initial(), 100)
        .await
        .expect("project terminal row before pruning");
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let prune_cutoff = terminal.updated_at_ms.saturating_add(1);
    registry
        .prune_terminal_processes(
            prune_cutoff,
            None,
            crate::ProjectionWatermark::UpTo(projection_cursor),
        )
        .await
        .expect("prune terminal process");
    let pruned_at_ms = match registry.get_process(process_id).await {
        Err(crate::PluginError::ProcessNoLongerRetained { pruned_at_ms, .. }) => pruned_at_ms,
        other => panic!("expected typed tombstone read, got {other:?}"),
    };
    assert!(
        pruned_at_ms >= prune_cutoff,
        "tombstones must be stamped with prune time, not the retention cutoff"
    );
    let await_output = crate::ProcessAwaiter::polling(Arc::clone(&registry))
        .await_terminal(process_id)
        .await
        .expect("await must render a retained tombstone as a typed outcome");
    assert!(matches!(
        await_output,
        crate::ProcessAwaitOutput::NoLongerRetained { .. }
    ));
    assert!(
        await_output.into_tool_output().is_success(),
        "a retained tombstone await must render as information, not a tool failure"
    );
    assert!(matches!(
        crate::InlineRuntimeEffectController::request_process_cancel(
            Arc::clone(&registry),
            process_id,
            Some("cancel after prune".to_string()),
        )
        .await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert!(matches!(
        registry.events_after(process_id, 0).await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    // A pruned process has no row to hold authority over, so a lease claim must
    // read as the tombstone rather than resurrecting a lease (FIG-953).
    assert!(matches!(
        registry
            .claim_process_lease(process_id, &process_lease_owner("after-prune"), 60_000)
            .await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert!(matches!(
        registry
            .append_event(
                process_id,
                ProcessEventAppendRequest::new("signal.after-prune", serde_json::Value::Null,),
            )
            .await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert!(matches!(
        registry
            .add_observer(
                "late-observer",
                process_id,
                crate::ProcessObserverBy::host("after-prune"),
            )
            .await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert!(matches!(
        registry.is_observer("late-observer", process_id).await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert!(matches!(
        registry.observers_for_process(process_id).await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert!(matches!(
        registry
            .complete_process(
                process_id,
                ProcessAwaitOutput::Success {
                    value: serde_json::Value::Null,
                    control: None,
                },
                ProcessCompletionAuthority::external_owner(),
            )
            .await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert_eq!(
        registry
            .compact_process_tombstones(
                u64::MAX,
                crate::ProjectionWatermark::UpTo(projection_cursor),
                None,
            )
            .await
            .expect("compact behind projector"),
        0,
        "compaction must retain a deletion beyond the supplied projection watermark"
    );
    let (changes, deletion_cursor) = registry
        .processes_changed_since(projection_cursor, 100)
        .await
        .expect("read change feed");
    assert!(changes.into_iter().any(|change| matches!(
        change,
        crate::ProcessChange::Deleted { tombstone } if tombstone.process_id == process_id
    )));
    assert!(
        registry
            .compact_process_tombstones(
                u64::MAX,
                crate::ProjectionWatermark::UpTo(deletion_cursor),
                None,
            )
            .await
            .expect("compact after projector catches up")
            >= 1,
        "compaction must remove the deletion after the projector catches up"
    );
    assert!(
        registry
            .get_process(process_id)
            .await
            .expect("compacted tombstone becomes ordinary absence")
            .is_none()
    );
}

async fn reopen_conformance(handles: ReopenableProcessRegistry) {
    refolded_process_record_matches_stored_projection(
        Arc::clone(&handles.open),
        Arc::clone(&handles.reopen),
        "process-refold-cold",
    )
    .await;
    let mut conservation = ProcessCountConservation::from_modeled_totals(
        REOPEN_BASELINE_SPAWNS,
        REOPEN_BASELINE_PRUNED,
    );
    assert_process_count_conservation(&handles.open, conservation)
        .await
        .expect("known refold registration conserves before reopen assertion");
    let process_id = "observer-reopen";
    handles
        .open
        .register_process_with_observers(
            registration(process_id).with_wake_session_id(Some("wake-reopen".to_string())),
            &["observer-reopen".to_string()],
        )
        .await
        .expect("register before reopen");
    conservation.record_spawn();
    assert_process_count_conservation(&handles.open, conservation)
        .await
        .expect("process counts conserve before reopen");
    assert!(
        handles
            .reopen
            .is_observer("observer-reopen", process_id)
            .await
            .expect("observer survives reopen")
    );
    assert!(
        handles
            .reopen
            .get_process(process_id)
            .await
            .expect("record survives reopen")
            .is_some()
    );
    assert_process_count_conservation(&handles.reopen, conservation)
        .await
        .expect("process counts conserve after reopen");
}
