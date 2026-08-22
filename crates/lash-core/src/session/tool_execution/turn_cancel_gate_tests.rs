//! The turn-cancel gate a deferred tool await attaches follows the execution
//! that owns the wait, not the wait executor's default.
//!
//! A process body runs with turn-cancel observation switched off. Before
//! FIG-1759 the deferred-tool await inside a process body stamped only the
//! execution scope and inherited the executor's observing default, so the wait
//! attached the very turn-cancel gate the process runner had switched off.

use lash_sansio::sync::MutexExt;
use std::sync::Arc;

/// Captures the wait shape each `AwaitEvent` effect asks the host for.
#[derive(Default)]
struct AwaitShapeRecorder {
    waits: std::sync::Mutex<Vec<(bool, Option<crate::ExecutionScope>)>>,
}

impl crate::AwaitEventResolver for AwaitShapeRecorder {}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for AwaitShapeRecorder {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        if !matches!(
            &envelope.command,
            crate::RuntimeEffectCommand::AwaitEvent { .. }
        ) {
            return local_executor.execute(envelope).await;
        }
        let options = local_executor.into_await_event_options()?;
        self.waits.lock_recover().push((
            options.observe_turn_cancel,
            options.turn_cancel_scope.clone(),
        ));
        Ok(crate::RuntimeEffectOutcome::AwaitEvent {
            resolution: crate::Resolution::Ok(serde_json::json!({"done": true})),
        })
    }
}

fn turn_scope() -> crate::ExecutionScope {
    crate::ExecutionScope::Turn {
        session_id: "test-session".to_string(),
        turn_id: "turn-1".to_string(),
    }
}

fn pending_tool() -> crate::tool_dispatch::PendingToolDispatchOutcome {
    crate::tool_dispatch::PendingToolDispatchOutcome {
        tool_name: "deferred".to_string(),
        args: serde_json::json!({}),
        key: crate::AwaitEventKey {
            scope: turn_scope(),
            wait: crate::AwaitEventWaitIdentity::ToolCompletion {
                tool_call_id: "call".to_string(),
            },
            key_id: "key".to_string(),
            signature: "signature".to_string(),
        },
        pending: crate::PendingCompletion::default(),
        duration_ms: 0,
        attempts: Vec::new(),
    }
}

/// Awaits one deferred tool completion under an execution context built with
/// `observe_turn_cancel` set as the caller asks, and reports the wait shape the
/// host was handed.
async fn deferred_tool_await_shape(
    observe_turn_cancel: bool,
) -> (bool, Option<crate::ExecutionScope>) {
    let recorder = Arc::new(AwaitShapeRecorder::default());
    let scoped = crate::ScopedEffectController::shared(
        Arc::clone(&recorder) as Arc<dyn crate::RuntimeEffectController>,
        turn_scope(),
    )
    .expect("valid turn scope");
    let mut context = crate::testing::TestExecutionContextBuilder::new()
        .plugin_factories(Vec::new())
        .borrowed_effect_controller(scoped)
        .build()
        .into_runtime();
    if !observe_turn_cancel {
        context = context.without_turn_cancel_observation();
    }
    let outcome = context
        .await_pending_tool_dispatch_outcome_with_suffix(
            "call",
            None,
            "call:await".to_string(),
            pending_tool(),
            None,
        )
        .await;
    assert_eq!(outcome.record.tool, "deferred");
    let waits = recorder.waits.lock_recover().clone();
    assert_eq!(waits.len(), 1, "exactly one durable wait per deferred tool");
    waits.into_iter().next().expect("the recorded wait")
}

/// A turn-driven deferred tool await keeps the turn-cancel gate: the wait is
/// what a `turn.cancel` has to interrupt.
#[tokio::test]
async fn deferred_tool_await_under_a_turn_attaches_the_turn_cancel_gate() {
    let (observe_turn_cancel, scope) = deferred_tool_await_shape(true).await;
    assert!(observe_turn_cancel);
    assert_eq!(scope, Some(turn_scope()));
}

/// The FIG-1759 fix: inside a process body the same await carries the body's
/// opt-out, so the host registers no turn-cancel gate for it.
#[tokio::test]
async fn deferred_tool_await_inside_a_process_body_attaches_no_turn_cancel_gate() {
    let (observe_turn_cancel, scope) = deferred_tool_await_shape(false).await;
    assert!(
        !observe_turn_cancel,
        "a process body runs without turn-cancel observation"
    );
    assert_eq!(
        scope, None,
        "no scope means no gate to register, whatever the host does with the flag"
    );
}
