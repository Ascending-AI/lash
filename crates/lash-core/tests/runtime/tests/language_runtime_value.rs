use std::sync::Arc;

use lash_core::{
    RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectKind, RuntimeEffectLocalExecutor,
};

use super::effect::RecordingEffectController;

const SEED: u64 = 0x5_f509;

#[tokio::test(flavor = "multi_thread")]
async fn values_are_sampled_once_and_replayed_by_effect_id() {
    let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let recorder = RecordingEffectController::default().with_replay_by_key();
    let handler = double
        .open_handler(lash_core::AdmittedScope::runtime_operation(
            "typescript-runtime-test",
        ))
        .await
        .expect("open the operation's handler");
    let scoped = lash_core::testing::LayeredEffectHost::layer_scoped(
        handler.scoped(),
        Arc::new(recorder.clone()),
    )
    .expect("layer the lent controller with the recorder");
    let controller = scoped.controller();
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
    drop(scoped);
    handler
        .close()
        .await
        .expect("close the operation's handler");

    assert_eq!(first, serde_json::json!(1_234));
    assert_eq!(replay, first);
    assert_eq!(recorder.envelopes().len(), 1);
    assert_eq!(
        recorder.count_kind(RuntimeEffectKind::LanguageRuntimeValue),
        1
    );
}
