//! Cross-backend conformance for process retention's trigger-store effects.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use crate::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessIdentity, ProcessInput,
    ProcessOriginator, ProcessProvenance, ProcessRegistration, ProcessRegistry,
    ProjectionWatermark, RecoveryDisposition, SessionScope, TriggerCommand, TriggerOwnerScope,
    TriggerStore, TriggerSubscriptionDraft,
};

/// Fresh paired process and trigger stores for retention conformance.
pub struct ProcessTriggerRetentionHandles {
    pub registry: Arc<dyn ProcessRegistry>,
    pub triggers: Arc<dyn TriggerStore>,
}

/// Run the process/trigger retention laws against one backend.
pub async fn process_trigger_retention<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ProcessTriggerRetentionHandles>,
{
    process_prune_preserves_trigger_mutation_receipts(make().await).await;
    process_prune_only_deletes_deliveries_for_pruned_processes(make().await).await;
    pruned_delivery_process_is_not_a_recovery_candidate(make().await).await;
    reregistered_process_shadows_tombstone_and_preserves_delivery(make().await).await;
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
                    RecoveryDisposition::ExternallyOwned,
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
            ProcessAwaitOutput::Success {
                value: serde_json::json!("done"),
                control: None,
            },
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
                RecoveryDisposition::ExternallyOwned,
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
            ProcessAwaitOutput::Success {
                value: serde_json::json!("done"),
                control: None,
            },
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

async fn reregistered_process_shadows_tombstone_and_preserves_delivery(
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
            RecoveryDisposition::ExternallyOwned,
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
            ProcessAwaitOutput::Success {
                value: serde_json::json!("done"),
                control: None,
            },
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

    handles
        .registry
        .register_process(registration())
        .await
        .expect("re-register pruned process id");
    assert!(
        handles
            .registry
            .filter_tombstoned_process_ids(std::slice::from_ref(&process_id))
            .await
            .expect("filter tombstoned ids")
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
        crate::reconcile_pruned_trigger_deliveries(
            handles.registry.as_ref(),
            handles.triggers.as_ref(),
        )
        .await
        .expect("reconcile after process-id reuse"),
        0,
        "the coordinator must never delete a live process's delivery"
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
