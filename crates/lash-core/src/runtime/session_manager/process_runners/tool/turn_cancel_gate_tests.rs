//! Runner-side deferred-tool awaits keep the process body's turn-cancel
//! observation decision instead of rebuilding an always-observing wait.

use super::*;
use lash_sansio::sync::MutexExt;
use std::sync::Arc;

#[derive(Default)]
struct AwaitShapeRecorder {
    waits: std::sync::Mutex<Vec<(bool, Option<crate::ExecutionScope>)>>,
    invocations: std::sync::Mutex<Vec<(crate::RuntimeEffectKind, crate::RuntimeEffectInvocation)>>,
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
        self.invocations
            .lock_recover()
            .push((envelope.command.kind(), envelope.invocation.clone()));
        if !matches!(
            &envelope.command,
            crate::RuntimeEffectCommand::AwaitEvent { .. }
        ) {
            if matches!(&envelope.command, crate::RuntimeEffectCommand::Sleep { .. }) {
                return Ok(crate::RuntimeEffectOutcome::Sleep);
            }
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

struct RetryingProcessTool {
    definition: crate::ToolDefinition,
    attempts: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl crate::ToolProvider for RetryingProcessTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![self.definition.manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        if self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            crate::ToolOutcome::retryable_failure(
                crate::ToolFailureClass::Execution,
                "retry_once",
                "retry once",
                Some(1),
            )
        } else {
            crate::ToolOutcome::ok(serde_json::json!({"ok": true}))
        }
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
    let invocation = crate::RuntimeEffectInvocation::new(
        crate::EffectAddress::new(
            crate::ExecutionScope::process("process-1"),
            "process:process-1:tool:deferred:await",
        )
        .expect("valid process tool address"),
        crate::RuntimeAttribution::for_session("session-1"),
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

async fn run_retrying_host_process_tool(
    parent_invocation: Option<crate::RuntimeInvocation>,
) -> Vec<(crate::RuntimeEffectKind, crate::RuntimeEffectInvocation)> {
    let definition = crate::ToolDefinition::raw(
        "tool:process-attribution-witness",
        "process_attribution_witness",
        "process attribution witness",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object"}),
    )
    .with_retry_policy(crate::ToolRetryPolicy::safe(2, 1, 1));
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(RetryingProcessTool {
        definition: definition.clone(),
        attempts: std::sync::atomic::AtomicUsize::new(0),
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
        crate::ExecutionScope::process("process-attribution-witness"),
    )
    .expect("valid process scope");
    let call = crate::PreparedToolCall::identity(
        definition.manifest.id.clone(),
        crate::sansio::PendingToolCall {
            call_id: "process-attribution-call".to_string(),
            tool_name: definition.name().to_string(),
            args: serde_json::json!({}),
            replay: None,
        },
    );
    let registration = crate::ProcessRegistration::new(
        "process-attribution-witness",
        crate::ProcessInput::ToolCall { call: call.clone() },
        crate::RecoveryContract::Rerunnable,
        crate::ProcessProvenance::host(),
    );
    let (output, _) = services
        .run_process_tool_call(ProcessToolCallRun {
            registration,
            call,
            parent_invocation,
            execution_write_authority: crate::ProcessExecutionWriteAuthority::invocation(
                "process-attribution-witness",
                "process-attribution-execution",
            ),
            scoped_effect_controller: scoped,
            cancellation: tokio_util::sync::CancellationToken::new(),
        })
        .await;
    assert!(output.into_tool_output().is_success());
    recorder.invocations.lock_recover().clone()
}

#[tokio::test]
async fn host_process_runner_attempts_and_retry_sleep_do_not_claim_ambient_session() {
    let invocations = Box::pin(run_retrying_host_process_tool(None)).await;
    let relevant = invocations
        .iter()
        .filter(|(kind, _)| {
            matches!(
                kind,
                crate::RuntimeEffectKind::ToolAttempt | crate::RuntimeEffectKind::Sleep
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        relevant.len(),
        3,
        "two attempts and one retry sleep cross the controller"
    );
    assert!(
        relevant
            .iter()
            .all(|(_, invocation)| invocation.attribution.is_none()),
        "a parentless host process must not borrow CurrentSession attribution: {relevant:?}"
    );
}

#[tokio::test]
async fn process_runner_attempts_and_retry_sleep_preserve_actual_parent_attribution() {
    let parent = crate::RuntimeInvocation::effect(
        crate::EffectAddress::new(
            crate::ExecutionScope::turn("origin-session", "origin-turn"),
            "origin-effect",
        )
        .expect("valid origin address"),
        crate::RuntimeAttribution::for_turn("origin-session", "origin-turn", 4, 2),
        "origin-effect",
    );
    let expected = parent.attribution.clone();
    let invocations = Box::pin(run_retrying_host_process_tool(Some(parent))).await;
    let relevant = invocations
        .iter()
        .filter(|(kind, _)| {
            matches!(
                kind,
                crate::RuntimeEffectKind::ToolAttempt | crate::RuntimeEffectKind::Sleep
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        relevant.len(),
        3,
        "two attempts and one retry sleep cross the controller"
    );
    assert!(
        relevant
            .iter()
            .all(|(_, invocation)| invocation.attribution == expected),
        "a real causal parent remains the attribution source: {relevant:?}"
    );
}
