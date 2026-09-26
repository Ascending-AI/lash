use crate::support::prelude::*;
use std::sync::Arc;

use crate::runtime::process::{
    ArtifactOwner, ProcessArtifactCleanup, ProcessArtifactCleanupAck, ProcessAwaitOutput,
    ProcessChange, ProcessChangeCursor, ProcessCompletionAuthority, ProcessEventAppendRequest,
    ProcessEventHistoryRetention, ProcessEventQueryMode, ProcessEventReadOutcome,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessIncarnation, ProcessInput, ProcessObserverBy, ProcessProvenance, ProcessRef,
    ProcessRegistration, ProcessValueSelector, ProcessWakeSpec, ProjectionWatermark,
    RecoveryContract, artifact_owner_is_permanently_retired,
    artifact_staging_owner_edge_is_missing,
};
use crate::{
    OnParentEnd, ParentScope, ProcessId, ProcessLifecyclePolicy, ProcessRegistry, SessionId,
};

use crate::support::{memory_backend, memory_store_set};

async fn memory_registry() -> Arc<dyn ProcessRegistry> {
    memory_store_set().await.process_registry()
}

fn registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        crate::RecoveryContract::ExternallyOwned,
        ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    )
}

/// FIG-3123. The three halves of "an announcement is not a wake", written
/// together because each is only meaningful against the other two: the same
/// process, the same event type, the same declared wake and the same target
/// session — and only the delivery differs.
#[tokio::test]
async fn a_suppressed_append_is_journaled_in_full_and_wakes_nobody() {
    let registry = memory_registry().await;
    let process_id = ProcessId::from("announcement-suppression");
    let target_session_id = SessionId::from("announcing-session");
    registry
        .register_process(wake_registration(process_id.as_str(), &target_session_id))
        .await
        .expect("register a process whose wakes have a target session");

    // (a) The runtime's own announcement of a park. The session it would reach
    // is the session parked on the announced call, so it gets no work.
    let announced = registry
        .append_event(
            &process_id,
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "input request opened"}),
            )
            .with_replay_key("announcement:park")
            .without_wake(),
        )
        .await
        .expect("append the park announcement");
    assert!(
        announced.wake_delivery.is_none(),
        "a park announcement must not deliver a wake: {:?}",
        announced.wake_delivery
    );
    assert!(
        registry
            .claim_pending_wake_deliveries(8)
            .await
            .expect("claim wake deliveries after the announcement")
            .is_empty(),
        "the announcing session must observe no queued work from its own park"
    );

    // (c) ...and yet nothing reading the journal can tell: the event is the
    // same event, semantics included.
    let journal = registry
        .full_event_window(&process_id, 0)
        .await
        .expect("read the process journal");
    let appended = journal
        .iter()
        .find(|event| event.sequence == announced.event.sequence)
        .expect("the announcement is in the journal");
    assert_eq!(appended.event_type, "producer.wake");
    assert_eq!(
        appended.payload,
        serde_json::json!({"wake_input": "input request opened"})
    );
    assert!(
        appended.semantics.wake.is_some(),
        "the event keeps the wake its type declares; only the delivery is withheld: {appended:?}"
    );

    // (b) A progress emission from that same process still says its piece.
    let emitted = registry
        .append_event(
            &process_id,
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "deploy complete"}),
            )
            .with_replay_key("announcement:emit"),
        )
        .await
        .expect("append the progress emission");
    assert!(
        emitted.wake_delivery.is_some(),
        "an unsuppressed append of the same event type still delivers its wake"
    );
    let claimed = registry
        .claim_pending_wake_deliveries(8)
        .await
        .expect("claim wake deliveries after the emission");
    assert_eq!(
        claimed.len(),
        1,
        "exactly the emission reaches the declaring session: {claimed:?}"
    );
}

fn wake_registration(id: &str, target_session_id: &SessionId) -> ProcessRegistration {
    registration(id)
        .with_wake_session_id(Some(SessionId::from(target_session_id.to_string())))
        .with_extra_event_types([ProcessEventType {
            name: "producer.wake".to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: ProcessEventSemanticsSpec {
                wake: Some(ProcessWakeSpec {
                    when: Some(ProcessValueSelector::Present("/wake_input".to_string())),
                    input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                }),
                ..ProcessEventSemanticsSpec::default()
            },
        }])
}

async fn register_successor(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> (ProcessRef, ProcessRef) {
    let old = registry
        .register_process(registration(process_id))
        .await
        .expect("register old incarnation");
    let old_ref = ProcessRef::from_record(&old);
    registry
        .complete_process(
            process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("old"),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete old incarnation");
    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune old incarnation");
    let current = registry
        .register_process(registration(process_id))
        .await
        .expect("register successor incarnation");
    let current_ref = ProcessRef::from_record(&current);
    assert_ne!(old_ref.incarnation, current_ref.incarnation);
    (old_ref, current_ref)
}

#[tokio::test]
async fn prune_retains_exact_artifact_cleanup_until_acknowledged() {
    let registry = memory_registry().await;
    let registration = ProcessRegistration::new(
        "artifact-cleanup-process",
        ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({"module_ref": "module-1"}),
        },
        RecoveryContract::Rerunnable,
        ProcessProvenance::host(),
        ProcessLifecyclePolicy::new(ParentScope::Host, OnParentEnd::Abandon),
    )
    .with_execution_env_ref(Some(ProcessExecutionEnvRef::new("process-env:cleanup")));
    let registered = registry
        .register_process(registration)
        .await
        .expect("register process with exact artifact inputs");
    registry
        .complete_process(
            &registered.id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::workflow_key("artifact-cleanup-process"),
        )
        .await
        .expect("complete process before prune");

    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune process row and persist cleanup evidence atomically");
    let pending = registry
        .pending_process_artifact_cleanup()
        .await
        .expect("read pending artifact cleanup");
    assert_eq!(
        pending,
        vec![ProcessArtifactCleanup::from_record(&registered)],
        "row deletion must leave its exact env and engine release inputs"
    );
    assert_eq!(
        registry
            .compact_process_tombstones(u64::MAX, ProjectionWatermark::NoProjector, None)
            .await
            .expect("compact while cleanup is pending"),
        0,
        "the tombstone is the durable parent of pending cleanup evidence"
    );

    let acknowledgement = registry
        .complete_process_artifact_cleanup(&registered.id, registered.incarnation)
        .await
        .expect("acknowledge artifact cleanup");
    assert_eq!(
        acknowledgement,
        ProcessArtifactCleanupAck::Acknowledged {
            process_ref: ProcessRef::from_record(&registered),
        }
    );
    assert!(
        registry
            .pending_process_artifact_cleanup()
            .await
            .expect("read cleanup after acknowledgement")
            .is_empty()
    );
    assert_eq!(
        registry
            .compact_process_tombstones(u64::MAX, ProjectionWatermark::NoProjector, None)
            .await
            .expect("compact after cleanup acknowledgement"),
        1
    );
}

#[tokio::test]
async fn superseded_event_cursor_is_refused_instead_of_reading_the_successor() {
    let registry = memory_registry().await;
    let (old_ref, _) = register_successor(&registry, &ProcessId::from("reused-event-cursor")).await;

    let result = registry.full_event_window_ref(&old_ref, 0).await;

    assert!(
        matches!(
            result,
            Err(crate::PluginError::ProcessIncarnationSuperseded { .. })
        ),
        "old event cursor must refuse the successor, got {result:?}"
    );
}

/// A page read names one exact process lifetime: a successor incarnation is
/// a typed retired outcome, never another lifetime's events. The projection
/// is a request parameter, not part of any continuation.
#[tokio::test]
async fn event_pages_bind_the_exact_process_lifetime() {
    let registry = memory_registry().await;
    let first_id = ProcessId::from("event-page-lifetime-first");
    let first = registry
        .register_process(registration(&first_id))
        .await
        .expect("register lifetime process");
    let limit = std::num::NonZeroUsize::new(1).expect("non-zero page size");

    for mode in [ProcessEventQueryMode::Full, ProcessEventQueryMode::Lite] {
        let current = registry
            .event_page_ref(&ProcessRef::from_record(&first), 0, limit, mode)
            .await
            .expect("current lifetime page");
        assert!(matches!(current, ProcessEventReadOutcome::Retained(_)));
    }

    let retired = registry
        .event_page_ref(
            &ProcessRef::new(
                first_id.clone(),
                ProcessIncarnation::from_registration_sequence(
                    first.incarnation.registration_sequence() + 1,
                ),
            ),
            0,
            limit,
            ProcessEventQueryMode::Full,
        )
        .await
        .expect("incarnation mismatch is a typed read outcome");
    assert!(
        matches!(
            retired,
            ProcessEventReadOutcome::NoLongerRetained(
                ProcessEventHistoryRetention::Retired {
                    current_incarnation,
                    ..
                }
            ) if current_incarnation == first.incarnation
        ),
        "a read of another incarnation must report retired history, got {retired:?}"
    );
}

#[tokio::test]
async fn superseded_observer_edge_is_refused_instead_of_attaching_to_the_successor() {
    let registry = memory_registry().await;
    let (old_ref, _) =
        register_successor(&registry, &ProcessId::from("reused-observer-edge")).await;

    let result = registry
        .add_observer_ref(
            &SessionId::from("stale-observer"),
            &old_ref,
            ProcessObserverBy::host("stale-edge"),
        )
        .await;

    assert!(
        matches!(
            result,
            Err(crate::PluginError::ProcessIncarnationSuperseded { .. })
        ),
        "old observer edge must refuse the successor, got {result:?}"
    );
}

#[tokio::test]
async fn reuse_retains_the_old_tombstone_beside_the_new_live_incarnation() {
    let registry = memory_registry().await;
    let (old_ref, current_ref) =
        register_successor(&registry, &ProcessId::from("reused-change-feed")).await;

    let (changes, _) = registry
        .processes_changed_since(ProcessChangeCursor::initial(), 100)
        .await
        .expect("read full process change feed");
    assert!(changes.iter().any(|change| matches!(
        change,
        ProcessChange::Deleted { tombstone }
            if tombstone.process_id == old_ref.process_id
                && tombstone.incarnation == old_ref.incarnation
    )));
    assert!(changes.iter().any(|change| matches!(
        change,
        ProcessChange::Upsert { record }
            if record.id == current_ref.process_id
                && record.incarnation == current_ref.incarnation
    )));
}

#[tokio::test]
async fn delete_session_process_command_revokes_only_observer_edges() {
    let backend = memory_backend().await;
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    let registry_dyn = Arc::clone(&registry);
    for process_id in ["sole", "shared"] {
        registry
            .register_process(registration(process_id))
            .await
            .expect("register");
        registry
            .add_observer(
                &SessionId::from("deleted"),
                &ProcessId::from(process_id),
                ProcessObserverBy::host(format!("deleted:{process_id}")),
            )
            .await
            .expect("observe from deleted");
    }
    registry
        .add_observer(
            &SessionId::from("remaining"),
            &ProcessId::from("shared"),
            ProcessObserverBy::host("remaining:shared"),
        )
        .await
        .expect("observe from remaining");
    let sole_events = serde_json::to_vec(
        &registry
            .full_event_window(&ProcessId::from("sole"), 0)
            .await
            .expect("sole events before delete"),
    )
    .expect("serialize sole events");
    let shared_events = serde_json::to_vec(
        &registry
            .full_event_window(&ProcessId::from("shared"), 0)
            .await
            .expect("shared events before delete"),
    )
    .expect("serialize shared events");
    let host = backend.effect_host();
    let scoped = host
        .scoped(crate::AdmittedScope::session_delete("deleted"))
        .expect("admit the session-delete scope");
    let invocation = crate::RuntimeEffectInvocation::new(
        crate::EffectAddress::new(
            crate::ExecutionScope::session_delete("deleted"),
            "deleted:delete-session",
        )
        .expect("valid delete-session address"),
        crate::RuntimeAttribution::for_session("deleted"),
        "process:delete-session:deleted",
    );

    let outcome = crate::RuntimeEffectController::execute_effect(
        scoped.controller(),
        crate::RuntimeEffectEnvelope::new(
            invocation,
            crate::RuntimeEffectCommand::process(crate::ProcessCommand::DeleteSession {
                session_id: SessionId::from("deleted"),
            }),
        ),
        crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry_dyn),
            Arc::new(crate::NativeProcessWork::for_registry(registry_dyn)),
        ),
    )
    .await
    .expect("delete session process command");

    let crate::RuntimeEffectOutcome::Process {
        result: crate::ProcessEffectOutcome::DeleteSession { report },
    } = outcome
    else {
        panic!("unexpected delete session outcome: {outcome:?}");
    };
    assert_eq!(report.removed_observer_count, 2);
    assert_eq!(
        serde_json::to_vec(
            &registry
                .full_event_window(&ProcessId::from("sole"), 0)
                .await
                .expect("sole events")
        )
        .expect("serialize sole events"),
        sole_events
    );
    assert_eq!(
        serde_json::to_vec(
            &registry
                .full_event_window(&ProcessId::from("shared"), 0)
                .await
                .expect("shared events")
        )
        .expect("serialize shared events"),
        shared_events
    );
}

/// The store's refusals carry the typed reasons, so every caller downstream of
/// `ProcessExecutionEnvStore` classifies by code.
#[tokio::test]
async fn env_store_reports_typed_retirement_and_edge_refusals() {
    let backend = memory_store_set().await;
    let store = backend.process_env_store();
    let spec = ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    );
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let staged = ArtifactOwner::process_start(&ProcessId::from("env-typed-staged"));
    let retired_destination =
        ArtifactOwner::process_start(&ProcessId::from("env-typed-destination"));

    store
        .retire_process_execution_env_owner(&retired_destination)
        .await
        .expect("retire destination owner");
    store
        .publish_process_execution_env(&staged, &env_ref, &bytes)
        .await
        .expect("stage env");

    let destination_error = store
        .transfer_process_execution_env(&staged, &retired_destination, &env_ref)
        .await
        .expect_err("a retired destination owner refuses the transfer");
    assert!(
        artifact_owner_is_permanently_retired(&destination_error),
        "destination retirement classifies as owner retirement: {destination_error}"
    );

    let missing_edge = store
        .transfer_process_execution_env(
            &ArtifactOwner::process_start(&ProcessId::from("env-typed-absent")),
            &ArtifactOwner::process_start(&ProcessId::from("env-typed-other")),
            &env_ref,
        )
        .await
        .expect_err("a transfer with neither edge refuses");
    assert!(
        artifact_staging_owner_edge_is_missing(&missing_edge),
        "missing staging edge classifies by code: {missing_edge}"
    );

    store
        .retire_process_execution_env_owner(&staged)
        .await
        .expect("retire staged owner");
    let retired_error = store
        .publish_process_execution_env(&staged, &env_ref, &bytes)
        .await
        .expect_err("a retired staging owner refuses publication");
    assert!(
        artifact_owner_is_permanently_retired(&retired_error),
        "staged-owner retirement classifies by code: {retired_error}"
    );
}
