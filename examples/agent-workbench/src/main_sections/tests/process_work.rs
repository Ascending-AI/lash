use super::tests::run_async_test_on_stack_budget;
use super::*;
use lash::ProcessId;
use lash::SessionId;
use lash::StoreSet as _;

#[test]
fn durable_process_registry_preserves_identity_lifecycle_and_execution_authority() {
    run_async_test_on_stack_budget("workbench-process-registry-lifecycle-test", || {
        durable_process_registry_preserves_identity_lifecycle_and_execution_authority_inner()
    });
}

async fn durable_process_registry_preserves_identity_lifecycle_and_execution_authority_inner() {
    use lash::persistence::ProcessExecutionEnvStore as _;
    use lash::process::{
        CausalRef, ProcessAwaitOutput, ProcessChangeCursor, ProcessCompletionAuthority,
        ProcessEventAppendRequest, ProcessEventType, ProcessExecutionEnvRef,
        ProcessExecutionEnvSpec, ProcessExecutionWriteAuthority, ProcessExternalRef,
        ProcessHandleView, ProcessIdentity, ProcessInput, ProcessListFilter, ProcessListMode,
        ProcessObserverBy, ProcessOriginator, ProcessProvenance, ProcessRegistration,
        ProcessRegistryCursor, ProcessStatus, ProcessStatusFilter, ProjectionWatermark,
        SessionScope,
    };
    let registry_dir = tempfile::tempdir().expect("process registry tempdir");
    let stores = lash::sqlite::SqliteStoreSet::open(registry_dir.path().join("lash.db"))
        .await
        .expect("open a durable registry store set");
    let registry: Arc<dyn lash::process::ProcessRegistry> = stores.process_registry();
    let process_id = "invoice-export";
    let frame_node_id =
        lash::testing::frame_node_id(&SessionId::from("session-finance"), "frame-review");
    let scope = SessionScope::for_agent_frame("session-finance", frame_node_id.clone());
    assert_eq!(
        scope.id().as_str(),
        format!("session:session-finance/frame:{frame_node_id}")
    );
    assert_eq!(scope.session_id, "session-finance");
    assert_eq!(scope.agent_frame_id.as_ref(), Some(&frame_node_id));

    let cause = CausalRef::TriggerOccurrence {
        occurrence_id: "occurrence-42".to_string(),
        subscription_id: Some("subscription-nightly".to_string()),
        subscription_revision: Some(7),
        subscription_incarnation: Some("incarnation-blue".to_string()),
    };
    let provenance = ProcessProvenance::session(scope.clone()).with_caused_by(Some(cause));
    let ProcessOriginator::Session {
        session_id,
        agent_frame_id,
    } = &provenance.originator
    else {
        panic!("session work must retain a session originator");
    };
    assert_eq!(session_id, "session-finance");
    assert_eq!(agent_frame_id.as_ref(), Some(&frame_node_id));
    let Some(CausalRef::TriggerOccurrence {
        occurrence_id,
        subscription_id,
        subscription_revision,
        subscription_incarnation,
    }) = &provenance.caused_by
    else {
        panic!("trigger-started work must retain its occurrence provenance");
    };
    assert_eq!(occurrence_id, "occurrence-42");
    assert_eq!(subscription_id.as_deref(), Some("subscription-nightly"));
    assert_eq!(*subscription_revision, Some(7));
    assert_eq!(
        subscription_incarnation.as_deref(),
        Some("incarnation-blue")
    );

    let input = ProcessInput::Engine {
        kind: "report-export".to_string(),
        payload: json!({ "format": "csv", "rows": 12 }),
    };
    assert_eq!(input.engine_kind(), "engine");
    assert_eq!(input.engine_specific_kind(), Some("report-export"));
    let definition_id = lash::process::ProcessDefinitionId::from_sha256_digest([7; 32]);
    let mut identity = ProcessIdentity::labelled("report-export", Some("Nightly invoice export"));
    identity.definition_id = Some(definition_id.clone());
    assert_eq!(identity.kind, "report-export");
    assert_eq!(identity.label.as_deref(), Some("Nightly invoice export"));
    assert_eq!(identity.definition_id.as_ref(), Some(&definition_id));
    let execution_env = ProcessExecutionEnvSpec::new(
        Default::default(),
        lash::runtime::SessionPolicy::new(
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        ),
    );
    let execution_env_ref = execution_env
        .stable_ref()
        .expect("derive process execution environment identity");
    let pin = lash::persistence::ReferrerClaim::unguarded(
        lash::persistence::ArtifactReferrer::HostPin(lash::process::HostArtifactPin::mint()),
    )
    .expect("fixture host pin");
    stores
        .process_env_store()
        .publish_process_execution_env(
            &pin,
            &execution_env_ref,
            &execution_env
                .to_store_bytes()
                .expect("encode fixture environment"),
        )
        .await
        .expect("publish fixture environment");
    let execution_env_digest = execution_env_ref
        .as_str()
        .strip_prefix("process-env:v6:blake3:")
        .expect("process execution environment uses the v5 identity family");
    assert_eq!(execution_env_digest.len(), 64);
    assert!(
        execution_env_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );

    let start_key = lash::process::StartKey::for_host(process_id);
    let registration = ProcessRegistration::new(
        input,
        ProcessProvenance::host(),
        lash::process::Lifetime::Detached,
    )
    .with_start_key(Some(start_key.clone()))
    .with_process_provenance(provenance)
    .with_admitted_identity(lash::process::AdmittedProcessIdentity::for_testing(
        identity,
    ))
    .with_execution_env_ref(Some(execution_env_ref.clone()))
    .with_extra_event_types([ProcessEventType {
        name: "progress".to_string(),
        payload_schema: lash::schema::JsonSchema::any(),
        semantics: Default::default(),
    }])
    .with_wake_session_id(Some(SessionId::from("session-finance")));
    assert_eq!(registration.start_key.as_ref(), Some(&start_key));
    assert_eq!(
        registration
            .env_ref
            .as_ref()
            .map(ProcessExecutionEnvRef::as_str),
        Some(execution_env_ref.as_str())
    );
    assert_eq!(
        registration.wake_session_id.as_deref(),
        Some("session-finance")
    );
    assert_eq!(
        registration.input.engine_specific_kind(),
        Some("report-export")
    );
    assert!(
        registration.event_types.iter().any(|event| {
            event.name == "process.completed" && event.semantics.terminal.is_some()
        })
    );

    let initial_cursor = ProcessChangeCursor::initial();
    assert_eq!(initial_cursor.store_sequence(), 0);
    let stored_cursor = ProcessChangeCursor::from_store_sequence(9);
    assert_eq!(stored_cursor.store_sequence(), 9);
    let replay_registration = registration.clone();
    let initial_observers = [
        SessionId::from("session-finance"),
        SessionId::from("session-ops"),
    ];
    let record = registry
        .register_process_with_observers(registration, &initial_observers)
        .await
        .expect("register process and initial observers");
    let process_id = record.id.clone();
    assert_eq!(record.start_key.as_ref(), Some(&start_key));
    assert_eq!(record.status(), ProcessStatus::Running);
    assert!(!record.is_terminal());
    assert_eq!(record.originator_id(), "session-finance");
    assert_eq!(
        record.identity.label.as_deref(),
        Some("Nightly invoice export")
    );
    assert_eq!(record.input.engine_specific_kind(), Some("report-export"));
    assert_eq!(
        record.provenance.originator,
        ProcessOriginator::Session {
            session_id: SessionId::from("session-finance"),
            agent_frame_id: Some(frame_node_id.clone()),
        }
    );
    assert_eq!(
        record.env_ref.as_ref().map(ProcessExecutionEnvRef::as_str),
        Some(execution_env_ref.as_str())
    );
    assert!(
        record
            .event_types
            .iter()
            .any(|event| event.name == "progress")
    );
    assert!(record.updated_at_ms >= record.created_at_ms);
    assert!(record.external_ref.is_none());
    assert!(record.first_started.is_none());
    assert!(record.wait().is_none());
    assert!(record.outcome().is_none());
    let replay = registry
        .register_process_with_observers(
            replay_registration,
            &[
                SessionId::from("session-ops"),
                SessionId::from("session-finance"),
            ],
        )
        .await
        .expect("replay process registration under its start key");
    assert_eq!(
        replay.id, process_id,
        "a retained start key answers the process it minted"
    );

    let observers = registry
        .observers_for_process(&process_id)
        .await
        .expect("list initial observers");
    assert_eq!(observers, ["session-finance", "session-ops"]);
    assert!(
        registry
            .is_observer(&SessionId::from("session-finance"), &process_id)
            .await
            .expect("read observer edge")
    );
    registry
        .transfer_observers(
            &SessionId::from("session-ops"),
            &SessionId::from("session-audit"),
            std::slice::from_ref(&process_id),
            ProcessObserverBy::host("audit-handoff"),
        )
        .await
        .expect("transfer observer");
    assert_eq!(
        registry
            .observers_for_process(&process_id)
            .await
            .expect("list transferred observers"),
        ["session-audit", "session-finance"]
    );
    registry
        .remove_observer(
            &SessionId::from("session-audit"),
            &process_id,
            ProcessObserverBy::host("observer-test"),
        )
        .await
        .expect("remove inherited observer");
    let inherited_observer = ProcessObserverBy::host("observer-test");
    assert_eq!(inherited_observer.replay_component(), "observer-test");

    let external_ref = ProcessExternalRef {
        backend: "host-queue".to_string(),
        id: "invocation-778".to_string(),
        metadata: Some(json!({ "region": "eu-central-1" })),
        segment_ordinal: None,
    };
    let record = registry
        .set_external_ref(&process_id, external_ref)
        .await
        .expect("bind backend work");
    assert_eq!(record.external_ref.as_ref().unwrap().backend, "host-queue");
    assert_eq!(record.external_ref.as_ref().unwrap().id, "invocation-778");
    assert_eq!(
        record
            .external_ref
            .as_ref()
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()["region"],
        "eu-central-1"
    );

    let progress = ProcessEventAppendRequest::new(
        "progress",
        json!({ "completed_rows": 8, "total_rows": 12 }),
    )
    .with_replay_key("invoice-export:progress:8");
    assert_eq!(progress.event_type, "progress");
    assert_eq!(progress.payload["completed_rows"], 8);
    assert_eq!(
        progress.replay.as_ref().unwrap().key,
        "invoice-export:progress:8"
    );
    let first_progress = registry
        .append_event(&process_id, progress.clone())
        .await
        .expect("append progress");
    let replayed_progress = registry
        .append_event(
            &process_id,
            progress.with_optional_replay(first_progress.event.invocation.replay.clone()),
        )
        .await
        .expect("replay progress append");
    assert_eq!(
        replayed_progress.event.sequence,
        first_progress.event.sequence
    );
    assert_eq!(replayed_progress.event.process_id, process_id);
    assert_eq!(replayed_progress.event.event_type, "progress");
    assert_eq!(replayed_progress.event.payload["total_rows"], 12);
    assert!(replayed_progress.wake_delivery.is_none());
    assert_eq!(
        registry
            .count_events_through(&process_id, "progress", first_progress.event.sequence)
            .await
            .expect("count progress events"),
        1
    );
    assert_eq!(
        registry
            .recent_events(&process_id, 1)
            .await
            .expect("read event tail")[0]
            .event_type,
        "progress"
    );

    let execution_authority =
        ProcessExecutionWriteAuthority::invocation(process_id.clone(), "worker-berlin:boot-9")
            .bind_attempt(1);
    let started = execution_authority
        .invocation_started()
        .expect("bound invocation has a started fact");
    let start = registry
        .record_first_started_with_authority(&process_id, started.clone(), &execution_authority)
        .await
        .expect("record invocation start");
    assert!(matches!(
        start,
        lash::process::ProcessStartOutcome::Started(_)
    ));
    assert!(started.same_execution(&execution_authority.invocation_started().unwrap()));
    let successor_attempt =
        ProcessExecutionWriteAuthority::invocation(process_id.clone(), "worker-berlin:boot-9")
            .bind_attempt(2)
            .invocation_started()
            .expect("successor invocation has a started fact");
    assert!(!started.same_execution(&successor_attempt));

    let running = registry
        .get_process(&process_id)
        .await
        .expect("read running process")
        .expect("registered process remains visible");
    assert_eq!(running.status(), ProcessStatus::Running);

    let filter = ProcessListFilter::decode(&json!({
        "status": {"in":["running"]},
        "originator": {"type": "session", "session_id": "session-finance"},
        "identity_kind": "report-export",
        "identity_label": "Nightly invoice export",
        "caused_by_occurrence_id": "occurrence-42",
        "caused_by_subscription_id": "subscription-nightly",
        "created_at_start_ms": record.created_at_ms,
        "created_at_end_ms": record.created_at_ms.saturating_add(1),
    }))
    .expect("decode process filters");
    assert_eq!(
        filter.status,
        ProcessStatusFilter::any_of([ProcessStatus::Running])
    );
    assert_eq!(filter.status.labels(), Some(vec!["running"]));
    assert_eq!(ProcessStatus::Failed.label(), "failed");
    assert!(ProcessStatus::Failed.is_terminal());
    assert_eq!(filter.list_mode(), ProcessListMode::Live);
    assert_eq!(filter.list_mode().as_str(), "live");
    assert!(filter.matches_record(&running));
    assert_eq!(
        registry
            .list_processes(&filter)
            .await
            .expect("filter live process")
            .len(),
        1
    );
    assert!(ProcessStatusFilter::Any.matches(ProcessStatus::Running));
    assert_eq!(ProcessStatusFilter::Any.list_mode(), ProcessListMode::All);
    assert_eq!(
        ProcessStatusFilter::decode(Some(&json!({"in":["completed"]}))),
        Ok(ProcessStatusFilter::any_of([ProcessStatus::Completed]))
    );

    let live_refs = registry
        .live_reference_summary()
        .await
        .expect("summarize live references");
    assert_eq!(live_refs.len(), 1);
    assert_eq!(live_refs[0].process_count, 1);
    assert_eq!(live_refs[0].definition_id.as_ref(), Some(&definition_id));
    assert_eq!(
        live_refs[0]
            .env_ref
            .as_ref()
            .map(ProcessExecutionEnvRef::as_str),
        Some(execution_env_ref.as_str())
    );
    assert_eq!(
        registry
            .filter_unregistered_process_ids(&[
                process_id.clone(),
                ProcessId::fixture("never-registered"),
            ])
            .await
            .expect("filter recovery candidates"),
        [ProcessId::fixture("never-registered")]
    );

    let success = ProcessAwaitOutput::from_tool_output(lash::tools::ToolCallOutput::success(
        json!({ "artifact": "invoices.csv", "rows": 12 }),
    ));
    assert_eq!(
        success.terminal_status(),
        Some(lash::process::TerminalProcessStatus::Completed)
    );
    assert_eq!(
        success.clone().into_tool_output().value_for_projection()["artifact"],
        "invoices.csv"
    );
    let completion = registry
        .complete_process(
            &process_id,
            success.clone(),
            ProcessCompletionAuthority::workflow_key(process_id.as_str()),
        )
        .await
        .expect("complete process under workflow-key authority");
    let completed = match completion {
        lash::process::ProcessCompletionOutcome::Committed(record) => record,
        other => panic!("first completion was not committed: {other:?}"),
    };
    assert_eq!(completed.status(), ProcessStatus::Completed);
    assert!(completed.is_terminal());
    assert_eq!(completed.outcome().as_ref(), Some(&success));

    let replay = registry
        .complete_process(
            &process_id,
            success.clone(),
            ProcessCompletionAuthority::workflow_key(process_id.as_str()),
        )
        .await
        .expect("replay terminal completion");
    let replay = match replay {
        lash::process::ProcessCompletionOutcome::AlreadyApplied { stored }
        | lash::process::ProcessCompletionOutcome::Superseded { stored } => stored,
        other => panic!("replayed completion was not settled: {other:?}"),
    };
    assert_eq!(replay.status(), ProcessStatus::Completed);
    assert_eq!(replay.outcome().as_ref(), Some(&success));
    let cancellation =
        ProcessAwaitOutput::from_tool_output(lash::tools::ToolCallOutput::cancelled(
            lash::tools::ToolCancellation::runtime("operator cancelled"),
        ));
    assert_eq!(
        cancellation.terminal_status(),
        Some(lash::process::TerminalProcessStatus::Cancelled)
    );
    assert_eq!(
        cancellation.into_tool_output().value_for_projection()["message"],
        "operator cancelled"
    );

    let handle = ProcessHandleView::from_record(completed.clone())
        .with_definition_id(Some(definition_id.clone()));
    // The handle id is opaque: it is the value to carry and hand back, not the
    // process id. Before ADR 0095 it was a copy of `process_id` and a separate
    // `__handle__` string said what kind of handle it was.
    assert_eq!(handle.process_id, process_id);
    assert_ne!(handle.id.as_str(), process_id.as_str());
    assert_eq!(handle.id, lash::process::HandleId::process(&process_id));
    assert_eq!(handle.kind, "report-export");
    assert_eq!(handle.label.as_deref(), Some("Nightly invoice export"));
    assert_eq!(handle.definition_id.as_ref(), Some(&definition_id));
    assert_eq!(handle.status, ProcessStatus::Completed);
    assert!(
        lash::process::ProcessCancelReceipt::from_record(completed.clone()).is_err(),
        "a completed process without an accepted cancel request has no cancel receipt"
    );

    let registry_cursor = ProcessRegistryCursor::new(
        "example",
        ProcessId::fixture("invoice-a"),
        ProcessId::fixture("invoice-z"),
    );
    assert_eq!(registry_cursor.backend(), "example");
    assert_eq!(
        registry_cursor.after_process_id(),
        &ProcessId::fixture("invoice-a")
    );
    assert_eq!(
        registry_cursor.through_process_id(),
        &ProcessId::fixture("invoice-z")
    );
    let non_terminal_page = registry
        .list_non_terminal_processes_page(
            std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
            None,
        )
        .await
        .expect("list recovery work");
    assert!(non_terminal_page.records.is_empty());
    assert!(non_terminal_page.continuation.is_none());
    assert_eq!(
        registry
            .list_processes(&ProcessListFilter {
                status: ProcessStatusFilter::any_of([ProcessStatus::Completed]),
                ..Default::default()
            })
            .await
            .expect("list completed process")
            .len(),
        1
    );

    let external_id = registry
        .register_process(
            lash::testing::held_engine_registration(
                json!({ "backend": "batch-service" }),
                ProcessProvenance::new(ProcessOriginator::host_scoped("batch-service")),
                lash::process::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(execution_env_ref.clone())),
        )
        .await
        .expect("register held work")
        .id;
    let mut batch_failure = lash::tools::ToolFailure::tool(
        lash::tools::ToolFailureClass::External,
        "batch_rejected",
        "batch service rejected the export",
    );
    batch_failure.raw = Some(lash::tools::ToolValue::untrusted_json(
        json!({ "retryable": false }),
    ));
    let external_completion = registry
        .complete_process(
            &external_id,
            ProcessAwaitOutput::from_tool_output(lash::tools::ToolCallOutput::failure(
                batch_failure,
            )),
            ProcessCompletionAuthority::workflow_key(&external_id),
        )
        .await
        .expect("the workflow key closes the work");
    assert_eq!(external_completion.status(), ProcessStatus::Failed);
    assert!(matches!(
        external_completion.outcome().as_ref(),
        Some(ProcessAwaitOutput::Settled { output })
            if !output.is_success()
                && output.value_for_projection()["class"] == "external"
                && output.value_for_projection()["code"] == "batch_rejected"
                && output.value_for_projection()["message"]
                    == "batch service rejected the export"
                && output.value_for_projection()["raw"]["retryable"] == false
    ));
    assert_eq!(
        ProcessCompletionAuthority::workflow_key("wf-1").label(),
        "workflow-key"
    );

    let report = registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune projected terminal processes");
    assert_eq!(report.pruned_processes, 2);
    assert!(report.pruned_events >= 2);
    assert_eq!(report.pruned_trigger_deliveries, 0);
    assert_eq!(
        registry
            .filter_tombstoned_process_ids(&[
                process_id.clone(),
                external_id.clone(),
                ProcessId::fixture("never-registered"),
            ])
            .await
            .expect("filter pruned process ids"),
        [process_id.clone(), external_id.clone()]
    );
    let compacted = registry
        .compact_process_tombstones(u64::MAX, ProjectionWatermark::UpTo(initial_cursor), None)
        .await
        .expect("compact projected tombstones");
    assert_eq!(
        compacted, 0,
        "unprojected deletions must retain their tombstones"
    );
    let cleanup_ledger = stores.artifact_cleanup();
    let pending_cleanup = cleanup_ledger
        .claim_due(
            i64::MAX as u64 / 2,
            1_000,
            std::num::NonZeroUsize::new(10).expect("nonzero page size"),
        )
        .await
        .expect("claim retained process artifact cleanup");
    let mut cleanup_process_ids = Vec::new();
    for claim in &pending_cleanup {
        let cleanup = cleanup_ledger
            .load_cleanup(&claim.id)
            .await
            .expect("read claimed cleanup")
            .expect("cleanup exists");
        assert_eq!(cleanup.referrer().kind().to_string(), "process_record");
        cleanup_process_ids.push(cleanup.referrer().canonical_id());
    }
    cleanup_process_ids.sort();
    assert_eq!(
        cleanup_process_ids,
        {
            let mut pruned = vec![process_id.to_string(), external_id.to_string()];
            pruned.sort();
            pruned
        },
        "each pruned process owes a referrer cleanup"
    );
    for claim in pending_cleanup {
        let acknowledgement = cleanup_ledger
            .settle(
                &claim.id,
                &claim.token,
                lash::persistence::ObligationSettlement::Delivered,
                i64::MAX as u64 / 2,
            )
            .await
            .expect("acknowledge process artifact cleanup");
        assert_eq!(acknowledgement, lash::persistence::SettleOutcome::Applied);
    }
    assert_eq!(
        registry
            .compact_process_tombstones(u64::MAX, ProjectionWatermark::NoProjector, None,)
            .await
            .expect("compact tombstones without a projector"),
        2
    );
}

use super::tests::{RecordingTrace, Workbench, silent_provider};

/// Register a held process `provenance` originated in `workbench`'s registry,
/// labelled `label` for the work rail.
async fn register_held(
    workbench: &Workbench,
    provenance: lash::process::ProcessProvenance,
    label: &str,
) -> ProcessId {
    workbench
        .stores
        .process_registry()
        .register_process(
            lash::testing::held_engine_registration(
                json!({ "label": label }),
                provenance,
                lash::process::Lifetime::Detached,
            )
            .with_host_facing_label(Some(label.to_string()))
            .with_extra_event_types([lash::process::ProcessEventType {
                name: "progress".to_string(),
                payload_schema: lash::schema::JsonSchema::any(),
                semantics: Default::default(),
            }]),
        )
        .await
        .expect("register process")
        .id
}

async fn complete(
    workbench: &Workbench,
    process_id: &ProcessId,
    output: lash::tools::ToolCallOutput,
) {
    workbench
        .stores
        .process_registry()
        .complete_process(
            process_id,
            lash::process::ProcessAwaitOutput::from_tool_output(output),
            lash::process::ProcessCompletionAuthority::workflow_key(process_id),
        )
        .await
        .expect("complete process");
}

/// The labels of the rows on the runtime-wide work rail, sorted.
async fn work_rail_labels(state: &AppState) -> Vec<String> {
    let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list runtime-wide work");
    let mut labels = work
        .into_iter()
        .map(|item| item.process.label)
        .collect::<Vec<_>>();
    labels.sort();
    labels
}

/// The await route answers a process's terminal outcome through the
/// process-work port (ADR 0016) with its event log reconciled from the
/// durable store (ADR 0017); a failed process shows failed on the rail; an
/// unknown id errors instead of hanging.
#[tokio::test]
async fn await_work_route_returns_terminal_outcome_and_reconciled_events() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let awaited = register_held(
        &workbench,
        lash::process::ProcessProvenance::host(),
        "awaited",
    )
    .await;
    workbench
        .stores
        .process_registry()
        .append_event(
            &awaited,
            lash::process::ProcessEventAppendRequest::new("progress", json!({ "step": 1 })),
        )
        .await
        .expect("append progress event");
    complete(
        &workbench,
        &awaited,
        lash::tools::ToolCallOutput::success(json!("done")),
    )
    .await;

    let Json(result) = await_work(AxumPath(awaited.to_string()), State(state.clone()))
        .await
        .expect("await work route");
    assert_eq!(
        result.outcome.terminal_status(),
        Some(lash::process::TerminalProcessStatus::Completed)
    );
    assert!(matches!(
        &result.outcome,
        lash::process::ProcessAwaitOutput::Settled { output }
            if output.is_success() && output.value_for_projection() == json!("done")
    ));
    assert!(
        result
            .events
            .iter()
            .any(|event| event.event_type == "progress"),
        "reconciled events missing the appended progress event: {:?}",
        result.events
    );

    let failed = register_held(
        &workbench,
        lash::process::ProcessProvenance::host(),
        "failed",
    )
    .await;
    complete(
        &workbench,
        &failed,
        lash::tools::ToolCallOutput::failure(lash::tools::ToolFailure::runtime(
            lash::tools::ToolFailureClass::External,
            "deterministic_failure",
            "deterministic durable process failure",
        )),
    )
    .await;
    let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list failed work");
    let failed = work
        .iter()
        .find(|item| item.process.process_id == failed)
        .expect("failed process in work API");
    assert_eq!(failed.process.status_label, "failed");
    assert!(failed.process.terminal);
    assert_eq!(
        failed.process.error.as_deref(),
        Some("deterministic durable process failure")
    );

    let missing = await_work(
        AxumPath("no-such-process".to_string()),
        State(state.clone()),
    )
    .await;
    assert!(missing.is_err(), "unknown process id must error");
    workbench.shutdown().await;
}

/// A process whose session's process state was deleted stays on the
/// runtime-wide rail, and its cancel is routed by the process, not by the
/// selected session: the request lands on the process and names the session
/// that originated it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_api_keeps_orphaned_process_visible_and_routes_cancel_globally() {
    let trace = Arc::new(RecordingTrace::default());
    let workbench = Workbench::builder(silent_provider())
        .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let registry = workbench.stores.process_registry();
    let process_id = register_held(
        &workbench,
        lash::process::ProcessProvenance::session(lash::process::SessionScope::new(&session_id)),
        "orphaned",
    )
    .await;
    registry
        .add_observer(
            &session_id,
            &process_id,
            lash::process::ProcessObserverBy::host("workbench-session-delete"),
        )
        .await
        .expect("observe process");
    let deletion = registry
        .delete_session_process_state(&session_id)
        .await
        .expect("delete session process edges");
    assert_eq!(deletion.removed_observer_count, 1);
    let (_, current_session_id) = state.sessions.rotate();
    assert_ne!(current_session_id, session_id);

    let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list runtime-wide work");
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].process.process_id, process_id);

    let Json(receipt) = cancel_work(AxumPath(process_id.to_string()), State(state.clone()))
        .await
        .expect("submit process cancellation");
    assert!(receipt.accepted);
    assert_eq!(receipt.process_id, process_id);
    let requested = trace.custom("process.cancel_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(
        requested[0].0.as_ref(),
        Some(&session_id),
        "process cancellation must retain its originating trace session"
    );
    assert_eq!(
        requested[0].1["process_id"].as_str(),
        Some(process_id.as_str())
    );
    let cancelled = state
        .process_observer
        .process(&process_id)
        .await
        .expect("read the cancelled process")
        .expect("the process is still observable");
    assert!(
        cancelled.cancel_request.is_some() || cancelled.terminal(),
        "the cancel landed on the process: {cancelled:?}"
    );
    workbench.shutdown().await;
}

/// The work rail reads the runtime-wide process registry, and deleting a
/// session deliberately detaches rather than deletes the globally-owned rows
/// it originated. Without the retention half of the delete, every reset left
/// the dead session's finished work — trigger deliveries above all — on the
/// rail forever (FIG-989).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_delete_reclaims_the_deleted_sessions_terminal_work() {
    use lash::process::{ProcessProvenance, SessionScope};
    let trace = Arc::new(RecordingTrace::default());
    let workbench = Workbench::builder(silent_provider())
        .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
        .build()
        .await;
    let state = &workbench.state;
    let deleted_session_id = state.current_session_id();
    let surviving_session_id = SessionId::fixture(format!("{deleted_session_id}-survivor"));
    let mut ids = BTreeMap::new();
    for (label, originator) in [
        (
            "trigger-delivery-of-deleted-session",
            Some(deleted_session_id.clone()),
        ),
        (
            "live-work-of-deleted-session",
            Some(deleted_session_id.clone()),
        ),
        (
            "trigger-delivery-of-surviving-session",
            Some(surviving_session_id.clone()),
        ),
        ("host-owned-work", None),
    ] {
        let provenance = match &originator {
            Some(session_id) => ProcessProvenance::session(SessionScope::new(session_id)),
            None => ProcessProvenance::host(),
        };
        ids.insert(label, register_held(&workbench, provenance, label).await);
    }
    for label in [
        "trigger-delivery-of-deleted-session",
        "trigger-delivery-of-surviving-session",
        "host-owned-work",
    ] {
        complete(
            &workbench,
            &ids[label],
            lash::tools::ToolCallOutput::success(json!({ "delivered": true })),
        )
        .await;
    }
    assert_eq!(
        work_rail_labels(state).await,
        vec![
            "host-owned-work".to_string(),
            "live-work-of-deleted-session".to_string(),
            "trigger-delivery-of-deleted-session".to_string(),
            "trigger-delivery-of-surviving-session".to_string(),
        ],
        "every registered row is on the runtime-wide rail before the delete"
    );

    let Json(replacement) = Box::pin(reset_chat(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(deleted_session_id.clone()),
        }),
    ))
    .await
    .expect("delete the session and reclaim its finished work");
    assert_ne!(replacement.settings.session_id, deleted_session_id);

    let deleted = trace.custom("api.session.delete.deleted");
    assert_eq!(deleted.len(), 1, "{deleted:?}");
    assert_eq!(
        deleted[0].1["process_retention"]["pruned_processes"],
        json!(1),
        "one finished row reclaimed: {deleted:?}"
    );
    let rail = work_rail_labels(state).await;
    assert_eq!(
        rail,
        vec![
            "host-owned-work".to_string(),
            "live-work-of-deleted-session".to_string(),
            "trigger-delivery-of-surviving-session".to_string(),
        ],
        "live work and other owners' finished work stay on the rail"
    );
    assert!(
        matches!(
            workbench
                .stores
                .process_registry()
                .get_process(&ids["trigger-delivery-of-deleted-session"])
                .await,
            Err(lash::plugins::PluginError::ProcessNoLongerRetained { .. })
        ),
        "the reclaimed row must read as a payload-free tombstone"
    );
    workbench.shutdown().await;
}

/// FIG-3155: the runtime-wide rail bounds retired rows by a recent-update
/// window. A row leaves the rail when its outcome is recorded, never because
/// time passed, so a running process older than the window stays on it.
#[tokio::test]
async fn work_rail_keeps_a_nonterminal_process_past_the_retirement_window() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    // Every write below is stamped a full minute before the rail's window, so
    // the window alone decides visibility.
    let stale_ms = lash::runtime::ClockWallTime::timestamp_ms(&lash::runtime::SystemClock)
        .saturating_sub(60_000);
    let stale_registry = workbench
        .stores
        .process_registry()
        .with_runtime_clock(Arc::new(lash::testing::TestClock::new(stale_ms)))
        .expect("the sqlite registry rebinds its clock");
    let mut ids = BTreeMap::new();
    for label in ["running-process", "settled-process"] {
        let process_id = stale_registry
            .register_process(lash::testing::held_engine_registration(
                json!({ "test": true }),
                lash::process::ProcessProvenance::session(lash::process::SessionScope::new(
                    &session_id,
                )),
                lash::process::Lifetime::Detached,
            ))
            .await
            .expect("register process")
            .id;
        ids.insert(label, process_id);
    }
    stale_registry
        .complete_process(
            &ids["settled-process"],
            lash::process::ProcessAwaitOutput::from_tool_output(
                lash::tools::ToolCallOutput::success(json!("done")),
            ),
            lash::process::ProcessCompletionAuthority::workflow_key(&ids["settled-process"]),
        )
        .await
        .expect("record the terminal outcome");

    let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
        .await
        .expect("list runtime-wide work");
    let listed = work
        .iter()
        .map(|item| item.process.process_id.clone())
        .collect::<Vec<_>>();
    assert!(
        listed.contains(&ids["running-process"]),
        "a non-terminal process must stay on the rail until its terminal lands: {listed:?}"
    );
    assert!(
        !listed.contains(&ids["settled-process"]),
        "a terminal older than the retirement window still leaves the rail: {listed:?}"
    );
    workbench.shutdown().await;
}
