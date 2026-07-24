use super::*;

use async_trait::async_trait;
use lash_core::plugin::runtime_host::{
    SessionGraphService, SessionLifecycleService, SessionStateService,
};
use lash_core::plugin::{PluginError, SessionHandle};
use lash_core::{
    DirectCompletionClient, DurabilityTier, RuntimeEffectController, RuntimeSessionState,
    SessionCreateRequest, SessionSnapshot, ToolCall, ToolProvider,
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct ConformanceSessionHost {
    snapshot: RuntimeSessionState,
}

#[async_trait]
impl SessionStateService for ConformanceSessionHost {
    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.to_snapshot())
    }

    async fn snapshot_session(&self, _session_id: &str) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.to_snapshot())
    }

    async fn tool_catalog(&self, _session_id: &str) -> Result<Vec<serde_json::Value>, PluginError> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl SessionLifecycleService for ConformanceSessionHost {
    async fn create_session(
        &self,
        _request: SessionCreateRequest,
    ) -> Result<SessionHandle, PluginError> {
        Err(PluginError::Session("not used".to_string()))
    }

    async fn close_session(&self, _session_id: &str) -> Result<(), PluginError> {
        Ok(())
    }
}

#[async_trait]
impl SessionGraphService for ConformanceSessionHost {}

fn tool_context() -> lash_core::ToolContext<'static> {
    let host = Arc::new(ConformanceSessionHost::default());
    let completions = DirectCompletionClient::from_fn(|_, usage_source| {
        assert_eq!(usage_source, "llm_query");
        Ok(lash_core::DirectCompletion {
            text: r#"{"kind":"value","value":{"answer":"covered"},"error":null}"#.to_string(),
            usage: lash_core::TokenUsage::default(),
            llm_call: lash_core::LlmCallRecord {
                call_id: lash_core::LlmCallId("tool-context-conformance".to_string()),
                label: None,
                attempts: Vec::new(),
            },
        })
    });
    lash_core::testing::mock_tool_context_with_host_and_direct_completions(host, completions)
}

fn args_for(tool_name: &str) -> serde_json::Value {
    match tool_name {
        "llm_query" => serde_json::json!({
            "task": "Return the covered answer",
            "inputs": {"answer": "covered"},
            "output": {"answer": "str"}
        }),
        other => panic!(
            "first-party tool `{other}` was registered without a conformance fixture; add its arguments before merging"
        ),
    }
}

fn tool_attempt_envelope(
    tier: DurabilityTier,
    tool_name: &str,
    args: serde_json::Value,
) -> RuntimeEffectEnvelope {
    let (scope, context_name) = match tier {
        DurabilityTier::Inline => (
            RuntimeScope::for_turn("tool-context-conformance", "inline", 1, 0),
            "inline",
        ),
        DurabilityTier::Durable => (
            RuntimeScope::new("tool-context-conformance-durable"),
            "restate-durable",
        ),
    };
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            scope,
            format!("{context_name}:{tool_name}"),
            RuntimeEffectKind::ToolAttempt,
            format!("tool-context:{context_name}:{tool_name}"),
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: lash_core::PreparedToolCall::from_parts(
                format!("{context_name}-{tool_name}"),
                format!("tool:{tool_name}"),
                tool_name,
                args,
                None,
                serde_json::Value::Null,
            ),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    )
}

fn executing_tool(
    provider: Arc<dyn ToolProvider>,
    tool_name: String,
    args: serde_json::Value,
    executions: Arc<AtomicUsize>,
) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |_| {
        let provider = Arc::clone(&provider);
        let tool_name = tool_name.clone();
        let args = args.clone();
        let executions = Arc::clone(&executions);
        async move {
            executions.fetch_add(1, Ordering::SeqCst);
            let context = tool_context();
            let output = provider
                .execute(ToolCall {
                    name: &tool_name,
                    args: &args,
                    context: &context,
                    progress: None,
                })
                .await
                .into_done_output()
                .map_err(|_| {
                    RuntimeEffectControllerError::new(
                        "tool_context_conformance_pending",
                        format!("first-party tool `{tool_name}` unexpectedly returned pending"),
                    )
                })?;
            assert!(
                output.is_success(),
                "{tool_name} failed in the conformance matrix: {output:?}"
            );
            Ok(RuntimeEffectOutcome::ToolAttempt {
                launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                    record: Box::new(lash_core::ToolCallRecord {
                        call_id: Some(format!("conformance-{tool_name}")),
                        tool: tool_name,
                        args,
                        output,
                        duration_ms: 0,
                    }),
                }),
                triggers: Vec::new(),
            })
        }
    })
}

fn replay_must_not_execute(executions: Arc<AtomicUsize>) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |_| {
        let executions = Arc::clone(&executions);
        async move {
            executions.fetch_add(1, Ordering::SeqCst);
            Err(RuntimeEffectControllerError::new(
                "tool_context_conformance_reexecuted",
                "recorded replay must not execute the first-party tool again",
            ))
        }
    })
}

async fn assert_cell(
    controller: &dyn RuntimeEffectController,
    start_replay: impl FnOnce(),
    tier: DurabilityTier,
    provider: Arc<dyn ToolProvider>,
    tool_name: String,
) {
    let args = args_for(&tool_name);
    let envelope = tool_attempt_envelope(tier, &tool_name, args.clone());
    let executions = Arc::new(AtomicUsize::new(0));
    let first = controller
        .execute_effect(
            envelope.clone(),
            executing_tool(
                Arc::clone(&provider),
                tool_name.clone(),
                args,
                Arc::clone(&executions),
            ),
        )
        .await
        .unwrap_or_else(|error| panic!("{tier:?} {tool_name} live execution failed: {error}"));
    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = &first else {
        panic!("{tier:?} {tool_name} returned the wrong effect outcome");
    };
    let lash_core::ToolAttemptLaunch::Done { record } = launch.as_ref() else {
        panic!("{tier:?} {tool_name} did not finish inline");
    };
    assert!(
        record.output.is_success(),
        "{tier:?} {tool_name} did not succeed"
    );

    start_replay();
    let replayed = controller
        .execute_effect(envelope, replay_must_not_execute(Arc::clone(&executions)))
        .await
        .unwrap_or_else(|error| panic!("{tier:?} {tool_name} replay failed: {error}"));
    assert_eq!(
        serde_json::to_value(replayed).expect("serialize replayed outcome"),
        serde_json::to_value(first).expect("serialize live outcome"),
        "{tier:?} {tool_name} replay must reproduce the recorded result"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "{tier:?} {tool_name} must execute exactly once across live and replay"
    );
}

#[tokio::test]
async fn every_registered_first_party_tool_succeeds_and_replays_in_every_context() {
    let provider: Arc<dyn ToolProvider> =
        Arc::new(lash_llm_tools::llm_query_provider(None, None, None));
    let manifests = provider.tool_manifests();
    assert!(
        !manifests.is_empty(),
        "the first-party tool registry must not be empty"
    );

    for manifest in manifests {
        let inline = lash_sqlite_store::SqliteRuntimeEffectController::memory(
            ExecutionScope::turn("tool-context-conformance", "inline"),
        )
        .await
        .expect("in-process replay controller");
        assert_cell(
            &inline,
            || inline.start_replay(),
            DurabilityTier::Inline,
            Arc::clone(&provider),
            manifest.name.clone(),
        )
        .await;

        let context = Arc::new(ReplayableRecordingContext::default());
        let durable = RestateRuntimeEffectController::new(Arc::clone(&context));
        assert_cell(
            &durable,
            || context.start_replay(),
            DurabilityTier::Durable,
            Arc::clone(&provider),
            manifest.name,
        )
        .await;
    }
}
