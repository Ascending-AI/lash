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

#[derive(Default)]
struct ToolAttemptInvariantRecorder {
    tool_attempt_envelopes: Mutex<Vec<String>>,
    provider_body_invocations: Mutex<Vec<String>>,
    /// Every `Sleep` envelope the layer saw, with its group key when the
    /// envelope carried membership. Restate dispatch resolves a wait child's
    /// membership from the group's durable record and drops `envelope.group`
    /// before executing the wait on the child's own journal, so a grouped
    /// timer arrives as a plain effect — it reaching the layer at all is what
    /// proves the child was routed through the host's stack.
    sleep_envelopes: Mutex<Vec<Option<String>>>,
}

impl ToolAttemptInvariantRecorder {
    fn record_tool_attempt(&self, tool_name: &str) {
        self.tool_attempt_envelopes
            .lock_recover()
            .push(tool_name.to_string());
    }

    fn record_provider_body_invocation(&self, tool_name: &str) {
        self.provider_body_invocations
            .lock_recover()
            .push(tool_name.to_string());
    }

    fn record_sleep(&self, group_key: Option<String>) {
        self.sleep_envelopes.lock_recover().push(group_key);
    }

    /// `provider_tools` names the tools this recorder's `ToolProvider` owns.
    /// Plugin tools -- `start_process` and friends, which the process surface
    /// installs -- cross the same effect boundary but execute inside the
    /// runtime, so they carry a ToolAttempt envelope with no provider body.
    /// Scoping the comparison to the provider's own tools keeps the invariant
    /// exact in both directions for every tool it can actually speak for.
    fn assert_every_provider_invocation_has_tool_attempt_envelope(&self, provider_tools: &[&str]) {
        let counts = |values: &Mutex<Vec<String>>| {
            let mut counts = HashMap::new();
            for value in values.lock_recover().iter() {
                if !provider_tools.contains(&value.as_str()) {
                    continue;
                }
                *counts.entry(value.clone()).or_insert(0usize) += 1;
            }
            counts
        };
        let provider_body_invocations = counts(&self.provider_body_invocations);
        let tool_attempt_envelopes = counts(&self.tool_attempt_envelopes);
        assert!(
            !provider_body_invocations.is_empty(),
            "the probe must invoke at least one provider tool body, or the invariant is vacuous"
        );
        assert_eq!(
            tool_attempt_envelopes, provider_body_invocations,
            "every provider-body invocation must have a corresponding ToolAttempt envelope; \
             provider_body_invocations={provider_body_invocations:?}, \
             tool_attempt_envelopes={tool_attempt_envelopes:?}"
        );
    }
}

/// Records every tool attempt — and every `Sleep` — that crosses the effect
/// boundary of the contract world's host.
struct ToolAttemptRecordingLayer {
    recorder: Arc<ToolAttemptInvariantRecorder>,
}

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for ToolAttemptRecordingLayer {
    async fn execute_effect(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        match &envelope.command {
            lash_core::RuntimeEffectCommand::ToolAttempt { call, .. } => {
                self.recorder.record_tool_attempt(&call.tool_name);
            }
            lash_core::RuntimeEffectCommand::Sleep { .. } => {
                self.recorder
                    .record_sleep(envelope.group.as_ref().map(|group| group.group_key.clone()));
            }
            _ => {}
        }
        inner.execute_effect(envelope, local_executor).await
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

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        self.delegate.attempt_may_defer(tool_id)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.recorder.record_provider_body_invocation(call.name());
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
        recording_layer(Arc::clone(&recorder)),
        &mut key_rx,
    )
    .await
    .expect("existing scalar Lashlang Pending contract");

    recorder.assert_every_provider_invocation_has_tool_attempt_envelope(&["mock_input_request"]);
}

#[tokio::test]
async fn batched_lashlang_provider_invocations_cross_tool_attempt_effect_boundary() {
    let recorder = Arc::new(ToolAttemptInvariantRecorder::default());
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(RecordingToolProvider {
        recorder: Arc::clone(&recorder),
        delegate: Arc::new(BatchEnvelopeProbeTools),
    });
    let (core, _, engine) = agent_process_contract_core_with_effect_layer(
        "lash_runtime batched tool attempt envelope",
        vec![
            r#"<typescript>
const collect = async () => {
  const [first, second] = await Promise.all([
    tools.envelope_probe({ value: "a" }),
    tools.envelope_probe({ value: "b" })
  ]);
  return { first: first, second: second };
};
const handle = await processes.start({ definition: collect });
finish(await handle);
</typescript>"#,
        ],
        Some(tools),
        recording_layer(Arc::clone(&recorder)),
    )
    .await
    .expect("build batch envelope contract");
    let session = core
        .session("sim-agent-batched-tool-attempt-envelope")
        .open()
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
    recorder.assert_every_provider_invocation_has_tool_attempt_envelope(&["envelope_probe"]);
}

#[tokio::test]
async fn lashlang_race_timer_child_crosses_the_host_layer() {
    let recorder = Arc::new(ToolAttemptInvariantRecorder::default());
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(RecordingToolProvider {
        recorder: Arc::clone(&recorder),
        delegate: Arc::new(BatchEnvelopeProbeTools),
    });
    let (core, _, engine) = agent_process_contract_core_with_effect_layer(
        "lash_runtime race timer child layer",
        vec![
            r#"<typescript>
const winner = await Promise.race([tools.envelope_probe({ value: "a" }), sleep(60000)]);
finish(winner);
</typescript>"#,
        ],
        Some(tools),
        recording_layer(Arc::clone(&recorder)),
    )
    .await
    .expect("build race timer contract");
    let session = core
        .session("sim-agent-race-timer-child-layer")
        .open()
        .await
        .expect("open race timer contract session");

    let outcome = engine
        .run_turn(
            &session,
            "sim-agent-race-timer-child-layer-turn",
            Arc::new(RuntimeProofRecordingEvents::default()),
            contract_turn("Race a tool call against a timer."),
        )
        .await
        .expect("run race timer contract handler");

    // The race's timer is a group child: its Sleep envelope reaches the layer
    // only because Restate dispatch routes wait children through the host.
    let tool_attempt_envelopes = recorder.tool_attempt_envelopes.lock_recover().clone();
    assert_eq!(
        tool_attempt_envelopes.as_slice(),
        ["envelope_probe"],
        "the race must record exactly one envelope_probe ToolAttempt; \
         tool_attempt_envelopes={tool_attempt_envelopes:?}"
    );
    assert!(
        !recorder.sleep_envelopes.lock_recover().is_empty(),
        "the race's timer child crossed no Sleep envelope through the host layer"
    );

    // Pre-existing Restate-tier gap, unrelated to the layer (the same turn
    // fails identically with no layer installed): the opener's end closes
    // the group under `Cancel` and then awaits the loser's rank with a
    // caller read, which the effect-group index has refused since FIG-3676
    // (#2135) unless the group was reopened — and a turn's journaled `open`
    // never re-executes inside its one invocation, so `reopened` stays
    // unset. Once the opener end reads post-close ranks as a host reader
    // (or the group is reopened before it), the turn succeeds and this
    // assertion flips to `result.is_success()`.
    let error = match outcome {
        Err(lash::EmbedError::Runtime(error)) => error,
        Ok(output) => panic!(
            "the opener end's post-close rank read was repaired; \
             assert the turn succeeds instead: {output:?}"
        ),
        Err(error) => panic!("the race turn failed on something new: {error:?}"),
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RuntimeEffectGroupShape,
        "the race turn fails only at the opener end's post-close rank read: {error:?}"
    );
}
