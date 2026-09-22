use super::effect::{RejectingEffectController, runtime_host_config_with_native_controller};
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
impl AwaitEventResolver for ProxyPumpingReplayMismatchController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.rejecting.await_event_authority_binding_id()
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.rejecting.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.rejecting.resolve_await_event(key, resolution).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for ProxyPumpingReplayMismatchController {
    async fn runtime_effect_failure_disposition(
        &self,
        code: RuntimeErrorCode,
    ) -> Result<RuntimeEffectFailureDisposition, RuntimeError> {
        self.rejecting
            .runtime_effect_failure_disposition(code)
            .await
    }

    async fn turn_control_participation(&self) -> Result<TurnControlParticipation, RuntimeError> {
        self.rejecting.turn_control_participation().await
    }

    async fn execute_effect(
        &self,
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
        self.rejecting
            .execute_effect(envelope, local_executor)
            .await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.rejecting.open_effect_group(group).await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.rejecting.await_next_settlement(handle, cancel).await
    }
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.rejecting.read_group_settlement(group_key, rank).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.rejecting.close_effect_group(handle, disposition).await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.rejecting.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.rejecting
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

#[tokio::test]
async fn controller_owned_replay_mismatch_reaches_host_with_structured_summary() {
    let controller = Arc::new(RejectingEffectController::default().with_replay_mismatch());
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_native_controller(
            controller.clone(),
        )),
    )
    .await;

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("hello"),
            CancellationToken::new(),
            ScopedEffectController::shared(
                controller,
                AdmittedScope::turn("root", "replay-mismatch-controller"),
            )
            .expect("replay-mismatch execution scope"),
        )
        .await
        .expect_err("controller-owned replay mismatch must abort to the host");

    assert!(
        error.code.is_replay_mismatch(),
        "controller-owned replay mismatch must retain its typed boundary code"
    );
    assert_eq!(
        error.summary,
        Some(RuntimeEffectReplayMismatchReport {
            divergent_path_count: 1,
            first_divergent_paths: vec!["command.request.model".to_string()],
        }),
        "the outer host error must retain the controller's structured divergence summary"
    );
}

#[tokio::test]
async fn proxied_controller_owned_replay_mismatch_aborts_with_structured_summary() {
    let controller = Arc::new(ProxyPumpingReplayMismatchController::new());
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_native_controller(
            controller.clone(),
        )),
    )
    .await;
    let (proxy, requests) = lash_core::runtime::effect::EffectTaskController::scoped(
        controller.as_ref(),
        AdmittedScope::turn("root", "proxied-replay-mismatch-controller"),
    )
    .expect("proxied replay-mismatch execution scope");
    let controller_task = lash_core::task::spawn({
        let controller = Arc::clone(&controller);
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

    let error =
        result.expect_err("proxied controller-owned replay mismatch must abort to the host");
    assert!(
        error.code.is_replay_mismatch(),
        "proxied controller-owned replay mismatch must retain its typed boundary code: {error:?}"
    );
    assert_eq!(
        error.summary,
        Some(RuntimeEffectReplayMismatchReport {
            divergent_path_count: 1,
            first_divergent_paths: vec!["command.request.model".to_string()],
        }),
        "the outer host error must retain the proxied controller's structured divergence summary"
    );
}
