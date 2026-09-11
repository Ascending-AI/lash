use std::sync::Arc;

use crate::{
    RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectKind, RuntimeEffectLocalExecutor,
};

use super::effect::RecordingEffectController;

#[tokio::test]
async fn values_are_sampled_once_and_replayed_by_effect_id() {
    let recorder = RecordingEffectController::default().with_replay_by_key();
    let clock = Arc::new(crate::testing::TestClock::new(1_234));
    let invocation = RuntimeEffectInvocation::new(
        crate::EffectAddress::new(
            crate::ExecutionScope::runtime_operation("typescript-runtime-test"),
            "typescript.runtime:date-now:0",
        )
        .expect("valid language runtime address"),
        crate::RuntimeAttribution::none(),
        "typescript.runtime:date-now:0",
    );
    let command = RuntimeEffectCommand::LanguageRuntimeValue {
        operation: "now".to_string(),
    };

    let first = recorder
        .execute_effect(
            RuntimeEffectEnvelope::new(invocation.clone(), command.clone()),
            RuntimeEffectLocalExecutor::language_runtime_value(clock.clone()),
        )
        .await
        .expect("first sample")
        .into_language_runtime_value()
        .expect("language runtime outcome");
    clock.set(9_999);
    let replay = recorder
        .execute_effect(
            RuntimeEffectEnvelope::new(invocation, command),
            RuntimeEffectLocalExecutor::language_runtime_value(clock),
        )
        .await
        .expect("replay")
        .into_language_runtime_value()
        .expect("language runtime outcome");

    assert_eq!(first, serde_json::json!(1_234));
    assert_eq!(replay, first);
    assert_eq!(recorder.envelopes().len(), 1);
    assert_eq!(
        recorder.count_kind(RuntimeEffectKind::LanguageRuntimeValue),
        1
    );
}
