//! Effect-boundary invariant tests for scalar vs batched Lashlang tool dispatch.

use super::*;
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;

#[test]
fn process_effect_outcome_contract_normalizes_only_opaque_replay_identity() {
    let first = json!({"replay_key": "first", "node_id": "node:a", "outcome_class": "success"});
    let second = json!({"replay_key": "second", "node_id": "node:a", "outcome_class": "success"});
    let changed_outcome =
        json!({"replay_key": "second", "node_id": "node:a", "outcome_class": "failure"});

    assert_eq!(
        normalize_contract_process_event_payload("process.effect_outcome", first.clone()),
        normalize_contract_process_event_payload("process.effect_outcome", second)
    );
    assert_ne!(
        normalize_contract_process_event_payload("process.effect_outcome", first.clone()),
        normalize_contract_process_event_payload("process.effect_outcome", changed_outcome)
    );
    assert_eq!(
        normalize_contract_process_event_payload("process.completed", first.clone()),
        first
    );
}

// FIG-1863 (c932ead875) moved provider execution from ToolAttempt envelopes
// into native X. Compare the acknowledged X identities, not the retired seam.
#[derive(Default)]
struct ToolAttemptInvariantRecorder {
    recorded_attempts: Mutex<Vec<(lash_core::ToolCallId, u32)>>,
    provider_body_invocations: Mutex<Vec<(lash_core::ToolCallId, u32, String)>>,
}

impl ToolAttemptInvariantRecorder {
    fn record_tool_attempt(&self, call_id: lash_core::ToolCallId, attempt: u32) {
        self.recorded_attempts
            .lock_recover()
            .push((call_id, attempt));
    }

    fn record_provider_body_invocation(&self, call: &lash_core::ToolCall<'_>) {
        self.provider_body_invocations.lock_recover().push((
            call.context.call_id().clone(),
            call.context.attempt_number(),
            call.name().to_string(),
        ));
    }

    /// Plugin-owned calls also record X. Restrict both sides to the admitted
    /// call identities the provider owns, retaining an exact comparison in
    /// both directions for each call and attempt ordinal.
    fn assert_every_provider_invocation_has_recorded_attempt(&self, provider_tools: &[&str]) {
        let mut provider_body_invocations = HashMap::new();
        for (call_id, attempt, name) in self.provider_body_invocations.lock_recover().iter() {
            if provider_tools.contains(&name.as_str()) {
                *provider_body_invocations
                    .entry((call_id.clone(), *attempt))
                    .or_insert(0usize) += 1;
            }
        }
        let mut recorded_attempts = HashMap::new();
        for key in self.recorded_attempts.lock_recover().iter() {
            if provider_body_invocations.keys().any(|(id, _)| id == &key.0) {
                *recorded_attempts.entry(key.clone()).or_insert(0usize) += 1;
            }
        }
        assert!(
            !provider_body_invocations.is_empty(),
            "the probe must invoke at least one provider tool body, or the invariant is vacuous"
        );
        assert_eq!(
            recorded_attempts, provider_body_invocations,
            "every provider-body invocation must have a corresponding recorded X attempt; \
             provider_body_invocations={provider_body_invocations:?}, \
             recorded_attempts={recorded_attempts:?}"
        );
    }
}

/// Records acknowledged X attempts through the contract world's host layer.
struct ToolAttemptRecordingLayer {
    recorder: Arc<ToolAttemptInvariantRecorder>,
}

impl lash_core::testing::EffectLayer for ToolAttemptRecordingLayer {
    fn start_run_attempt<'run>(
        &'run self,
        inner: &'run dyn lash_core::RuntimeEffectController,
        name: String,
        step: lash_core::tool_dispatch::RunAttemptStep<'run>,
    ) -> lash_core::tool_dispatch::RunAttemptHandle<'run> {
        let handle = inner.start_run_attempt(name, step);
        lash_core::tool_dispatch::RunAttemptHandle {
            body: handle.body,
            result: Box::pin(async move {
                let entry = handle.result.await?;
                self.recorder
                    .record_tool_attempt(entry.call_id.clone(), entry.attempt.get());
                Ok(entry)
            }),
        }
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.recorder.record_provider_body_invocation(&call);
        self.delegate.execute(call).await
    }
}

/// The recording layer the contract world runs over its own host — the one
/// every other agent contract runs on.
fn recording_layer(
    recorder: Arc<ToolAttemptInvariantRecorder>,
) -> Option<Arc<dyn lash_core::testing::EffectLayer>> {
    Some(Arc::new(ToolAttemptRecordingLayer { recorder }))
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
        .expect("valid declared tool schemas")
        .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async { lash_core::ToolOutcome::ok(json!({ "value": call.args["value"].clone() })) })
            .await
            .into()
    }
}

#[tokio::test]
async fn scalar_lashlang_pending_provider_invocation_crosses_native_recorded_attempt_boundary() {
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
        recording_layer(Arc::clone(&recorder)),
        &mut key_rx,
    )
    .await
    .expect("existing scalar Lashlang Pending contract");

    recorder.assert_every_provider_invocation_has_recorded_attempt(&["mock_input_request"]);
}

#[tokio::test]
async fn batched_lashlang_provider_invocations_cross_native_recorded_attempt_boundary() {
    let recorder = Arc::new(ToolAttemptInvariantRecorder::default());
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(RecordingToolProvider {
        recorder: Arc::clone(&recorder),
        delegate: Arc::new(BatchEnvelopeProbeTools),
    });
    let (core, _, engine) = agent_process_contract_core_with_effect_layer(
        "lash_runtime batched tool attempt envelope",
        vec![
            r#"<typescript>
const collect = await processes.create({ dialect: "typescript", source: `const collect = async () => {
  const [first, second] = await Promise.all([
    tools.envelope_probe({ value: "a" }),
    tools.envelope_probe({ value: "b" })
  ]);
  return { first: first, second: second };
};` });
const handle = await processes.start({ definition: collect });
finish(await handle);
</typescript>"#,
        ],
        Some(tools),
        recording_layer(Arc::clone(&recorder)),
    )
    .await
    .expect("build batch envelope contract");
    let session = crate::open_created_session(
        "lash_runtime batched tool attempt envelope",
        &core,
        "sim-agent-batched-tool-attempt-envelope",
    )
    .await
    .expect("open batch envelope contract session");

    let result = engine
        .run_turn(
            &session,
            "sim-agent-batched-tool-attempt-envelope-turn",
            Arc::new(RuntimeProofRecordingEvents::default()),
            contract_turn("Run the batched tool probe."),
        )
        .await
        .expect("run batch envelope contract handler")
        .expect("run batch envelope contract")
        .result;
    assert!(
        result.is_success(),
        "batch envelope contract failed: {result:?}"
    );
    recorder.assert_every_provider_invocation_has_recorded_attempt(&["envelope_probe"]);
}

/// A scalar tool call inside a process runs on the segment's own controller,
/// which the engine's handler minted: its X crosses the host layer because
/// the durable worker routes that controller through the host (FIG-3738).
#[tokio::test]
async fn scalar_lashlang_process_segment_tool_call_crosses_native_recorded_attempt_boundary() {
    let recorder = Arc::new(ToolAttemptInvariantRecorder::default());
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(RecordingToolProvider {
        recorder: Arc::clone(&recorder),
        delegate: Arc::new(BatchEnvelopeProbeTools),
    });
    let (core, _, engine) = agent_process_contract_core_with_effect_layer(
        "lash_runtime process segment tool attempt envelope",
        vec![
            r#"<typescript>
const collect = await processes.create({ dialect: "typescript", source: `const collect = async () => {
  const first = await tools.envelope_probe({ value: "a" });
  return { first: first };
};` });
const handle = await processes.start({ definition: collect });
finish(await handle);
</typescript>"#,
        ],
        Some(tools),
        recording_layer(Arc::clone(&recorder)),
    )
    .await
    .expect("build segment envelope contract");
    let session = crate::open_created_session(
        "lash_runtime process segment tool attempt envelope",
        &core,
        "sim-agent-segment-tool-attempt-envelope",
    )
    .await
    .expect("open segment envelope contract session");

    let result = engine
        .run_turn(
            &session,
            "sim-agent-segment-tool-attempt-envelope-turn",
            Arc::new(RuntimeProofRecordingEvents::default()),
            contract_turn("Run the process tool probe."),
        )
        .await
        .expect("run segment envelope contract handler")
        .expect("run segment envelope contract")
        .result;
    assert!(
        result.is_success(),
        "segment envelope contract failed: {result:?}"
    );
    recorder.assert_every_provider_invocation_has_recorded_attempt(&["envelope_probe"]);
}

/// A race whose tool wins ends its turn cleanly on Restate.
#[tokio::test]
async fn a_race_turn_on_restate_succeeds() {
    let (core, _, engine) = agent_process_contract_core_with_effect_layer(
        "lash_runtime race turn",
        vec![
            r#"<typescript>
const winner = await Promise.race([tools.envelope_probe({ value: "a" }), sleep(60000)]);
finish(winner);
</typescript>"#,
        ],
        Some(Arc::new(BatchEnvelopeProbeTools) as Arc<dyn lash_core::ToolProvider>),
        None,
    )
    .await
    .expect("build race turn contract");
    let session =
        crate::open_created_session("lash_runtime race turn", &core, "sim-agent-race-turn")
            .await
            .expect("open race turn contract session");
    let result = engine
        .run_turn(
            &session,
            "sim-agent-race-turn-turn",
            Arc::new(RuntimeProofRecordingEvents::default()),
            contract_turn("Race a tool call against a timer."),
        )
        .await
        .expect("run race turn contract handler")
        .expect("the race turn completes")
        .result;
    assert!(result.is_success(), "race turn failed: {result:?}");
}
