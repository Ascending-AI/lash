//! Cross-backend conformance for process retention's trigger-store effects.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use crate::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessIdentity, ProcessInput,
    ProcessOriginator, ProcessProvenance, ProcessRegistration, ProcessRegistry,
    ProjectionWatermark, RecoveryContract, SessionScope, TriggerCommand, TriggerCommandOutcome,
    TriggerOwnerScope, TriggerStore, TriggerSubscriptionDraft,
};

/// Fresh paired process and trigger stores for retention conformance.
pub struct ProcessTriggerRetentionHandles {
    pub registry: Arc<dyn ProcessRegistry>,
    pub triggers: Arc<dyn TriggerStore>,
    pub sessions: Arc<dyn crate::SessionStoreFactory>,
}

/// Run the process/trigger retention laws against one backend.
pub async fn process_trigger_retention<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ProcessTriggerRetentionHandles>,
{
    let first = make().await;
    let second = make().await;
    assert!(
        !Arc::ptr_eq(&first.registry, &second.registry),
        "process_trigger_retention reused one process-registry Arc"
    );
    assert!(
        !Arc::ptr_eq(&first.triggers, &second.triggers),
        "process_trigger_retention reused one trigger-store Arc"
    );
    drop((first, second));
    deleted_session_frontier_authorizes_trigger_owner_reclamation(make().await).await;
    process_prune_preserves_trigger_mutation_receipts(make().await).await;
    zero_match_occurrence_is_reclaimed_at_delivery_reconciliation(make().await).await;
    delivery_delete_is_bound_to_observed_row_identity(make().await).await;
    process_prune_only_deletes_deliveries_for_pruned_processes(make().await).await;
    pruned_delivery_process_is_not_a_recovery_candidate(make().await).await;
    reregistered_between_classification_and_delete_preserves_delivery(make().await).await;
    outstanding_delivery_blocks_interleaved_tombstone_compaction(make().await).await;
}

async fn deleted_session_frontier_authorizes_trigger_owner_reclamation(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "frontier-trigger-retention-session";
    const RECEIPT_ONLY_SESSION: &str = "frontier-trigger-receipt-only-session";
    const KEY: &str = "frontier-trigger-retention-key";
    const REGISTER_OPERATION: &str = "frontier-trigger-retention-register";
    let request = super::session_store_factory::session_store_request(
        SESSION,
        "frontier-trigger-retention-model",
        crate::SessionRelation::Root,
    );
    handles
        .sessions
        .create_store(&request)
        .await
        .expect("materialize trigger owner session");
    let original_draft = draft(SESSION, KEY, "frontier-trigger-retention-source");
    let created = handles
        .triggers
        .execute_command(
            REGISTER_OPERATION,
            TriggerCommand::Register {
                owner_scope: owner(SESSION),
                actor: actor(SESSION),
                draft: original_draft.clone(),
            },
        )
        .await
        .expect("register frontier-owned trigger")
        .expect("frontier-owned registration succeeds");
    let TriggerCommandOutcome::Mutation { receipt: created } = created else {
        panic!("register must return a mutation receipt")
    };
    handles
        .triggers
        .execute_command(
            "frontier-trigger-retention-delete",
            TriggerCommand::Delete {
                owner_scope: owner(SESSION),
                actor: actor(SESSION),
                subscription_key: KEY.to_string(),
                expected_revision: created.revision,
            },
        )
        .await
        .expect("delete frontier-owned trigger")
        .expect("frontier-owned delete succeeds");
    handles
        .sessions
        .delete_session(SESSION)
        .await
        .expect("delete trigger owner session");
    let receipt_only_request = super::session_store_factory::session_store_request(
        RECEIPT_ONLY_SESSION,
        "frontier-trigger-receipt-only-model",
        crate::SessionRelation::Root,
    );
    handles
        .sessions
        .create_store(&receipt_only_request)
        .await
        .expect("materialize receipt-only trigger owner session");
    handles
        .triggers
        .execute_command(
            "frontier-trigger-receipt-only-operation",
            TriggerCommand::Prune {
                owner_scope: owner(RECEIPT_ONLY_SESSION),
                actor: actor(RECEIPT_ONLY_SESSION),
                subscription_keys: Vec::new(),
            },
        )
        .await
        .expect("journal receipt-only trigger command")
        .expect("receipt-only trigger command succeeds");
    handles
        .sessions
        .delete_session(RECEIPT_ONLY_SESSION)
        .await
        .expect("delete receipt-only trigger owner session");

    let report = crate::reconcile_pruned_trigger_deliveries(
        handles.registry.as_ref(),
        handles.triggers.as_ref(),
        Some(handles.sessions.as_ref()),
    )
    .await
    .expect("reconcile deleted trigger owner");
    assert_eq!(report.reclaimed_subscription_count, 1);
    assert_eq!(report.reclaimed_mutation_receipt_count, 3);

    let mut replacement = original_draft;
    replacement.source_key = "frontier-trigger-retention-replacement".to_string();
    let recreated = handles
        .triggers
        .execute_command(
            REGISTER_OPERATION,
            TriggerCommand::Register {
                owner_scope: owner(SESSION),
                actor: actor(SESSION),
                draft: replacement,
            },
        )
        .await
        .expect("reuse operation id after dead-owner receipt reclamation")
        .expect("recreate trigger after dead-owner fence reclamation");
    let TriggerCommandOutcome::Mutation { receipt: recreated } = recreated else {
        panic!("recreate must return a mutation receipt")
    };
    assert_eq!(recreated.revision, 1);
    handles
        .triggers
        .execute_command(
            "frontier-trigger-receipt-only-operation",
            TriggerCommand::Prune {
                owner_scope: owner(RECEIPT_ONLY_SESSION),
                actor: actor(RECEIPT_ONLY_SESSION),
                subscription_keys: vec!["different-content-after-reclaim".to_string()],
            },
        )
        .await
        .expect("reuse receipt-only operation id after owner cascade")
        .expect("receipt-only operation is re-evaluated after owner cascade");
}

async fn zero_match_occurrence_is_reclaimed_at_delivery_reconciliation(
    handles: ProcessTriggerRetentionHandles,
) {
    let ingress = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "zero-match-reconciliation-source",
            serde_json::json!({ "button": "Blue" }),
            "zero-match-reconciliation-occurrence",
        ))
        .await
        .expect("ingest zero-match occurrence");
    assert!(ingress.reservations.is_empty());

    crate::reconcile_pruned_trigger_deliveries(
        handles.registry.as_ref(),
        handles.triggers.as_ref(),
        Some(handles.sessions.as_ref()),
    )
    .await
    .expect("reconcile zero-match occurrence");

    assert!(
        handles
            .triggers
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("list occurrences after reconciliation")
            .is_empty(),
        "a committed zero-match fan-out must be reclaimed at delivery reconciliation"
    );
}

async fn delivery_delete_is_bound_to_observed_row_identity(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "delivery-retention-identity-session";
    register_trigger(
        &handles.triggers,
        SESSION,
        "delivery-retention-identity-key",
        "delivery-retention-identity-source",
        "delivery-retention-identity-register",
    )
    .await;
    for occurrence in ["first", "second"] {
        let ingress = handles
            .triggers
            .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                "delivery-retention-identity-source",
                serde_json::json!({ "button": "Blue" }),
                format!("delivery-retention-identity-{occurrence}"),
            ))
            .await
            .expect("ingest identity-law occurrence");
        assert_eq!(ingress.reservations.len(), 1);
    }
    let candidates = handles
        .triggers
        .list_delivery_retention_candidates()
        .await
        .expect("list delivery retention candidates");
    assert_eq!(candidates.len(), 2);

    // Model a row replacement between classification and deletion: the row key
    // now identifies the second row while the stale plan still carries the
    // first row's process id. No current row matches the complete observation.
    let mut stale_observation = candidates[0].clone();
    stale_observation.occurrence_id = candidates[1].occurrence_id.clone();
    stale_observation.subscription_id = candidates[1].subscription_id.clone();
    assert_eq!(
        handles
            .triggers
            .delete_delivery_retention_candidates(&[stale_observation])
            .await
            .expect("apply stale row observation"),
        0,
        "a stale classification must not expand into a process-wide delete"
    );
    assert_eq!(
        handles
            .triggers
            .list_delivery_retention_candidates()
            .await
            .expect("list rows after stale delete")
            .len(),
        2,
        "both the original and replacement rows survive a mismatched observation"
    );

    assert_eq!(
        handles
            .triggers
            .delete_delivery_retention_candidates(std::slice::from_ref(&candidates[0]))
            .await
            .expect("delete exact observed row"),
        1,
        "the exact observed row remains reclaimable"
    );
    assert_eq!(
        handles
            .triggers
            .list_delivery_retention_candidates()
            .await
            .expect("list rows after exact delete"),
        vec![candidates[1].clone()],
        "an exact delete preserves every unlisted row"
    );
}

async fn outstanding_delivery_blocks_interleaved_tombstone_compaction(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "process-compact-interleave-session";
    register_trigger(
        &handles.triggers,
        SESSION,
        "process-compact-interleave-key",
        "process-compact-interleave-source",
        "process-compact-interleave-register",
    )
    .await;
    let ingress = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-compact-interleave-source",
            serde_json::json!({ "button": "Blue" }),
            "process-compact-interleave-occurrence",
        ))
        .await
        .expect("ingest occurrence");
    assert_eq!(ingress.reservations.len(), 1);
    let process_id = ingress.reservations[0].process_id.clone();
    handles
        .registry
        .register_process(
            ProcessRegistration::new(
                process_id.clone(),
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                RecoveryContract::ExternallyOwned,
                ProcessProvenance::host(),
            )
            .with_identity(ProcessIdentity::new("test")),
        )
        .await
        .expect("register delivery process");
    handles
        .registry
        .complete_process(
            &process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete delivery process");

    // Compaction's preceding reconciliation observes the process as live and
    // preserves its delivery. A concurrent retention writer then prunes the
    // process before raw compaction starts. The raw lever must perform its own
    // complete delivery survey and refuse the new tombstone.
    assert_eq!(
        crate::reconcile_pruned_trigger_deliveries(
            handles.registry.as_ref(),
            handles.triggers.as_ref(),
            Some(handles.sessions.as_ref()),
        )
        .await
        .expect("reconcile while process is live")
        .reclaimed_delivery_count,
        0
    );
    handles
        .registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("interleaved process prune");
    assert_eq!(
        handles
            .registry
            .compact_process_tombstones(
                u64::MAX,
                ProjectionWatermark::NoProjector,
                Some(handles.triggers.as_ref()),
            )
            .await
            .expect("delivery-aware tombstone compaction"),
        0,
        "compaction must refuse a tombstone guarded by an outstanding delivery"
    );
    assert_eq!(
        handles
            .registry
            .filter_tombstoned_process_ids(std::slice::from_ref(&process_id))
            .await
            .expect("classify guarded tombstone"),
        vec![process_id.clone()],
        "the outstanding delivery's tombstone must remain durable"
    );
    assert_eq!(
        handles
            .triggers
            .list_deliveries_by_process_id(&process_id)
            .await
            .expect("list guarded delivery")
            .len(),
        1,
        "refusing compaction preserves the delivery beside its tombstone"
    );

    assert_eq!(
        crate::reconcile_pruned_trigger_deliveries(
            handles.registry.as_ref(),
            handles.triggers.as_ref(),
            Some(handles.sessions.as_ref()),
        )
        .await
        .expect("reconcile guarded delivery")
        .reclaimed_delivery_count,
        1
    );
    assert_eq!(
        handles
            .registry
            .compact_process_tombstones(
                u64::MAX,
                ProjectionWatermark::NoProjector,
                Some(handles.triggers.as_ref()),
            )
            .await
            .expect("compact reconciled tombstone"),
        1,
        "a later cycle may compact after the delivery is gone"
    );
    assert!(
        handles
            .triggers
            .list_deliveries_by_process_id(&process_id)
            .await
            .expect("list delivery after compaction")
            .is_empty(),
        "compaction can never orphan the delivery"
    );
}

fn owner(session_id: &str) -> TriggerOwnerScope {
    TriggerOwnerScope::session(session_id)
}

fn actor(session_id: &str) -> ProcessOriginator {
    ProcessOriginator::session(SessionScope::new(session_id))
}

fn draft(session_id: &str, key: &str, source_key: &str) -> TriggerSubscriptionDraft {
    let mut input_template = BTreeMap::new();
    input_template.insert("event".to_string(), crate::TriggerInputBinding::Event);
    TriggerSubscriptionDraft {
        subscription_key: key.to_string(),
        env_ref: crate::ProcessExecutionEnvRef::new(format!("process-env:{session_id}")),
        wake_target: Some(SessionScope::new(session_id)),
        name: Some("worker".to_string()),
        source_type: "ui.button.pressed".to_string(),
        source_key: source_key.to_string(),
        source: serde_json::json!({ "button": "Blue" }),
        payload_schema: crate::LashSchema::new(serde_json::json!({
            "type": "object",
            "properties": { "button": { "type": "string" } },
            "required": ["button"],
            "additionalProperties": false
        })),
        target: ProcessInput::Engine {
            kind: "test".to_string(),
            payload: serde_json::json!({ "process": "worker" }),
        },
        target_identity: ProcessIdentity::new("test")
            .with_label(Some("worker".to_string()))
            .with_definition(Some(serde_json::json!({ "process_name": "worker" }))),
        event_types: Vec::new(),
        input_template,
        target_label: Some("worker".to_string()),
    }
}

async fn register_trigger(
    triggers: &Arc<dyn TriggerStore>,
    session_id: &str,
    key: &str,
    source_key: &str,
    operation_id: &str,
) {
    triggers
        .execute_command(
            operation_id,
            TriggerCommand::Register {
                owner_scope: owner(session_id),
                actor: actor(session_id),
                draft: draft(session_id, key, source_key),
            },
        )
        .await
        .expect("register trigger call")
        .expect("register trigger succeeds");
}

async fn process_prune_preserves_trigger_mutation_receipts(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "process-prune-receipt-session";
    const KEY: &str = "process-prune-receipt-key";
    register_trigger(
        &handles.triggers,
        SESSION,
        KEY,
        "process-prune-receipt-v1",
        "process-prune-receipt-register",
    )
    .await;
    let update = TriggerCommand::Update {
        owner_scope: owner(SESSION),
        actor: actor(SESSION),
        subscription_key: KEY.to_string(),
        draft: draft(SESSION, KEY, "process-prune-receipt-v2"),
        expected_revision: 1,
    };
    let committed = handles
        .triggers
        .execute_command("process-prune-receipt-update", update.clone())
        .await
        .expect("update trigger call")
        .expect("update trigger succeeds");

    let report = handles
        .registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune with no processes");
    assert_eq!(report.pruned_processes, 0, "no process was eligible");

    let retried = handles
        .triggers
        .execute_command("process-prune-receipt-update", update)
        .await
        .expect("retry trigger update")
        .expect("retry returns the committed result");
    assert_eq!(
        retried, committed,
        "process prune must preserve the original trigger mutation receipt"
    );
}

async fn prune_with_trigger_cleanup(handles: &ProcessTriggerRetentionHandles) {
    handles
        .registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune terminal processes");
    crate::reconcile_pruned_trigger_deliveries(
        handles.registry.as_ref(),
        handles.triggers.as_ref(),
        Some(handles.sessions.as_ref()),
    )
    .await
    .expect("reconcile pruned trigger deliveries");
}

async fn process_prune_only_deletes_deliveries_for_pruned_processes(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "process-prune-scope-session";
    register_trigger(
        &handles.triggers,
        SESSION,
        "process-prune-scope-key",
        "process-prune-scope-source",
        "process-prune-scope-register",
    )
    .await;
    let first = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-prune-scope-source",
            serde_json::json!({ "button": "Blue" }),
            "process-prune-scope-first",
        ))
        .await
        .expect("ingest first occurrence");
    let second = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-prune-scope-source",
            serde_json::json!({ "button": "Blue" }),
            "process-prune-scope-second",
        ))
        .await
        .expect("ingest second occurrence");
    assert_eq!(first.reservations.len(), 1);
    assert_eq!(second.reservations.len(), 1);
    let pruned_id = first.reservations[0].process_id.clone();
    let live_id = second.reservations[0].process_id.clone();

    for process_id in [&pruned_id, &live_id] {
        handles
            .registry
            .register_process(
                ProcessRegistration::new(
                    process_id.clone(),
                    ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    RecoveryContract::ExternallyOwned,
                    ProcessProvenance::host(),
                )
                .with_identity(ProcessIdentity::new("test")),
            )
            .await
            .expect("register delivery process");
    }
    handles
        .registry
        .complete_process(
            &pruned_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete prunable delivery process");

    prune_with_trigger_cleanup(&handles).await;

    assert!(
        handles
            .triggers
            .list_deliveries_by_process_id(&pruned_id)
            .await
            .expect("list pruned deliveries")
            .is_empty(),
        "process prune must delete a pruned process's trigger delivery"
    );
    assert_eq!(
        handles
            .triggers
            .list_deliveries_by_process_id(&live_id)
            .await
            .expect("list live deliveries")
            .len(),
        1,
        "process prune must preserve deliveries for processes it did not prune"
    );
}

async fn pruned_delivery_process_is_not_a_recovery_candidate(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "process-prune-tombstone-session";
    register_trigger(
        &handles.triggers,
        SESSION,
        "process-prune-tombstone-key",
        "process-prune-tombstone-source",
        "process-prune-tombstone-register",
    )
    .await;
    let ingress = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-prune-tombstone-source",
            serde_json::json!({ "button": "Blue" }),
            "process-prune-tombstone-occurrence",
        ))
        .await
        .expect("ingest occurrence");
    assert_eq!(ingress.reservations.len(), 1);
    let process_id = ingress.reservations[0].process_id.clone();
    handles
        .registry
        .register_process(
            ProcessRegistration::new(
                process_id.clone(),
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                RecoveryContract::ExternallyOwned,
                ProcessProvenance::host(),
            )
            .with_identity(ProcessIdentity::new("test")),
        )
        .await
        .expect("register delivery process");
    handles
        .registry
        .complete_process(
            &process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete delivery process");

    prune_with_trigger_cleanup(&handles).await;

    assert!(
        handles
            .triggers
            .list_deliveries_by_process_id(&process_id)
            .await
            .expect("list delivery after prune")
            .is_empty(),
        "pruned delivery must not survive"
    );
    assert!(
        handles
            .registry
            .filter_unregistered_process_ids(std::slice::from_ref(&process_id))
            .await
            .expect("filter recovery candidates")
            .is_empty(),
        "a tombstoned process must not be offered back to the recovery sweep"
    );
}

async fn reregistered_between_classification_and_delete_preserves_delivery(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "process-prune-reuse-session";
    register_trigger(
        &handles.triggers,
        SESSION,
        "process-prune-reuse-key",
        "process-prune-reuse-source",
        "process-prune-reuse-register",
    )
    .await;
    let ingress = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-prune-reuse-source",
            serde_json::json!({ "button": "Blue" }),
            "process-prune-reuse-occurrence",
        ))
        .await
        .expect("ingest occurrence");
    assert_eq!(ingress.reservations.len(), 1);
    let process_id = ingress.reservations[0].process_id.clone();
    let registration = || {
        ProcessRegistration::new(
            process_id.clone(),
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryContract::ExternallyOwned,
            ProcessProvenance::host(),
        )
        .with_identity(ProcessIdentity::new("test"))
    };
    handles
        .registry
        .register_process(registration())
        .await
        .expect("register delivery process");
    handles
        .registry
        .complete_process(
            &process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete delivery process");
    handles
        .registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune delivery process");
    assert_eq!(
        handles
            .triggers
            .list_deliveries_by_process_id(&process_id)
            .await
            .expect("list delivery after prune")
            .len(),
        1,
        "the coordinator is the only process-trigger delivery reclamation path"
    );

    assert_eq!(
        handles
            .registry
            .filter_tombstoned_process_ids(std::slice::from_ref(&process_id))
            .await
            .expect("classify tombstoned id before interleaving"),
        vec![process_id.clone()],
        "the reconciliation plan must first classify the retained tombstone"
    );
    let registry = Arc::clone(&handles.registry);
    let interleaved_process_id = process_id.clone();
    assert_eq!(
        crate::runtime::reconcile_pruned_trigger_deliveries_interleaved(
            handles.registry.as_ref(),
            handles.triggers.as_ref(),
            Some(handles.sessions.as_ref()),
            move || async move {
                registry
                    .register_process(
                        ProcessRegistration::new(
                            interleaved_process_id,
                            ProcessInput::External {
                                metadata: serde_json::Value::Null,
                            },
                            RecoveryContract::ExternallyOwned,
                            ProcessProvenance::host(),
                        )
                        .with_identity(ProcessIdentity::new("test")),
                    )
                    .await
                    .expect("re-register after classification and before delete");
            },
        )
        .await
        .expect("reconcile across process-id reuse interleaving")
        .reclaimed_delivery_count,
        0,
        "stale classification must not delete a re-registered process's delivery"
    );
    assert!(
        handles
            .registry
            .filter_tombstoned_process_ids(std::slice::from_ref(&process_id))
            .await
            .expect("filter tombstoned ids after re-registration")
            .is_empty(),
        "a live process shadows its stale tombstone"
    );
    assert!(
        handles
            .registry
            .filter_unregistered_process_ids(std::slice::from_ref(&process_id))
            .await
            .expect("filter unregistered ids")
            .is_empty(),
        "a re-registered process is live, not unregistered"
    );
    assert_eq!(
        handles
            .triggers
            .list_deliveries_by_process_id(&process_id)
            .await
            .expect("list delivery after reconciliation")
            .len(),
        1,
        "a re-registered process's delivery must survive reconciliation"
    );
}
