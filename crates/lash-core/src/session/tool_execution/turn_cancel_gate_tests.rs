//! The turn-cancel gate a deferred tool await attaches follows the execution
//! that owns the wait, not the wait executor's default.
//!
//! A process body runs with turn-cancel observation switched off. Before
//! FIG-1759 the deferred-tool await inside a process body stamped only the
//! execution scope and inherited the executor's observing default, so the wait
//! attached the very turn-cancel gate the process runner had switched off.

use lash_sansio::sync::MutexExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Captures the wait shape each `AwaitEvent` effect asks the host for.
#[derive(Default)]
struct AwaitShapeRecorder {
    waits: std::sync::Mutex<Vec<(bool, Option<crate::ExecutionScope>)>>,
    sleeps: std::sync::Mutex<Vec<(bool, Option<crate::ExecutionScope>)>>,
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
            if matches!(&envelope.command, crate::RuntimeEffectCommand::Sleep { .. }) {
                let options = local_executor.into_sleep_options();
                self.sleeps
                    .lock_recover()
                    .push((options.observe_turn_cancel, options.turn_cancel_scope));
                return Ok(crate::RuntimeEffectOutcome::Sleep);
            }
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

struct ScalarRetryTool {
    definition: crate::ToolDefinition,
    attempts: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::ToolProvider for ScalarRetryTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![self.definition.manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            crate::ToolOutcome::retryable_failure(
                crate::ToolFailureClass::External,
                "retryable",
                "retry witness failure",
                Some(1),
            )
        } else {
            crate::ToolOutcome::ok(serde_json::json!({"ok": true}))
        }
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

/// Drives the scalar production call site through a retry so the retry sleep
/// receives its trio from the owning turn execution.
#[tokio::test]
async fn scalar_retry_sleep_attaches_the_owning_turn_cancel_gate() {
    let definition = crate::ToolDefinition::raw(
        "tool:scalar-retry-witness",
        "scalar_retry_witness",
        "scalar retry witness",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object"}),
    )
    .with_retry_policy(crate::ToolRetryPolicy::safe(2, 1, 1));
    let provider = Arc::new(ScalarRetryTool {
        definition: definition.clone(),
        attempts: AtomicUsize::new(0),
    });
    let recorder = Arc::new(AwaitShapeRecorder::default());
    let scoped = crate::ScopedEffectController::shared(
        Arc::clone(&recorder) as Arc<dyn crate::RuntimeEffectController>,
        turn_scope(),
    )
    .expect("valid turn scope");
    let context = crate::testing::TestExecutionContextBuilder::new()
        .plugin_factories(Vec::new())
        .provider(provider)
        .tool_catalog(crate::ToolCatalog::from_tool_definitions(vec![
            definition.clone(),
        ]))
        .borrowed_effect_controller(scoped)
        .build()
        .into_runtime();

    let reply = context
        .call_tool_by_id(
            "scalar-retry-call".to_string(),
            definition.manifest.id,
            serde_json::json!({}),
            0,
        )
        .await;
    assert!(
        reply
            .record
            .expect("scalar tool record")
            .output
            .is_success()
    );
    assert_eq!(
        *recorder.sleeps.lock_recover(),
        vec![(true, Some(turn_scope()))],
        "the scalar retry sleep must use the owning turn execution's trio"
    );
}
