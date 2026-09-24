use std::sync::Arc;

use lash_core::{
    RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectKind, RuntimeEffectLocalExecutor,
};

use super::effect::{RecordingEffectController, layered_controller};

#[tokio::test]
async fn values_are_sampled_once_and_replayed_by_effect_id() {
    let backend = super::memory_backend().await;
    let recorder = RecordingEffectController::default().with_replay_by_key();
    let controller = layered_controller(
        &backend,
        Arc::new(recorder.clone()),
        lash_core::AdmittedScope::runtime_operation("typescript-runtime-test"),
    );
    let clock = Arc::new(lash_core::testing::TestClock::new(1_234));
    let invocation = RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            lash_core::ExecutionScope::runtime_operation("typescript-runtime-test"),
            "typescript.runtime:date-now:0",
        )
        .expect("valid language runtime address"),
        lash_core::RuntimeAttribution::none(),
        "typescript.runtime:date-now:0",
    );
    let command = RuntimeEffectCommand::LanguageRuntimeValue {
        operation: "now".to_string(),
    };

    let first = controller
        .execute_effect(
            RuntimeEffectEnvelope::new(invocation.clone(), command.clone()),
            RuntimeEffectLocalExecutor::language_runtime_value(clock.clone()),
        )
        .await
        .expect("first sample")
        .into_language_runtime_value()
        .expect("language runtime outcome");
    clock.set(9_999);
    let replay = controller
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
