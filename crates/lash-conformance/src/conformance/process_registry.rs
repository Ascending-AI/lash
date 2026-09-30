//! Cross-backend conformance for the durable process registry.

use lash_sansio::ProcessId;
mod caller_departure;
mod cancellation;
mod completion_authority;
mod consumer_holds;
mod event_count;
mod event_paging;
mod event_replay;
mod external_ref;
mod lifecycle;
mod observer_transfer;
mod parent_end;
mod registration;
mod trigger_delivery_pins;
pub use external_ref::external_ref_is_written_compare_and_set_by_segment_ordinal;
pub use observer_transfer::a_failed_observer_transfer_leaves_no_partial_mutation;
pub use registration::{
    a_host_retry_with_another_wake_target_conflicts,
    a_host_start_key_after_prune_starts_new_for_any_originator,
    a_host_start_key_is_global_and_fences_its_originator,
    a_start_key_after_prune_starts_a_new_process, a_start_key_conflict_names_no_retained_process,
    a_start_key_reports_created_then_existing_and_is_trusted,
    concurrent_starts_under_one_key_register_one_process, keyless_starts_are_always_new,
    process_registry_fresh_instances, registration_and_observers_are_atomic,
};
pub mod status_filters;
mod terminal_publication;
mod turn_parent_end;

use super::process_change_horizon::changes_after_full_relist_if_required;
use super::process_references::{ProcessCountConservation, assert_process_count_conservation};
use super::*;
use crate::ProcessEventLogTestSupport as _;
use crate::{PluginError, ProcessRecord, ProjectionWatermark, TestProcessRegistryWriteExt};
use pretty_assertions::assert_eq;

fn settled_success(value: serde_json::Value) -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(value))
}

fn settled_failure(
    class: crate::ToolFailureClass,
    code: &str,
    message: &str,
) -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
        crate::ToolFailure::runtime(class, code, message),
    ))
}

fn settled_cancellation(message: &str) -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
        crate::ToolCancellation::runtime(message),
    ))
}

pub async fn process_registry_registration_contract(
    registry: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    lifecycle::registration_contract(registry).await;
}

pub async fn empty_tool_call_identifiers_leave_no_row(
    registry: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    lifecycle::empty_tool_call_identifiers_leave_no_row(registry).await;
}

pub async fn process_registry_cancellation_contract(
    registry: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    Box::pin(cancellation::contract(Arc::clone(&registry), registry)).await;
}

pub async fn process_registry_cancellation_reopen_contract(handles: ReopenableProcessRegistry) {
    Box::pin(cancellation::contract(handles.open, handles.reopen)).await;
}

pub async fn canonical_process_event_payload_replay(registry: Arc<dyn ProcessRegistry>) {
    event_replay::canonical_process_event_payload_replay(registry).await;
}

pub async fn count_events_through_counts_every_event_at_any_top_bound(
    registry: Arc<dyn ProcessRegistry>,
) {
    event_count::count_events_through_counts_every_event_at_any_top_bound(registry).await;
}

pub async fn long_cancellation_requester_replay_is_backend_safe(
    registry: Arc<dyn ProcessRegistry>,
) {
    event_replay::long_cancellation_requester_replay_is_backend_safe(registry).await;
}

/// Prove that terminal replay repairs a stale record projection from
/// the persisted tail event on the backend under test.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn external_completion_replay_repairs_projection<C, Fut>(
    registry: Arc<dyn ProcessRegistry>,
    corrupt_projection: C,
) where
    C: FnOnce(ProcessRecord) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let base = registry
        .register_process(ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register external replay repair process");
    let process_id = base.id.clone();
    let output = settled_success(serde_json::json!({"repaired": true}));
    let committed = registry
        .complete_process(
            &process_id,
            output.clone(),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("commit external terminal event");
    assert!(matches!(
        committed,
        crate::ProcessCompletionOutcome::Committed(ref stored) if stored.is_terminal()
    ));

    corrupt_projection(base).await;
    assert!(
        !registry
            .get_process(&process_id)
            .await
            .expect("read deliberately stale projection")
            .expect("stale process exists")
            .is_terminal(),
        "fixture must expose a stale non-terminal projection before replay"
    );

    let replayed = registry
        .complete_process(
            &process_id,
            output,
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("replay external terminal event");
    assert!(matches!(
        replayed,
        crate::ProcessCompletionOutcome::AlreadyApplied { ref stored }
            if stored.is_terminal()
    ));
    assert!(
        registry
            .get_process(&process_id)
            .await
            .expect("read repaired external replay projection")
            .expect("repaired process exists")
            .is_terminal(),
        "external completion replay must persist the repaired terminal projection"
    );
}

/// Prove that the retention filter scopes a prune (ADR 0023): a host pruning
/// one originator's terminal work reclaims exactly those rows and leaves every
/// other originator's row, and its own live row, in place.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_prune_scoped_by_originator(registry: Arc<dyn ProcessRegistry>) {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn register_for(
        registry: &Arc<dyn ProcessRegistry>,
        label: &str,
        scope: &SessionScope,
    ) -> ProcessId {
        registry
            .register_process(
                registration(label)
                    .with_process_provenance(ProcessProvenance::session(scope.clone())),
            )
            .await
            .expect("register scoped prune process")
            .id
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn complete(registry: &Arc<dyn ProcessRegistry>, process_id: &ProcessId) {
        registry
            .complete_process(
                process_id,
                settled_success(serde_json::Value::Null),
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete scoped prune process");
    }

    async fn retained(registry: &Arc<dyn ProcessRegistry>, process_id: &ProcessId) -> bool {
        match registry.get_process(process_id).await {
            Ok(record) => record.is_some(),
            Err(crate::PluginError::ProcessNoLongerRetained { .. }) => false,
            Err(err) => panic!("read scoped prune process: {err:?}"),
        }
    }

    let deleted = SessionScope::for_agent_frame(
        "scoped-prune-deleted",
        crate::session_graph::frame_node_id(
            &SessionId::from("scoped-prune-deleted"),
            "scoped-prune-frame",
        ),
    );
    let surviving = SessionScope::for_agent_frame(
        "scoped-prune-surviving",
        crate::session_graph::frame_node_id(
            &SessionId::from("scoped-prune-surviving"),
            "scoped-prune-frame",
        ),
    );
    let deleted_terminal = register_for(&registry, "scoped-prune-deleted-terminal", &deleted).await;
    complete(&registry, &deleted_terminal).await;
    let deleted_live = register_for(&registry, "scoped-prune-deleted-live", &deleted).await;
    let surviving_terminal =
        register_for(&registry, "scoped-prune-surviving-terminal", &surviving).await;
    complete(&registry, &surviving_terminal).await;

    let report = registry
        .prune_terminal_processes(
            u64::MAX,
            Some(ProcessListFilter {
                status: ProcessStatusFilter::Any,
                originator: Some(ProcessOriginatorFilter::session(deleted.session_id.clone())),
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
        !retained(&registry, &deleted_terminal).await,
        "the filtered originator's terminal row must be gone"
    );
    assert!(
        retained(&registry, &deleted_live).await,
        "a live row is never a prune candidate, whatever the filter matches"
    );
    assert!(
        retained(&registry, &surviving_terminal).await,
        "another originator's terminal row must survive a scoped prune"
    );
    assert_eq!(
        registry
            .filter_tombstoned_process_ids(&[deleted_terminal.clone(), surviving_terminal.clone(),])
            .await
            .expect("classify scoped prune history"),
        vec![deleted_terminal.to_string()],
        "only the reclaimed row becomes a tombstone"
    );

    // A terminal status narrows the same lever further, and the unfiltered
    // sweep still reaches the row a scoped prune deliberately skipped.
    let report = registry
        .prune_terminal_processes(
            u64::MAX,
            Some(ProcessListFilter {
                status: ProcessStatusFilter::any_of([ProcessStatus::Failed]),
                originator: Some(ProcessOriginatorFilter::session(
                    surviving.session_id.clone(),
                )),
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
    assert!(!retained(&registry, &surviving_terminal).await);
    assert!(retained(&registry, &deleted_live).await);
}

/// Prove that one SQL prune batch allocates complete, process-id-ordered
/// tombstone sequences and reports every removed process event.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_prune_batch_tombstones(registry: Arc<dyn ProcessRegistry>) {
    let cases = [
        (
            "batch-prune-a",
            settled_success(serde_json::Value::Null),
            "completed",
        ),
        (
            "batch-prune-b",
            settled_failure(
                crate::ToolFailureClass::External,
                "batch_failure",
                "batch failure",
            ),
            "failed",
        ),
        (
            "batch-prune-c",
            settled_cancellation("batch cancellation"),
            "cancelled",
        ),
    ];
    // Minted ids sort in registration order, so the batch tombstones in the
    // order the cases register.
    let mut case_ids = Vec::new();
    for (label, output, _) in &cases {
        let process_id = registry
            .register_process(registration(label))
            .await
            .expect("register batch-prune process")
            .id;
        case_ids.push(process_id.clone());
        registry
            .complete_process(
                &process_id,
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
    for (((_, _, expected_label), expected_id), expected_sequence) in
        cases.iter().zip(&case_ids).zip([8_u64, 9, 10])
    {
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
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        ProcessIdentity::for_definition(
            lash_core::ProcessDefinitionRef::unclaimed(
                "conformance",
                serde_json::json!({"suite": "process_registry"}),
            ),
            Some(id),
        ),
    ))
}

/// A process lash executes: an engine input with its captured execution env.
/// [`registration`] is an externally-owned row lash never executes.
pub(super) fn executed_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::Engine {
            kind: "conformance".to_string(),
            payload: serde_json::json!({"suite": "process_registry", "id": id}),
        },
        ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
        "process-env:{id}"
    ))))
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        ProcessIdentity::for_definition(
            lash_core::ProcessDefinitionRef::unclaimed(
                "conformance",
                serde_json::json!({"suite": "process_registry"}),
            ),
            Some(id),
        ),
    ))
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

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
pub async fn record_fold_and_retention_hold_for_every_registry_writer(
    registry: Arc<dyn ProcessRegistry>,
) {
    refolded_process_record_matches_stored_projection(
        Arc::clone(&registry),
        registry.clone(),
        "process-refold-hot",
    )
    .await;
    let base = registry
        .register_process(registration("refold-departure"))
        .await
        .expect("register external writer");
    registry
        .record_caller_departure(&base.id)
        .await
        .expect("abandon the external caller");
    assert_refold_matches_stored_projection(&registry, &base, &base.id, "caller departure").await;
    registry
        .complete_process(
            &base.id,
            settled_success(serde_json::Value::Null),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("reconcile abandoned caller");
    assert_refold_matches_stored_projection(&registry, &base, &base.id, "external reconciliation")
        .await;
    for (name, output) in [
        (
            "failed",
            settled_failure(
                crate::ToolFailureClass::Execution,
                "refold_failure",
                "failed",
            ),
        ),
        ("cancelled", settled_cancellation("cancelled")),
        (
            "abandoned",
            ProcessAwaitOutput::Abandoned {
                evidence: Box::new(crate::AbandonEvidence {
                    writer: crate::AbandonWriter::Producer,
                    owner: None,
                    epoch_ms: 1,
                }),
                control: None,
            },
        ),
    ] {
        let terminal_base = registry
            .register_process(registration(&format!("refold-{name}")))
            .await
            .expect("register terminal writer");
        registry
            .complete_process(
                &terminal_base.id,
                output.clone(),
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("terminal variant commits");
        let stored = registry
            .get_process(&terminal_base.id)
            .await
            .expect("read terminal variant")
            .expect("terminal variant retained");
        assert_eq!(stored.outcome, Some(output));
        assert_eq!(stored.status.label(), name);
        assert_refold_matches_stored_projection(&registry, &terminal_base, &terminal_base.id, name)
            .await;
    }
    let (_, terminal_cursor) = registry
        .processes_changed_since(crate::ProcessChangeCursor::initial(), 1000)
        .await
        .expect("project terminal writers");
    assert_eq!(
        registry
            .prune_terminal_processes(
                u64::MAX,
                None,
                ProjectionWatermark::UpTo(crate::ProcessChangeCursor::initial())
            )
            .await
            .expect("prune behind projector")
            .pruned_processes,
        0
    );
    assert!(
        registry
            .get_process(&base.id)
            .await
            .expect("watermark retains row")
            .is_some()
    );
    assert_eq!(
        registry
            .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::UpTo(terminal_cursor))
            .await
            .expect("prune projected writers")
            .pruned_processes,
        5
    );
    assert!(matches!(
        registry.get_process(&base.id).await,
        Err(PluginError::ProcessNoLongerRetained { .. })
    ));
    assert_eq!(
        registry
            .compact_process_tombstones(u64::MAX, ProjectionWatermark::UpTo(terminal_cursor), None)
            .await
            .expect("retain unseen deletions"),
        0
    );
    let (_, deletion_cursor) = registry
        .processes_changed_since(terminal_cursor, 1000)
        .await
        .expect("project deletions");
    assert_eq!(
        registry
            .compact_process_tombstones(u64::MAX, ProjectionWatermark::UpTo(deletion_cursor), None)
            .await
            .expect("compact projected deletions"),
        5
    );
    assert!(
        registry
            .get_process(&base.id)
            .await
            .expect("compacted absence")
            .is_none()
    );
}

pub async fn process_event_pages_reject_out_of_range_sequences(registry: Arc<dyn ProcessRegistry>) {
    event_paging::assert_out_of_range_sequences_are_rejected(registry).await;
}

/// FIG-3611 L4 through the watched registry decorator: the watch layer adds
/// no name that could outlive a pruned process.
pub async fn watched_process_registry_start_key_after_prune_starts_a_new_process(
    registry: Arc<dyn ProcessRegistry>,
) {
    let watched = lash_core::facade_support::watch_process_registry(registry);
    a_start_key_after_prune_starts_a_new_process(Arc::clone(watched.registry())).await;
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn lifecycle_transition_refusals_are_backend_invariant(
    registry: Arc<dyn ProcessRegistry>,
) {
    let departed_id = "transition-refusal-departed-wait";
    let departed = registry
        .register_process(executed_registration(departed_id))
        .await
        .expect("register departed-wait-refusal process");
    let departed_id = departed.id.clone();
    let authority = crate::ProcessExecutionWriteAuthority::invocation(
        departed_id.clone(),
        "transition-refusal:execution",
    )
    .bind_attempt(1);
    registry
        .record_first_started_with_authority(
            &departed_id,
            authority
                .invocation_started()
                .expect("a bound invocation names its execution"),
            &authority,
        )
        .await
        .expect("record departed-wait process execution start");
    assert_session_refusal(
        registry.record_caller_departure(&departed_id).await,
        &format!(
            "process `{departed_id}` is not externally-owned and cannot record a caller departure"
        ),
    );
    registry
        .complete_process(
            &departed_id,
            settled_success(serde_json::Value::Null),
            ProcessCompletionAuthority::workflow_key(departed_id.as_str()),
        )
        .await
        .expect("reconcile departed-wait-refusal process");

    let external_ref_id = "transition-refusal-external-ref";
    let transition_refusal_external_ref_record = registry
        .register_process(registration(external_ref_id))
        .await
        .expect("register external-ref-refusal process");
    let external_ref_id = transition_refusal_external_ref_record.id.clone();
    registry
        .set_external_ref(
            &external_ref_id,
            crate::ProcessExternalRef {
                backend: "first-backend".to_string(),
                id: "first-id".to_string(),
                metadata: None,
                segment_ordinal: None,
            },
        )
        .await
        .expect("record first external reference");
    assert_session_refusal(
        registry
            .set_external_ref(
                &external_ref_id,
                crate::ProcessExternalRef {
                    backend: "second-backend".to_string(),
                    id: "second-id".to_string(),
                    metadata: None,
                    segment_ordinal: None,
                },
            )
            .await,
        &format!(
            "process `{external_ref_id}` external ref conflict: existing first-backend / first-id, requested second-backend / second-id"
        ),
    );
}

fn assert_session_refusal<T>(result: Result<T, crate::PluginError>, expected: &str) {
    match result {
        Err(crate::PluginError::Session(message)) => assert_eq!(message, expected),
        Err(other) => panic!("expected session refusal `{expected}`, got {other:?}"),
        Ok(_) => panic!("expected session refusal `{expected}`, got success"),
    }
}

/// A terminal parent's ledger row survives the retention prune of its own
/// process row: the ledger is keyed by scope, not by the parent row.
pub async fn terminal_completion_atomically_retains_parent_end_plan(
    registry: Arc<dyn ProcessRegistry>,
) {
    parent_end::terminal_completion_atomically_retains_parent_end_plan(registry).await;
}

/// Every transaction that makes a process terminal arms its `ProcessTerminal`
/// obligation once (ADR 0109 §3), whichever completion wrote it, and the
/// engine that published the terminal settles it delivered, once.
pub async fn a_terminal_write_arms_its_publication_once(registry: Arc<dyn ProcessRegistry>) {
    terminal_publication::a_terminal_write_arms_its_publication_once(registry).await;
}

/// ADR 0027, granted half: each completion authority commits on the input
/// class it names, and the terminal event records it as audit evidence.
pub async fn a_completion_authority_matching_its_input_class_commits(
    registry: Arc<dyn ProcessRegistry>,
) {
    completion_authority::a_completion_authority_matching_its_input_class_commits(registry).await;
}

/// ADR 0027, refused half: an external owner never closes an engine-executed
/// row and a workflow authority never closes an externally-owned row; the
/// refusal is typed and writes no terminal.
pub async fn a_completion_authority_for_the_wrong_input_class_is_refused(
    registry: Arc<dyn ProcessRegistry>,
) {
    completion_authority::a_completion_authority_for_the_wrong_input_class_is_refused(registry)
        .await;
}

/// A turn scope has no terminal row to ride, so its ledger row is recorded
/// on its own: the write, its fence, its scoping and its settlement.
pub async fn a_turn_scope_ends_through_its_recorded_ledger_row(registry: Arc<dyn ProcessRegistry>) {
    turn_parent_end::a_turn_scope_ends_through_its_recorded_ledger_row(registry).await;
}

/// A consumer hold's abandonment returns what its call owes a cancel and
/// refuses every later start under the hold (ADR 0116 §3.4).
pub async fn an_abandoned_consumer_hold_fences_registration(registry: Arc<dyn ProcessRegistry>) {
    consumer_holds::an_abandoned_consumer_hold_fences_registration(registry).await;
}

/// A trigger delivery's pin keeps its row from prune until it is released
/// (ADR 0021, FIG-4203).
pub async fn a_trigger_delivery_pin_holds_its_row_until_released(
    registry: Arc<dyn ProcessRegistry>,
) {
    trigger_delivery_pins::a_trigger_delivery_pin_holds_its_row_until_released(registry).await;
}

/// Two scopes whose components render to one stored id under the retired
/// delimiter codec must share no ledger key, children page or fence.
pub async fn scopes_that_collide_in_rendering_share_no_ledger_key(
    registry: Arc<dyn ProcessRegistry>,
) {
    turn_parent_end::scopes_that_collide_in_rendering_share_no_ledger_key(registry).await;
}

/// A turn scope that never became a root is closed by its session's close:
/// the session's row refuses every later start inside the session, and its
/// plan cancels the live children of every scope inside it with no row of its
/// own (FIG-3948).
pub async fn a_session_close_reaps_the_turn_scopes_that_never_became_roots(
    registry: Arc<dyn ProcessRegistry>,
) {
    turn_parent_end::a_session_close_reaps_the_turn_scopes_that_never_became_roots(registry).await;
}

/// A turn that committed without its ledger row is a recovery candidate until
/// the row exists, and no other shape of row ever is.
pub async fn an_unrecorded_turn_parent_is_reported_until_its_row_is_written(
    registry: Arc<dyn ProcessRegistry>,
) {
    turn_parent_end::an_unrecorded_turn_parent_is_reported_until_its_row_is_written(registry).await;
}

/// A session's `Session` scope closes only through its close row, which its
/// `CloseSession` intent writes, never through the deletion of its process
/// state; the row owes its cancels and refuses later starts naming it
/// (FIG-3607 R10, R11).
pub async fn a_session_scope_closes_only_through_its_close_row(registry: Arc<dyn ProcessRegistry>) {
    parent_end::a_session_scope_closes_only_through_its_close_row(registry).await;
}

/// Retention reclaims a settled parent-end ledger row once no live child
/// names its scope, so the ledger does not grow by one row per ended scope
/// forever.
pub async fn settled_parent_end_plans_are_reclaimed_by_retention(
    registry: Arc<dyn ProcessRegistry>,
) {
    parent_end::settled_parent_end_plans_are_reclaimed_by_retention(registry).await;
}

/// Prove bounded keyset pagination and its page-boundary completion contract.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_registry_pagination(registry: Arc<dyn ProcessRegistry>) {
    let mut process_ids = Vec::new();
    for index in 0..7 {
        process_ids.push(
            registry
                .register_process(registration(&format!("paged-process-{index:02}")))
                .await
                .expect("register paged process")
                .id,
        );
    }

    let limit = std::num::NonZeroUsize::new(2).expect("non-zero test page size");
    let first = registry
        .list_non_terminal_processes_page(limit, None)
        .await
        .expect("read first non-terminal page");
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
            settled_success(serde_json::json!({"completed_between_pages": true})),
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
            .list_non_terminal_processes_page(limit, Some(cursor))
            .await
            .expect("read non-terminal page continuation");
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
        returned_ids.iter().filter(|id| *id == boundary_id).count(),
        1,
        "a process completed after its page must not be dispatched again"
    );
    non_terminal_page_excludes_rows_terminalized_before_a_later_page(Arc::clone(&registry)).await;
    non_terminal_page_bound_defers_beyond_bound_insert(registry).await;
}

/// A non-terminal registry pass is bounded per read and visits every row over
/// successive pages, even when its requested page size exceeds the hard cap.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn non_terminal_process_pages_visit_every_row_across_the_page_bound(
    registry: Arc<dyn ProcessRegistry>,
) {
    let mut process_ids = Vec::new();
    for index in 0..=lash_core::MAX_NON_TERMINAL_PROCESS_PAGE_SIZE {
        process_ids.push(
            registry
                .register_process(registration(&format!("bounded-page-{index:04}")))
                .await
                .expect("register bounded-page process")
                .id,
        );
    }

    let requested = std::num::NonZeroUsize::new(usize::MAX).expect("non-zero page size");
    let first = registry
        .list_non_terminal_processes_page(requested, None)
        .await
        .expect("read first bounded process page");
    assert!(
        first.records.len() <= lash_core::MAX_NON_TERMINAL_PROCESS_PAGE_SIZE,
        "a caller request cannot exceed the hard page bound"
    );

    let mut page_count = 1;
    let mut returned_ids = first
        .records
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    let mut continuation = first.continuation;
    while let Some(cursor) = continuation {
        let page = registry
            .list_non_terminal_processes_page(requested, Some(cursor))
            .await
            .expect("read bounded process continuation");
        assert!(
            page.records.len() <= lash_core::MAX_NON_TERMINAL_PROCESS_PAGE_SIZE,
            "each continuation respects the hard page bound"
        );
        page_count += 1;
        returned_ids.extend(page.records.into_iter().map(|record| record.id));
        continuation = page.continuation;
    }

    assert!(page_count > 1, "the fixture must span more than one page");
    for process_id in &process_ids {
        assert_eq!(
            returned_ids.iter().filter(|id| *id == process_id).count(),
            1,
            "every non-terminal row in the scan must be returned exactly once"
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn collect_non_terminal_process_ids(registry: &dyn ProcessRegistry) -> Vec<ProcessId> {
    let limit = std::num::NonZeroUsize::new(128).expect("non-zero test page size");
    let mut continuation = None;
    let mut ids = Vec::new();
    loop {
        let page = registry
            .list_non_terminal_processes_page(limit, continuation)
            .await
            .expect("scan complete non-terminal registry");
        ids.extend(page.records.into_iter().map(|record| record.id));
        let Some(next) = page.continuation else {
            return ids;
        };
        continuation = Some(next);
    }
}

/// A row that terminalizes before its not-yet-read page is no longer recovery work.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn non_terminal_page_excludes_rows_terminalized_before_a_later_page(
    registry: Arc<dyn ProcessRegistry>,
) {
    let mut ids = Vec::new();
    for label in [
        "non-terminal-page-later-a",
        "non-terminal-page-later-b",
        "non-terminal-page-later-c",
    ] {
        ids.push(
            registry
                .register_process(registration(label))
                .await
                .expect("register later-page terminalization fixture")
                .id,
        );
    }
    // Minted ids order the scan; the law needs only their relative order,
    // never a chosen position.
    ids.sort();
    let [first_id, terminalized_id, last_id] =
        <[ProcessId; 3]>::try_from(ids).expect("three fixture ids");
    let limit = std::num::NonZeroUsize::new(1).expect("non-zero test page size");
    // Page up to and including the first fixture row, so the other two are
    // still ahead of the cursor.
    let mut continuation = None;
    loop {
        let page = registry
            .list_non_terminal_processes_page(limit, continuation)
            .await
            .expect("read up to the first later-page fixture");
        let reached = page.records.iter().any(|record| record.id == first_id);
        continuation = page.continuation;
        if reached {
            break;
        }
        assert!(
            continuation.is_some(),
            "the scan must reach the first fixture row"
        );
    }
    registry
        .complete_process(
            &terminalized_id,
            settled_success(serde_json::json!({"terminalized_before_page": true})),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("terminalize row before its page");

    let mut ids = Vec::new();
    while let Some(cursor) = continuation {
        let page = registry
            .list_non_terminal_processes_page(limit, Some(cursor))
            .await
            .expect("read later-page terminalization continuation");
        ids.extend(page.records.into_iter().map(|record| record.id));
        continuation = page.continuation;
    }
    assert!(!ids.contains(&terminalized_id));
    assert!(ids.contains(&last_id));
}

/// An insert beyond the captured upper bound waits for the next scan.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn non_terminal_page_bound_defers_beyond_bound_insert(
    registry: Arc<dyn ProcessRegistry>,
) {
    let limit = std::num::NonZeroUsize::new(1).expect("non-zero test page size");
    let first = registry
        .list_non_terminal_processes_page(limit, None)
        .await
        .expect("capture bounded non-terminal scan");
    // A registrar mints ids in order, so a row registered after the scan
    // captured its bound sorts beyond it.
    let inserted_id = registry
        .register_process(registration("page-after-captured-bound"))
        .await
        .expect("insert beyond captured bound")
        .id;

    let mut current_scan_ids = first
        .records
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    let mut continuation = first.continuation;
    while let Some(cursor) = continuation {
        let page = registry
            .list_non_terminal_processes_page(limit, Some(cursor))
            .await
            .expect("read captured-bound continuation");
        current_scan_ids.extend(page.records.into_iter().map(|record| record.id));
        continuation = page.continuation;
    }
    assert!(
        !current_scan_ids.contains(&inserted_id),
        "an insert beyond the captured bound must not leak into the current scan"
    );
    assert!(
        collect_non_terminal_process_ids(registry.as_ref())
            .await
            .contains(&inserted_id),
        "the next scan must include the beyond-bound insert"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn refolded_process_record_matches_stored_projection(
    writer: Arc<dyn ProcessRegistry>,
    reader: Arc<dyn ProcessRegistry>,
    case: &str,
) {
    let base = writer
        .register_process(
            ProcessRegistration::new(
                ProcessInput::Engine {
                    kind: "refold-conformance".to_string(),
                    payload: serde_json::json!({"case": case}),
                },
                ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
                "process-env:{case}"
            ))))
            .with_extra_event_types([plain_event_type("signal.ready")]),
        )
        .await
        .expect("register refold process");
    let process_id = &base.id.clone();
    assert_refold_matches_stored_projection(&reader, &base, process_id, "registration").await;
    let authority = crate::ProcessExecutionWriteAuthority::invocation(
        (*process_id).clone(),
        format!("refold-worker:{process_id}"),
    )
    .bind_attempt(1);
    writer
        .record_first_started_with_authority(
            process_id,
            crate::ProcessStarted {
                owner: authority.owner_identity(),
                attempt: 1,
                started_at_ms: base.created_at_ms,
                build_generation: None,
                generation: None,
            },
            &authority,
        )
        .await
        .expect("record refold first start");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "first start").await;
    writer
        .park_process_with_authority(
            process_id,
            crate::store::ParkReason::ReplayDivergence {
                message: "refold park".to_string(),
            }
            .into(),
            &authority,
        )
        .await
        .expect("park refold process");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "park entered").await;
    writer
        .begin_parked_rerun_with_authority(process_id, &authority)
        .await
        .expect("begin parked rerun");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "parked rerun").await;
    let wait = WaitState {
        since_ms: base.created_at_ms,
        kind: WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: lash_core::runtime::process_signal_wait_key(process_id, "ready", 1),
            ordinal: 1,
        },
    };
    writer
        .set_process_wait_with_authority(process_id, wait, Vec::new(), &authority)
        .await
        .expect("enter refold wait");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "wait entered").await;
    writer
        .clear_process_wait_with_authority(process_id, Vec::new(), &authority)
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
                segment_ordinal: None,
            },
        )
        .await
        .expect("set refold external reference");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "external ref set").await;
    let signal =
        ProcessEventAppendRequest::new("signal.ready", serde_json::json!({"signal": "ready"}))
            .with_replay_key(lash_core::runtime::process_signal_wait_key(
                process_id, "ready", 1,
            ));
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
        .append_event_with_authority(
            process_id,
            ProcessEventAppendRequest::new("signal.ready", serde_json::json!("authorized append")),
            &authority,
        )
        .await
        .expect("authorized event writer");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "authorized append").await;
    let batch = writer
        .append_events(
            process_id,
            vec![
                ProcessEventAppendRequest::new("signal.ready", serde_json::json!("batch first")),
                ProcessEventAppendRequest::new("signal.ready", serde_json::json!("batch second")),
            ],
            &authority,
        )
        .await
        .expect("batch event writer");
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[1].event.sequence, batch[0].event.sequence + 1);
    assert_refold_matches_stored_projection(&reader, &base, process_id, "batch append").await;
    writer
        .add_observer(
            &SessionId::from("refold-observer"),
            process_id,
            crate::ProcessObserverBy::host("refold-add"),
        )
        .await
        .expect("add refold observer");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "observer added").await;
    writer
        .transfer_observers(
            &SessionId::from("refold-observer"),
            &SessionId::from("refold-transferred"),
            std::slice::from_ref(process_id),
            crate::ProcessObserverBy::host("refold-transfer"),
        )
        .await
        .expect("transfer observer");
    assert!(
        !reader
            .is_observer(&SessionId::from("refold-observer"), process_id)
            .await
            .expect("old observer absent")
    );
    assert!(
        reader
            .is_observer(&SessionId::from("refold-transferred"), process_id)
            .await
            .expect("new observer present")
    );
    assert_refold_matches_stored_projection(&reader, &base, process_id, "observer transferred")
        .await;
    writer
        .remove_observer(
            &SessionId::from("refold-transferred"),
            process_id,
            crate::ProcessObserverBy::host("refold-remove"),
        )
        .await
        .expect("remove refold observer");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "observer removed").await;
    writer
        .retarget_subscription(process_id, Some("refold-wake"))
        .await
        .expect("retarget wake subscription");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "wake retarget").await;
    writer
        .request_process_cancel(
            process_id,
            crate::CancelOrigin::OperatorRequested,
            "refold-operator".to_string(),
            None,
        )
        .await
        .expect("record cancellation");
    assert_refold_matches_stored_projection(&reader, &base, process_id, "cancel requested").await;
    let faults = Arc::new(crate::testing::ProcessRegistryFaults::new(writer.clone()));
    let watched = lash_core::facade_support::watch_process_registry(faults.clone());
    let sink = Arc::new(RefoldSink::default());
    let _sink_guard = watched.add_event_sink(sink.clone());
    let before = reader
        .full_event_window(process_id, 0)
        .await
        .expect("event preimage");
    faults.fail_next_event_append(PluginError::Session("injected failed commit".to_string()));
    assert!(
        watched
            .registry()
            .append_event(
                process_id,
                ProcessEventAppendRequest::new("signal.ready", serde_json::json!("failed"))
            )
            .await
            .is_err()
    );
    assert!(
        sink.0.lock().expect("sink lock").is_empty(),
        "a failed append publishes no event"
    );
    assert_eq!(
        serde_json::to_value(
            reader
                .full_event_window(process_id, 0)
                .await
                .expect("failed append log")
        )
        .expect("serialize log"),
        serde_json::to_value(before).expect("serialize preimage")
    );
    assert_refold_matches_stored_projection(&reader, &base, process_id, "failed append").await;
    watched
        .registry()
        .append_event(
            process_id,
            ProcessEventAppendRequest::new("signal.ready", serde_json::json!("committed")),
        )
        .await
        .expect("positive publication control");
    assert_eq!(sink.0.lock().expect("sink lock").len(), 1);
    assert_refold_matches_stored_projection(&reader, &base, process_id, "published append").await;
    writer
        .complete_process_with_prelude(
            process_id,
            settled_success(serde_json::json!({"refolded": true})),
            vec![ProcessEventAppendRequest::new(
                "signal.ready",
                serde_json::json!("terminal prelude"),
            )],
            ProcessCompletionAuthority::workflow_key(format!("refold:{process_id}")),
        )
        .await
        .expect("complete refold process");

    assert_refold_matches_stored_projection(&reader, &base, process_id, "terminal completion")
        .await;
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_refold_matches_stored_projection(
    reader: &Arc<dyn ProcessRegistry>,
    base: &ProcessRecord,
    process_id: &ProcessId,
    transition: &str,
) {
    let events = reader
        .full_event_window(process_id, 0)
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn producer_terminal_status_must_match_materialized_outcome(
    registry: Arc<dyn ProcessRegistry>,
) {
    let record = registry
        .register_process(
            registration("producer-terminal-outcome-mismatch").with_extra_event_types([
                ProcessEventType {
                    name: "producer.failed".to_string(),
                    payload_schema: LashSchema::any(),
                    semantics: ProcessEventSemanticsSpec {
                        terminal: Some(crate::ProcessTerminalSpec {
                            status: ProcessStatus::Failed,
                            await_output: Some(ProcessValueSelector::Pointer("/out".to_string())),
                        }),
                        ..ProcessEventSemanticsSpec::default()
                    },
                },
            ]),
        )
        .await
        .expect("register producer terminal event");
    let process_id = record.id.clone();
    let before = serde_json::to_vec(&record).expect("serialize producer before rejected append");
    let error = registry
        .append_event(
            &process_id,
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
        .get_process(&process_id)
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
            .full_event_window(&process_id, 0)
            .await
            .expect("read events after rejected append")
            .is_empty(),
        "rejected core terminal semantics must not append an event"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn generic_append_rejects_reserved_edge_audit_events(registry: Arc<dyn ProcessRegistry>) {
    let process_id = registry
        .register_process(registration("reserved-edge-audit"))
        .await
        .expect("register reserved-audit process")
        .id;
    let by = crate::ProcessObserverBy::host("generic-append");
    let requests = [
        ProcessEventAppendRequest::observer_added(&process_id, "observer", &by),
        ProcessEventAppendRequest::observer_removed(&process_id, "observer", &by),
        ProcessEventAppendRequest::subscription_retargeted(&process_id, Some("target")),
    ];
    for request in requests {
        let event_type = request.event_type.clone();
        assert!(
            matches!(
                registry.append_event(&process_id, request).await,
                Err(crate::PluginError::ReservedProcessEvent {
                    event_type: rejected
                }) if rejected == event_type
            ),
            "generic append must reject reserved edge audit event `{event_type}`"
        );
    }
    assert!(
        !registry
            .is_observer(&SessionId::from("observer"), &process_id)
            .await
            .expect("observer query remains available")
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn waiting_processes_remain_in_the_non_terminal_scan(registry: Arc<dyn ProcessRegistry>) {
    let definition = serde_json::json!({"suite": "waiting-non-terminal-scan"});
    let env_ref = ProcessExecutionEnvRef::new("process-env:waiting-non-terminal-scan");
    let count_before = registry
        .count_non_terminal_processes()
        .await
        .expect("count existing non-terminal processes");
    let record = registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::Engine {
                    kind: "waiting-non-terminal-scan".to_string(),
                    payload: serde_json::Value::Null,
                },
                ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
                ProcessIdentity::for_definition(
                    lash_core::ProcessDefinitionRef::unclaimed(
                        "waiting-non-terminal-scan",
                        definition.clone(),
                    ),
                    None::<String>,
                ),
            ))
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await
        .expect("register waiting process");
    let process_id = record.id.clone();
    let authority = crate::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        format!("waiting-scan:{process_id}"),
    )
    .bind_attempt(1);
    registry
        .record_first_started_with_authority(
            &process_id,
            authority
                .invocation_started()
                .expect("the authority is bound to attempt one"),
            &authority,
        )
        .await
        .expect("start the waiting process");
    registry
        .set_process_wait_with_authority(
            &process_id,
            WaitState {
                since_ms: record.created_at_ms,
                kind: WaitKind::Signal {
                    name: "resume".to_string(),
                    event_type: "signal.resume".to_string(),
                    key: format!("{process_id}:signal.resume:1"),
                    ordinal: 1,
                },
            },
            Vec::new(),
            &authority,
        )
        .await
        .expect("park process");

    let non_terminal = registry
        .list_non_terminal_processes_page(
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
            summary
                .definition
                .as_ref()
                .map(|reference| reference.definition.as_json())
                == Some(&definition)
                && summary.env_ref.as_ref() == Some(&env_ref)
                && summary.process_count == 1
        }),
        "live-reference accounting must retain waiting process rows"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn observer_events_are_auditable_and_transfer_is_atomic(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = registry
        .register_process_with_observers(
            registration("observer-transfer"),
            &[SessionId::from("observer-source")],
        )
        .await
        .expect("register observer transfer process")
        .id;
    registry
        .add_observer(
            &SessionId::from("observer-extra"),
            &process_id,
            crate::ProcessObserverBy::host("add-operation"),
        )
        .await
        .expect("add observer");
    registry
        .remove_observer(
            &SessionId::from("observer-extra"),
            &process_id,
            crate::ProcessObserverBy::host("remove-operation"),
        )
        .await
        .expect("remove observer");
    registry
        .transfer_observers(
            &SessionId::from("observer-source"),
            &SessionId::from("observer-target"),
            std::slice::from_ref(&process_id),
            crate::ProcessObserverBy::host("transfer-operation"),
        )
        .await
        .expect("transfer observers");

    assert!(
        !registry
            .is_observer(&SessionId::from("observer-source"), &process_id)
            .await
            .expect("source observer removed")
    );
    assert!(
        registry
            .is_observer(&SessionId::from("observer-target"), &process_id)
            .await
            .expect("target observer added")
    );
    let event_types = registry
        .full_event_window(&process_id, 0)
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn wake_subscription_is_indexed_and_retargetable(registry: Arc<dyn ProcessRegistry>) {
    let process_id = registry
        .register_process(
            registration("wake-retarget")
                .with_extra_event_types([wake_event_type("producer.wake")])
                .with_wake_session_id(Some(SessionId::from("wake-old"))),
        )
        .await
        .expect("register wake process")
        .id;
    registry
        .append_event(
            &process_id,
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "old"}),
            ),
        )
        .await
        .expect("append old-target wake");
    registry
        .retarget_subscription(&process_id, Some("wake-new"))
        .await
        .expect("retarget wake subscription");

    let deliveries = registry
        .list_wake_deliveries(None)
        .await
        .expect("list wake deliveries");
    assert!(deliveries.iter().any(|delivery| {
        delivery.wake.process_id == process_id
            && delivery.disposition.discard_reason() == Some(crate::WakeDiscardReason::Retargeted)
    }));
    assert!(
        registry
            .full_event_window(&process_id, 0)
            .await
            .expect("retarget audit log")
            .iter()
            .any(|event| event.event_type == "process.subscription_retargeted")
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn lifecycle_status_and_outcome_fold(registry: Arc<dyn ProcessRegistry>) {
    let process_id = registry
        .register_process(registration("terminal-outcome"))
        .await
        .expect("register terminal process")
        .id;
    let expected = settled_success(serde_json::json!({"done": true}));
    let terminal = registry
        .complete_process(
            &process_id,
            expected.clone(),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete process");
    assert_eq!(terminal.status, ProcessStatus::Completed);
    assert_eq!(terminal.outcome, Some(expected));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_delete_preserves_process_bytes(registry: Arc<dyn ProcessRegistry>) {
    let process_id = registry
        .register_process_with_observers(
            registration("session-delete-bytes")
                .with_wake_session_id(Some(SessionId::from("deleted-session"))),
            &[SessionId::from("deleted-session")],
        )
        .await
        .expect("register session-delete process")
        .id;
    let before = serde_json::to_vec(
        &registry
            .get_process(&process_id)
            .await
            .expect("read before delete")
            .expect("process before delete"),
    )
    .expect("serialize before delete");
    let events_before = serde_json::to_vec(
        &registry
            .full_event_window(&process_id, 0)
            .await
            .expect("read events before delete"),
    )
    .expect("serialize events before delete");
    let report = registry
        .delete_session_process_state(&SessionId::from("deleted-session"))
        .await
        .expect("delete session process state");
    assert_eq!(report.removed_observer_count, 1);
    assert_eq!(report.cleared_subscription_count, 1);
    let after = serde_json::to_vec(
        &registry
            .get_process(&process_id)
            .await
            .expect("read after delete")
            .expect("process after delete"),
    )
    .expect("serialize after delete");
    assert_eq!(
        before, after,
        "session delete changed lifecycle record bytes"
    );
    let events_after_delete = serde_json::to_vec(
        &registry
            .full_event_window(&process_id, 0)
            .await
            .expect("read events after delete"),
    )
    .expect("serialize events after delete");
    assert_eq!(
        events_before, events_after_delete,
        "session delete changed process event bytes"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn tombstones_make_pruned_processes_distinguishable(registry: Arc<dyn ProcessRegistry>) {
    let process_id = registry
        .register_process(registration("pruned-tombstone"))
        .await
        .expect("register prunable process")
        .id;
    let terminal = registry
        .complete_process(
            &process_id,
            settled_success(serde_json::Value::Null),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("create terminal process");
    let mut projection_cursor = crate::ProcessChangeCursor::initial();
    loop {
        let (changes, next_cursor) = registry
            .processes_changed_since(projection_cursor, 100)
            .await
            .expect("project terminal row before pruning");
        projection_cursor = next_cursor;
        if changes.is_empty() {
            break;
        }
    }
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
    let pruned_at_ms = match registry.get_process(&process_id).await {
        Err(crate::PluginError::ProcessNoLongerRetained { pruned_at_ms, .. }) => pruned_at_ms,
        other => panic!("expected typed tombstone read, got {other:?}"),
    };
    assert!(
        pruned_at_ms >= prune_cutoff,
        "tombstones must be stamped with prune time, not the retention cutoff"
    );
    let await_output = crate::NoProcessWork::for_registry(Arc::clone(&registry))
        .await_terminal(&process_id)
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
    // A cancel names the process by its current reference; a pruned process
    // has none to name, so the request reads as the tombstone.
    assert!(matches!(
        registry.require_process_id(&process_id).await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert!(matches!(
        registry.full_event_window(&process_id, 0).await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    event_paging::assert_pruned_history(&registry, &process_id, pruned_at_ms).await;
    assert!(matches!(
        registry
            .append_event(
                &process_id,
                ProcessEventAppendRequest::new("signal.after-prune", serde_json::Value::Null,),
            )
            .await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert!(matches!(
        registry
            .add_observer(
                &SessionId::from("late-observer"),
                &process_id,
                crate::ProcessObserverBy::host("after-prune"),
            )
            .await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert!(matches!(
        registry
            .is_observer(&SessionId::from("late-observer"), &process_id)
            .await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert!(matches!(
        registry.observers_for_process(&process_id).await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert!(matches!(
        registry
            .complete_process(
                &process_id,
                settled_success(serde_json::Value::Null),
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
            .get_process(&process_id)
            .await
            .expect("compacted tombstone becomes ordinary absence")
            .is_none()
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_registry_reopen_conformance(handles: ReopenableProcessRegistry) {
    let open: Arc<dyn ProcessRegistry> = handles.open.clone();
    let reopen: Arc<dyn ProcessRegistry> = handles.reopen.clone();
    refolded_process_record_matches_stored_projection(
        Arc::clone(&open),
        Arc::clone(&reopen),
        "process-refold-cold",
    )
    .await;
    let mut conservation = ProcessCountConservation::default();
    conservation.record_spawn();
    assert_process_count_conservation(&open, conservation)
        .await
        .expect("known refold registration conserves before reopen assertion");
    let process_id = handles
        .open
        .register_process_with_observers(
            registration("observer-reopen")
                .with_wake_session_id(Some(SessionId::from("wake-reopen"))),
            &[SessionId::from("observer-reopen")],
        )
        .await
        .expect("register before reopen")
        .id;
    conservation.record_spawn();
    assert_process_count_conservation(&open, conservation)
        .await
        .expect("process counts conserve before reopen");
    assert!(
        handles
            .reopen
            .is_observer(&SessionId::from("observer-reopen"), &process_id)
            .await
            .expect("observer survives reopen")
    );
    assert!(
        handles
            .reopen
            .get_process(&process_id)
            .await
            .expect("record survives reopen")
            .is_some()
    );
    assert_process_count_conservation(&reopen, conservation)
        .await
        .expect("process counts conserve after reopen");
}

/// The complete caller-departure state machine, pinned identically on every
/// backend (FIG-1383).
///
/// Every transition in the model is exercised here, legal and illegal alike:
/// the state is durable, reachable only from a running Externally-Owned row,
/// idempotent, refused from every other source state and input class, closable
/// by external reconciliation, and never retracts a reconciled terminal state.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn caller_departure_state_machine(registry: Arc<dyn ProcessRegistry>) {
    let observer_session = "caller-departure-observer";
    let registered = registry
        .register_process_with_observers(
            registration("caller-departure-machine"),
            &[SessionId::from(observer_session)],
        )
        .await
        .expect("register externally-owned audit row");
    let process_id = registered.id.clone();
    assert_eq!(registered.status, ProcessStatus::Running);

    // running -> caller_departed.
    let departed = registry
        .record_caller_departure(&process_id)
        .await
        .expect("running externally-owned row records a caller departure");
    assert_eq!(departed.status, ProcessStatus::CallerDeparted);
    assert!(
        !departed.is_terminal(),
        "the state must never claim an outcome lash cannot observe"
    );
    assert!(
        departed.outcome.is_none(),
        "a caller-departed row carries no outcome"
    );
    assert!(departed.status.is_retired());

    // The transition is a durable event, so the fold reproduces it.
    let events = registry
        .full_event_window(&process_id, 0)
        .await
        .expect("read caller-departure events");
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "process.caller_departed"),
        "the transition must be an appended lifecycle event, not a silent column write"
    );
    assert_refold_matches_stored_projection(
        &registry,
        &registered,
        &process_id,
        "caller departure",
    )
    .await;

    // caller_departed -> caller_departed is idempotent.
    let again = registry
        .record_caller_departure(&process_id)
        .await
        .expect("repeat departure is idempotent");
    assert_eq!(again.status, ProcessStatus::CallerDeparted);
    assert_eq!(again.updated_at_ms, departed.updated_at_ms);

    // A caller-departed row is not live: recovery must never pick it up, and a
    // live listing must not present it as work still in flight.
    let page = registry
        .list_non_terminal_processes_page(
            std::num::NonZeroUsize::new(256).expect("non-zero page size"),
            None,
        )
        .await
        .expect("read non-terminal registry page")
        .records;
    assert!(
        !page.iter().any(|record| record.id == process_id),
        "recovery may never act on a caller-departed row"
    );
    let live = registry
        .list_processes(&ProcessListFilter {
            status: crate::ProcessStatusFilter::any_of([crate::ProcessStatus::Running]),
            ..ProcessListFilter::default()
        })
        .await
        .expect("list running rows");
    assert!(!live.iter().any(|record| record.id == process_id));
    // The session-scoped live view is the same partition: a caller-departed
    // row is retired, so every ProcessListMode::Live reader must stop showing
    // it as in flight, while the unfiltered observation view still carries it.
    let live_observed = registry
        .list_live_observed_by(&SessionId::from(observer_session))
        .await
        .expect("list live observed rows");
    assert!(
        !live_observed.iter().any(|record| record.id == process_id),
        "a caller-departed row must never appear in a live observation listing"
    );
    let all_observed = registry
        .list_observed_by(
            &SessionId::from(observer_session),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list all observed rows");
    assert!(
        all_observed.iter().any(|record| record.id == process_id),
        "the observer edge survives the departure; only the live partition drops it"
    );
    // ...but external reconciliation can enumerate it by name on any backend.
    let departed_rows = registry
        .list_processes(&ProcessListFilter {
            status: crate::ProcessStatusFilter::any_of([crate::ProcessStatus::CallerDeparted]),
            ..ProcessListFilter::default()
        })
        .await
        .expect("list caller-departed rows");
    assert!(
        departed_rows.iter().any(|record| record.id == process_id),
        "external reconciliation must be able to find the state on every backend"
    );

    // An externally-owned row has no engine invocation authority, so it cannot
    // enter an execution wait state after caller departure.
    let wait_refusal = registry
        .set_process_wait(
            &process_id,
            WaitState {
                since_ms: departed.updated_at_ms,
                kind: WaitKind::Signal {
                    name: "resume".to_string(),
                    event_type: "signal.resume".to_string(),
                    key: format!("{process_id}:signal.resume:1"),
                    ordinal: 1,
                },
            },
        )
        .await;
    assert!(
        wait_refusal.is_err(),
        "a caller-departed row must not enter a wait state"
    );

    // Illegal: departures belong to rows lash never executes.
    let executed_id = registry
        .register_process(executed_registration("caller-departure-executed"))
        .await
        .expect("register a lash-executed row")
        .id;
    let ownership_refusal = registry.record_caller_departure(&executed_id).await;
    assert!(
        ownership_refusal.is_err(),
        "only an externally-owned row can record a caller departure"
    );

    // Illegal: an unknown row.
    assert!(
        registry
            .record_caller_departure(&crate::ProcessId::fixture("caller-departure-missing"))
            .await
            .is_err(),
        "an unknown process cannot record a caller departure"
    );

    // Legal: external reconciliation closes the row with observed truth.
    registry
        .complete_process(
            &process_id,
            settled_success(serde_json::json!({"reconciled": true})),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("external reconciliation closes a caller-departed row");
    let closed = registry
        .get_process(&process_id)
        .await
        .expect("read reconciled row")
        .expect("reconciled row remains stored");
    assert_eq!(
        closed.status,
        ProcessStatus::Completed,
        "reconciliation, not lash, supplies the outcome"
    );

    // Replaying the earlier departure is a no-op once reconciliation appended
    // a later terminal event. The terminal projection must not go back.
    let terminal_replay = registry
        .record_caller_departure(&process_id)
        .await
        .expect("the earlier non-tail departure replays without repair");
    assert_eq!(
        terminal_replay, closed,
        "a recorded outcome cannot be retracted into a caller departure"
    );
}

pub async fn caller_departed_rows_are_reclaimed_by_retention(registry: Arc<dyn ProcessRegistry>) {
    caller_departure::caller_departed_rows_are_reclaimed_by_retention(registry).await;
}

#[derive(Default)]
struct RefoldSink(std::sync::Mutex<Vec<crate::ProcessEvent>>);
#[async_trait::async_trait]
impl crate::ProcessEventSink for RefoldSink {
    async fn emit(&self, event: &crate::ProcessEvent) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(event.clone());
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
pub async fn signals_refuse_undeclared_invalid_and_terminal_sends(
    registry: Arc<dyn ProcessRegistry>,
) {
    let base = registry
        .register_process(
            registration("signal-refusal-matrix").with_extra_event_types([ProcessEventType {
                name: "signal.ready".to_string(),
                payload_schema: LashSchema::new(serde_json::json!({"type":"integer"})),
                semantics: Default::default(),
            }]),
        )
        .await
        .expect("register typed signal");
    for (name, payload, reason) in [
        ("signal.missing", serde_json::json!(1), "undeclared"),
        ("signal.ready", serde_json::json!("invalid"), "invalid"),
    ] {
        let error = registry
            .append_event(&base.id, ProcessEventAppendRequest::new(name, payload))
            .await
            .expect_err("signal refused");
        assert!(
            matches!(error, PluginError::Session(ref message) if message.contains(reason)),
            "{error:?}"
        );
        assert_eq!(
            registry
                .get_process(&base.id)
                .await
                .expect("unchanged record"),
            Some(base.clone())
        );
        assert!(
            registry
                .full_event_window(&base.id, 0)
                .await
                .expect("unchanged log")
                .is_empty()
        );
    }
    let key = lash_core::runtime::process_signal_wait_key(&base.id, "ready", 1);
    let request =
        ProcessEventAppendRequest::new("signal.ready", serde_json::json!(7)).with_replay_key(key);
    let first = registry
        .append_event(&base.id, request.clone())
        .await
        .expect("valid signal");
    registry
        .complete_process(
            &base.id,
            settled_success(serde_json::json!("done")),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("terminal writer");
    let terminal = registry
        .get_process(&base.id)
        .await
        .expect("terminal record");
    let events = registry
        .full_event_window(&base.id, 0)
        .await
        .expect("terminal log");
    assert_eq!(
        serde_json::to_value(
            registry
                .append_event(&base.id, request)
                .await
                .expect("recorded signal replays after terminal")
                .event
        )
        .expect("serialize replay"),
        serde_json::to_value(first.event).expect("serialize original")
    );
    assert!(matches!(
        registry
            .append_event(
                &base.id,
                ProcessEventAppendRequest::new("signal.ready", serde_json::json!(8))
                    .with_replay_key(lash_core::runtime::process_signal_wait_key(
                        &base.id, "ready", 2
                    ))
            )
            .await,
        Err(PluginError::ProcessAlreadyTerminal {
            status: ProcessStatus::Completed,
            ..
        })
    ));
    assert_eq!(
        registry
            .get_process(&base.id)
            .await
            .expect("terminal preimage preserved"),
        terminal
    );
    assert_eq!(
        serde_json::to_value(
            registry
                .full_event_window(&base.id, 0)
                .await
                .expect("terminal log preserved")
        )
        .expect("serialize log"),
        serde_json::to_value(events).expect("serialize preimage")
    );
}

struct ReattachingWorkPort {
    process_id: ProcessId,
    output: ProcessAwaitOutput,
    waits: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl crate::ProcessWorkSubstrate for ReattachingWorkPort {
    async fn deliver_process_start(&self, _: &ProcessRecord) -> Result<(), PluginError> {
        Err(PluginError::Invoke("unexpected start delivery".to_string()))
    }

    async fn await_process_terminal(
        &self,
        process_id: &ProcessId,
    ) -> Result<crate::ProcessTerminalWait, PluginError> {
        assert_eq!(
            process_id, &self.process_id,
            "reattachment preserves the explicit id"
        );
        match self.waits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => Ok(crate::ProcessTerminalWait::Reattach),
            1 => Ok(crate::ProcessTerminalWait::Terminal(self.output.clone())),
            _ => Err(PluginError::Invoke("external attach refused".to_string())),
        }
    }

    async fn deliver_cancel(
        &self,
        _: &ProcessId,
        _: &crate::CancelRequest,
        _: &str,
    ) -> Result<(), PluginError> {
        Err(PluginError::Invoke(
            "unexpected cancel delivery".to_string(),
        ))
    }

    async fn publish_process_terminal(
        &self,
        _: &ProcessId,
        _: &ProcessAwaitOutput,
        _: &str,
    ) -> Result<(), PluginError> {
        Err(PluginError::Invoke(
            "unexpected terminal publication".to_string(),
        ))
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
pub async fn work_wait_seam_covers_unknown_pruned_departed_and_external_processes(
    registry: Arc<dyn ProcessRegistry>,
) {
    use crate::ProcessWorkSubstrate as _;
    let work = crate::NoProcessWork::for_registry(registry.clone());
    // This backend owns a process absent from the local registry. Await must
    // use the port and preserve its error, without a registry polling fallback.
    let port = Arc::new(ReattachingWorkPort {
        process_id: ProcessId::fixture("external-backend-only"),
        output: settled_success(serde_json::json!({"backend":"authoritative"})),
        waits: std::sync::atomic::AtomicUsize::new(0),
    });
    let await_backend = || {
        crate::RuntimeEffectLocalExecutor::processes(registry.clone(), port.clone())
            .into_process()
            .expect("production process executor")
            .execute(crate::ProcessCommand::Await {
                process_id: port.process_id.clone(),
            })
    };
    assert!(matches!(
        await_backend().await.expect("reattached terminal"),
        crate::ProcessEffectOutcome::Await { output } if *output == port.output
    ));
    assert_eq!(port.waits.load(std::sync::atomic::Ordering::SeqCst), 2);
    let refusal = await_backend()
        .await
        .expect_err("backend refusal is authoritative");
    assert!(
        refusal.to_string().contains("external attach refused"),
        "{refusal}"
    );
    assert_eq!(port.waits.load(std::sync::atomic::Ordering::SeqCst), 3);
    let unknown = ProcessId::fixture("wait-unknown");
    assert!(
        matches!(work.await_process_terminal(&unknown).await, Err(PluginError::ProcessUnknown { process_id }) if process_id == unknown)
    );
    let external = registry
        .register_process(registration("wait-external"))
        .await
        .expect("external process");
    let output = settled_success(serde_json::json!({"external":true}));
    let completion = async {
        registry
            .complete_process(
                &external.id,
                output.clone(),
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("external completion");
    };
    let (wait, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(work.await_process_terminal(&external.id), completion)
    })
    .await
    .expect("external wait bounded");
    assert_eq!(
        wait.expect("external result"),
        crate::ProcessTerminalWait::Terminal(output)
    );
    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune external");
    assert!(
        matches!(work.await_process_terminal(&external.id).await.expect("pruned information"), crate::ProcessTerminalWait::Terminal(ProcessAwaitOutput::NoLongerRetained { terminal_label, .. }) if terminal_label == "completed")
    );
    let departed = registry
        .register_process(registration("wait-departed"))
        .await
        .expect("departed process");
    registry
        .record_caller_departure(&departed.id)
        .await
        .expect("caller departure");
    assert!(
        matches!(work.await_process_terminal(&departed.id).await, Err(PluginError::ProcessCallerDeparted { process_id }) if process_id == departed.id)
    );
}
