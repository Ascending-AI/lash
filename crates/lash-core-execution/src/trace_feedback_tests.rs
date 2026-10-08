use super::*;
use std::sync::Arc;

fn recorded_model() -> crate::LlmProfileConfig {
    crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
        crate::LlmProfileKey::new("trace-test"),
        crate::LlmProfileMetadata::builder("model")
            .cache_retention(crate::provider::CacheRetention::Short)
            .context_window_tokens(128000)
            .build()
            .expect("valid profile"),
    ))
}

#[test]
fn runtime_feedback_is_not_part_of_initial_composition_identity() {
    let mut request =
        crate::direct::build_llm_request(crate::DirectRequest::text("user"), recorded_model())
            .unwrap();
    request.instructions = Some(Arc::from("I"));
    let before = trace_composition_key(&request, &[]);
    request.messages.insert(
        0,
        crate::llm::types::LlmMessage::text(LlmRole::System, "leading feedback"),
    );
    request.messages.push(crate::llm::types::LlmMessage::text(
        LlmRole::System,
        "retry feedback",
    ));
    assert_eq!(trace_composition_key(&request, &[]), before);
    assert_eq!(
        trace_composition_snapshot(&request, before).rendered_system_prompt,
        "I"
    );
    request.instructions = Some(Arc::from("changed"));
    assert_ne!(trace_composition_key(&request, &[]), before);
}

#[test]
fn runtime_feedback_composition_identity_includes_instruction_authority() {
    let mut request =
        crate::direct::build_llm_request(crate::DirectRequest::text("user"), recorded_model())
            .unwrap();
    request.instructions = Some(Arc::from("I"));
    let before = trace_composition_key(&request, &[]);
    request.model.metadata_mut().capability.instruction_role = crate::InstructionRole::Developer;
    assert_ne!(trace_composition_key(&request, &[]), before);
    request.instructions = None;
    let absent = trace_composition_key(&request, &[]);
    request.instructions = Some(Arc::from(""));
    assert_ne!(trace_composition_key(&request, &[]), absent);
}

/// S14 F2: a physical-turn trace parent never becomes a tool's durable owner.
#[test]
fn tool_scope_owner_stays_on_the_admitted_run_across_physical_turns() {
    use lash_trace::{
        DurableTraceScope, TraceAnchor, TraceCause, TraceScopeId, TraceScopeOwner, TraceToolOwner,
    };
    let call_id = crate::ToolCallId::fixture("same-call");
    let opener = crate::EffectOpener::turn("session", "logical-run");
    let owner = TraceToolOwner::from(&opener);
    let mut scopes = Vec::new();
    for turn_id in ["logical-run", "logical-run:follow-on:1"] {
        let parent = DurableTraceScope {
            scope: TraceScopeId::admission(TraceScopeOwner::Turn {
                session_id: "session".into(),
                turn_id: turn_id.into(),
            }),
            cause: TraceCause::Root,
            anchor: TraceAnchor::Untraced,
            started_at_ms: 1,
        };
        let scope = tool_trace_scope(&opener, Some(&parent), &call_id, 2);
        assert_eq!(
            scope.scope.owner,
            TraceScopeOwner::Tool {
                owner: owner.clone(),
                call_id: call_id.to_string(),
            }
        );
        scopes.push(scope.scope);
    }
    assert_eq!(scopes[0], scopes[1]);
    let parent = DurableTraceScope {
        scope: TraceScopeId::admission(TraceScopeOwner::Process {
            process_id: crate::process_id_for_test("parent"),
        }),
        cause: TraceCause::Root,
        anchor: TraceAnchor::Untraced,
        started_at_ms: 1,
    };
    assert_eq!(
        tool_trace_scope(&opener, Some(&parent), &call_id, 2).scope,
        scopes[0]
    );
    assert_eq!(
        tool_trace_scope(&opener, None, &call_id, 2).scope,
        scopes[0]
    );
    assert!(
        serde_json::from_value::<TraceToolOwner>(serde_json::json!({
            "kind": "run", "session_id": "session", "run": "logical-run"
        }))
        .is_err()
    );
}
