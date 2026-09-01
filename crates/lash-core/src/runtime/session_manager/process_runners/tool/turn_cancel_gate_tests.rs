//! Runner-side deferred-tool awaits keep the process body's turn-cancel
//! observation decision instead of rebuilding an always-observing wait.

use super::*;
use lash_sansio::sync::MutexExt;
use std::sync::Arc;

#[derive(Default)]
struct AwaitShapeRecorder {
    waits: std::sync::Mutex<Vec<(bool, Option<crate::ExecutionScope>)>>,
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for AwaitShapeRecorder {
    async fn prepare_completion_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<crate::CompletionKeyPreparation, crate::RuntimeError> {
        if may_defer {
            Ok(crate::CompletionKeyPreparation::Issued(
                crate::AwaitEventKey {
                    scope: scope.clone(),
                    wait,
                    key_id: "process-witness-key".to_string(),
                    signature: "process-witness-signature".to_string(),
                },
            ))
        } else {
            Ok(crate::CompletionKeyPreparation::NotNeeded)
        }
    }
}

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
        self.waits
            .lock_recover()
            .push((options.observe_turn_cancel, options.turn_cancel_scope));
        Ok(crate::RuntimeEffectOutcome::AwaitEvent {
            resolution: crate::Resolution::Ok(serde_json::Value::Null),
        })
    }
}

struct PendingProcessTool {
    definition: crate::ToolDefinition,
}

#[async_trait::async_trait]
impl crate::ToolProvider for PendingProcessTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![self.definition.manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        tool_id == self.definition.id()
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        let _ = call
            .context
            .completion_key()
            .expect("the process witness receives its pre-derived completion key");
        crate::ToolOutcome::pending(crate::PendingCompletion::default())
    }
}

#[tokio::test]
async fn runner_side_deferred_await_inside_a_process_body_attaches_no_turn_cancel_gate() {
    let recorder = AwaitShapeRecorder::default();
    let wait =
        crate::runtime::TurnCancelWait::unobserved(tokio_util::sync::CancellationToken::new());
    let key = crate::AwaitEventKey {
        scope: crate::ExecutionScope::process("process-1"),
        wait: crate::AwaitEventWaitIdentity::ToolCompletion {
            tool_call_id: "call-1".to_string(),
        },
        key_id: "key-1".to_string(),
        signature: "signature-1".to_string(),
    };
    let pending = crate::tool_dispatch::PendingToolDispatchOutcome {
        tool_name: "deferred".to_string(),
        args: serde_json::json!({}),
        key,
        pending: crate::PendingCompletion::default(),
        duration_ms: 0,
        attempts: Vec::new(),
    };
    let invocation = crate::RuntimeInvocation::effect(
        crate::RuntimeScope::new("session-1"),
        "process:process-1:tool:deferred:await",
        crate::RuntimeEffectKind::AwaitEvent,
        "process:process-1:tool:deferred:await",
    );

    let resolution = await_pending_process_tool(
        &recorder,
        Arc::new(crate::SystemClock),
        invocation,
        pending,
        &wait,
    )
    .await
    .expect("the deferred await resolves");

    assert!(matches!(resolution, crate::Resolution::Ok(_)));
    assert_eq!(
        *recorder.waits.lock_recover(),
        vec![(false, None)],
        "the runner-side process await must not attach the turn-cancel gate"
    );
}

/// Runs the production process-tool runner far enough to park on its deferred
/// await. The runner's owning process execution supplies the unobserved trio.
#[tokio::test]
async fn process_runner_deferred_await_uses_the_owning_process_execution_trio() {
    let definition = crate::ToolDefinition::raw(
        "tool:process-witness",
        "process_witness",
        "process runner witness",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object"}),
    );
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(PendingProcessTool {
        definition: definition.clone(),
    });
    let runtime = crate::runtime::tests::helpers::runtime_with_plugins_and_tools(
        crate::testing::test_standard_protocol_factories(),
        provider,
        crate::runtime::tests::helpers::mock_provider(Vec::new()),
    )
    .await;
    let services = runtime
        .runtime_session_services()
        .expect("runtime session services");
    let recorder = Arc::new(AwaitShapeRecorder::default());
    let scoped = crate::ScopedEffectController::shared(
        Arc::clone(&recorder) as Arc<dyn crate::RuntimeEffectController>,
        crate::ExecutionScope::process("process-witness"),
    )
    .expect("valid process scope");
    let call = crate::PreparedToolCall::identity(
        definition.manifest.id.clone(),
        crate::sansio::PendingToolCall {
            call_id: "process-witness-call".to_string(),
            tool_name: definition.name().to_string(),
            args: serde_json::json!({}),
            replay: None,
        },
    );
    let registration = crate::ProcessRegistration::new(
        "process-witness",
        crate::ProcessInput::ToolCall { call: call.clone() },
        crate::RecoveryContract::Rerunnable,
        crate::ProcessProvenance::host(),
    );
    let cancellation = tokio_util::sync::CancellationToken::new();
    let (output, _) = services
        .run_process_tool_call(ProcessToolCallRun {
            registration,
            call,
            parent_invocation: None,
            execution_write_authority: crate::ProcessExecutionWriteAuthority::invocation(
                "process-witness",
                "process-witness-execution",
            ),
            scoped_effect_controller: scoped,
            cancellation,
        })
        .await;

    assert!(output.into_tool_output().is_success());
    assert_eq!(
        *recorder.waits.lock_recover(),
        vec![(false, None)],
        "the process runner await must use the owning process execution's trio"
    );
}
