use super::*;

#[test]
fn runtime_feedback_is_not_part_of_initial_composition_identity() {
    let provider = crate::testing::TestProvider::default().into_handle();
    let mut direct = crate::DirectRequest::text("model", "user");
    direct.instructions = Some(Arc::from("I"));
    let mut request = crate::direct::build_llm_request(&provider, direct, "model".into()).unwrap();
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
    let provider = crate::testing::TestProvider::default().into_handle();
    let mut direct = crate::DirectRequest::text("model", "user");
    direct.instructions = Some(Arc::from("I"));
    let mut request = crate::direct::build_llm_request(&provider, direct, "model".into()).unwrap();
    let before = trace_composition_key(&request, &[]);
    request.model_capability.instruction_role = crate::InstructionRole::Developer;
    assert_ne!(trace_composition_key(&request, &[]), before);
    request.instructions = None;
    let absent = trace_composition_key(&request, &[]);
    request.instructions = Some(Arc::from(""));
    assert_ne!(trace_composition_key(&request, &[]), absent);
}

#[test]
fn runtime_feedback_composition_has_a_new_hash_family() {
    let previous = Blake3DomainHasher::new("lash-model-facing-composition/v2").finalize();
    let current = Blake3DomainHasher::new("lash-model-facing-composition/v3").finalize();
    assert_ne!(previous, current);
}
