//! Effect-boundary invariant tests for scalar vs batched Lashlang tool dispatch.

use super::*;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Default)]
struct ToolAttemptInvariantRecorder {
    tool_attempt_envelopes: Mutex<Vec<String>>,
    provider_body_invocations: Mutex<Vec<String>>,
}

impl ToolAttemptInvariantRecorder {
    fn record_tool_attempt(&self, tool_name: &str) {
        self.tool_attempt_envelopes
            .lock()
            .expect("tool attempt envelopes")
            .push(tool_name.to_string());
    }

    fn record_provider_body_invocation(&self, tool_name: &str) {
        self.provider_body_invocations
            .lock()
            .expect("provider body invocations")
            .push(tool_name.to_string());
    }

    fn assert_every_provider_invocation_has_tool_attempt_envelope(&self) {
        let counts = |values: &Mutex<Vec<String>>| {
            let mut counts = HashMap::new();
            for value in values.lock().expect("invariant records").iter() {
                *counts.entry(value.clone()).or_insert(0usize) += 1;
            }
            counts
        };
        let provider_body_invocations = counts(&self.provider_body_invocations);
        let tool_attempt_envelopes = counts(&self.tool_attempt_envelopes);
        assert_eq!(
            tool_attempt_envelopes, provider_body_invocations,
            "every provider-body invocation must have a corresponding ToolAttempt envelope; \
             provider_body_invocations={provider_body_invocations:?}, \
             tool_attempt_envelopes={tool_attempt_envelopes:?}"
        );
    }
}

struct RecordingInlineEffectController {
    recorder: Arc<ToolAttemptInvariantRecorder>,
    delegate: lash_core::InlineRuntimeEffectController,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for RecordingInlineEffectController {
    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.delegate.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.delegate.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.delegate.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: lash::CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.delegate.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), lash_core::RuntimeError> {
        self.delegate
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), lash_core::RuntimeError> {
        self.delegate
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for RecordingInlineEffectController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        if let lash_core::RuntimeEffectCommand::ToolAttempt { call, .. } = &envelope.command {
            self.recorder.record_tool_attempt(&call.tool_name);
        }
        self.delegate.execute_effect(envelope, local_executor).await
    }
}

struct RecordingToolProvider {
    recorder: Arc<ToolAttemptInvariantRecorder>,
    delegate: Arc<dyn lash_core::ToolProvider>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RecordingToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.delegate.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.delegate.resolve_contract(name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        self.recorder.record_provider_body_invocation(call.name);
        self.delegate.execute(call).await
    }
}

fn recording_effect_host(
    recorder: Arc<ToolAttemptInvariantRecorder>,
) -> Arc<dyn lash_core::EffectHost> {
    Arc::new(
        lash_core::InlineEffectHost::new(Arc::new(RecordingInlineEffectController {
            recorder,
            delegate: lash_core::InlineRuntimeEffectController::default(),
        }))
        .allow_process_lifetime_completion_keys(),
    )
}

struct BatchEnvelopeProbeTools;

impl BatchEnvelopeProbeTools {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:envelope_probe",
            "envelope_probe",
            "Return a value while probing the runtime effect boundary.",
            json!({
                "type": "object",
                "properties": { "value": {} },
                "required": ["value"],
                "additionalProperties": false
            }),
            json!({ "type": "object" }),
        )
        .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
            ["tools"],
            "envelope_probe",
        ))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for BatchEnvelopeProbeTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "envelope_probe").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        lash_core::ToolResult::ok(json!({ "value": call.args["value"].clone() }))
    }
}

#[tokio::test]
async fn scalar_lashlang_pending_provider_invocation_crosses_tool_attempt_effect_boundary() {
    let recorder = Arc::new(ToolAttemptInvariantRecorder::default());
    let (key_tx, mut key_rx) =
        tokio::sync::oneshot::channel::<Result<lash_core::AwaitEventKey, String>>();
    let tools = Arc::new(ContractDurableInputTools::new(key_tx));
    let registered_tools: Arc<dyn lash_core::ToolProvider> = Arc::new(RecordingToolProvider {
        recorder: Arc::clone(&recorder),
        delegate: Arc::clone(&tools) as Arc<dyn lash_core::ToolProvider>,
    });

    facade_agent_durable_input_execution_with(
        tools,
        registered_tools,
        recording_effect_host(Arc::clone(&recorder)),
        &mut key_rx,
    )
    .await
    .expect("existing scalar Lashlang Pending contract");

    recorder.assert_every_provider_invocation_has_tool_attempt_envelope();
}

#[tokio::test]
async fn batched_lashlang_provider_invocations_cross_tool_attempt_effect_boundary() {
    let recorder = Arc::new(ToolAttemptInvariantRecorder::default());
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(RecordingToolProvider {
        recorder: Arc::clone(&recorder),
        delegate: Arc::new(BatchEnvelopeProbeTools),
    });
    let (core, _) = agent_process_contract_core_with_effect_host(
        "lash_runtime batched tool attempt envelope",
        vec![
            r#"<lashlang>
process collect(tools: Tools) {
  results = await {
first: tools.envelope_probe({ value: "a" })?,
second: tools.envelope_probe({ value: "b" })?
  }
  finish results
}
handle = start collect(tools: tools)
finish (await handle)?
</lashlang>"#,
        ],
        Some(tools),
        recording_effect_host(Arc::clone(&recorder)),
    )
    .expect("build batch envelope contract");
    let session = core
        .session("sim-agent-batched-tool-attempt-envelope")
        .open()
        .await
        .expect("open batch envelope contract session");

    let result = session
        .turn(lash::TurnInput::text("Run the batched tool probe."))
        .run()
        .await
        .expect("run batch envelope contract");
    assert!(
        result.is_success(),
        "batch envelope contract failed: {result:?}"
    );
    recorder.assert_every_provider_invocation_has_tool_attempt_envelope();
}
