//! Contract tests of the facade-level process harness.

use super::*;

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

/// A race whose tool wins ends its turn cleanly on Restate.
#[tokio::test]
async fn a_race_turn_on_restate_succeeds() {
    let (core, _, engine) = agent_process_contract_core_with_tools(
        "lash_runtime race turn",
        vec![
            r#"<typescript>
const winner = await Promise.race([tools.envelope_probe({ value: "a" }), sleep(60000)]);
finish(winner);
</typescript>"#,
        ],
        Some(Arc::new(BatchEnvelopeProbeTools) as Arc<dyn lash_core::ToolProvider>),
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
