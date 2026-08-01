//! Cross-backend conformance for process retention's trigger-store effects.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use crate::{
    ProcessIdentity, ProcessInput, ProcessOriginator, ProcessRegistry, ProjectionWatermark,
    SessionScope, TriggerCommand, TriggerOwnerScope, TriggerStore, TriggerSubscriptionDraft,
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
