use super::tests::{projection_test_config, projector};
use super::*;

#[test]
fn runtime_feedback_projectors_trim_configured_instructions() {
    for (prompt, expected) in [
        (" \n\t ", None),
        ("  configured prompt\n", Some("configured prompt")),
    ] {
        let config = projection_test_config("test-model", Default::default(), None);
        let environment = lash_core::sansio::ExecutionEnvironmentSync {
            system_prompt: Arc::from(prompt),
            ..Default::default()
        };
        let messages = lash_core::facade_support::MessageSequence::default();
        let context = || ProjectorContext {
            config: &config,
            messages: &messages,
            events: &[],
            turn_causes: &[],
            protocol_iteration: 0,
            use_tools: false,
            environment: &environment,
        };
        let rlm = projector(1000).project(context());
        let chat = lash_core::sansio::ChatContextProjector.project(context());
        assert_eq!(rlm.instructions.as_deref(), expected);
        assert_eq!(chat.instructions.as_deref(), expected);
    }
}
