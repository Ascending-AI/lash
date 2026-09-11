use super::*;

#[tokio::test]
async fn sqlite_sleep_replay_returns_after_recorded_due_time() {
    let (_controller_dir, controller) =
        open_ephemeral_effect_controller(durable_turn_scope("session", "turn")).await;
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(durable_turn_scope("session", "turn"), "sleep-key")
                .expect("valid sleep effect address"),
            lash_core::RuntimeAttribution::for_turn("session", "turn", 1, 0),
            "sleep",
        ),
        RuntimeEffectCommand::Sleep { duration_ms: 120 },
    );

    let started = std::time::Instant::now();
    let first = controller
        .execute_effect(envelope.clone(), RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("first sleep");
    assert!(matches!(first, RuntimeEffectOutcome::Sleep));
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(100),
        "first sleep must wait until the recorded due_at"
    );

    controller.start_replay();
    let replayed = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        controller.execute_effect(envelope, failing_executor()),
    )
    .await
    .expect("replay must not sleep the full original duration")
    .expect("sleep replay");
    assert!(matches!(replayed, RuntimeEffectOutcome::Sleep));
}
