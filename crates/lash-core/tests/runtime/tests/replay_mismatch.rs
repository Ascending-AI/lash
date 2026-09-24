use super::effect::{RejectingEffectController, runtime_host_config_with_effect_layer};
use super::*;

struct ProxyPumpingReplayMismatchController {
    rejecting: RejectingEffectController,
}

impl ProxyPumpingReplayMismatchController {
    fn new() -> Self {
        Self {
            rejecting: RejectingEffectController::default().with_replay_mismatch(),
        }
    }
}

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for ProxyPumpingReplayMismatchController {
    async fn execute_effect(
        &self,
        inner: &dyn RuntimeEffectController,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(
            &envelope.command,
            RuntimeEffectCommand::Sleep {
                spec: lash_core::SleepSpec::For {
                    duration_ms: u64::MAX
                }
            }
        ) {
            return std::future::pending().await;
        }
        lash_core::testing::EffectLayer::execute_effect(
            &self.rejecting,
            inner,
            envelope,
            local_executor,
        )
        .await
    }

    async fn open_effect_group(
        &self,
        inner: &dyn RuntimeEffectController,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        lash_core::testing::EffectLayer::open_effect_group(&self.rejecting, inner, group).await
    }
}

#[tokio::test]
async fn controller_owned_replay_mismatch_parks_the_turn_with_structured_summary() {
    let backend = memory_backend().await;
    let controller = Arc::new(RejectingEffectController::default().with_replay_mismatch());
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
            &backend,
            controller.clone(),
        )),
    )
    .await;

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("hello"),
            CancellationToken::new(),
            super::effect::layered_scope(
                &backend,
                controller,
                AdmittedScope::turn("root", "replay-mismatch-controller"),
            ),
        )
        .await
        .expect_err("a replay divergence parks the turn (FIG-3587)");

    assert_parked_replay_mismatch(&error);
}

/// A replay divergence parks the turn (FIG-3587, superseding FIG-3575's
/// recorded failure): the abort names the typed mismatch code, which parks,
/// and keeps the structured divergence summary.
fn assert_parked_replay_mismatch(error: &RuntimeError) {
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::SqliteEffectReplayHashConflict,
        "{error:?}"
    );
    assert!(error.code.parks_turn(), "a replay hash conflict parks");
    assert_eq!(
        error.summary.as_deref(),
        Some(&RuntimeEffectReplayMismatchReport {
            divergent_path_count: 1,
            first_divergent_paths: vec!["command.request.model".to_string()],
            effect_kind: None,
        })
    );
}

#[tokio::test]
async fn proxied_controller_owned_replay_mismatch_parks_the_turn_with_structured_summary() {
    let backend = memory_backend().await;
    let controller = Arc::new(ProxyPumpingReplayMismatchController::new());
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
            &backend,
            controller.clone(),
        )),
    )
    .await;
    let layered = super::effect::layered_controller(
        &backend,
        controller.clone(),
        AdmittedScope::turn("root", "proxied-replay-mismatch-controller"),
    );
    let (proxy, requests) = lash_core::runtime::effect::EffectTaskController::scoped(
        layered.as_ref(),
        AdmittedScope::turn("root", "proxied-replay-mismatch-controller"),
    )
    .expect("proxied replay-mismatch execution scope");
    let controller_task = lash_core::task::spawn({
        let controller = Arc::clone(&layered);
        async move {
            let pump_scope = ExecutionScope::runtime_operation("proxied-replay-mismatch-pump");
            lash_core::runtime::effect::drive_effect_controller_task(
                controller.as_ref(),
                pump_scope.clone(),
                RuntimeEffectEnvelope::new(
                    RuntimeEffectInvocation::new(
                        EffectAddress::new(pump_scope, "proxy-pump:sleep")
                            .expect("valid proxy pump address"),
                        RuntimeAttribution::none(),
                        "proxy-pump",
                    ),
                    RuntimeEffectCommand::Sleep {
                        spec: lash_core::SleepSpec::For {
                            duration_ms: u64::MAX,
                        },
                    },
                ),
                RuntimeEffectLocalExecutor::unavailable(),
                requests,
            )
            .await
        }
    });

    let result = runtime
        .run_turn_assembled(TurnInput::text("hello"), CancellationToken::new(), proxy)
        .await;
    controller_task.abort();
    let task_error = controller_task
        .await
        .expect_err("proxy controller task must remain alive until explicitly stopped");
    assert!(task_error.is_cancelled());

    let error = result.expect_err("a proxied replay divergence parks the turn too");
    assert_parked_replay_mismatch(&error);
}
