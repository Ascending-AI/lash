use super::*;

#[derive(Default)]
struct JournaledCommitController<const ENGINE: bool> {
    native: lash_core::facade_support::NativeRuntimeEffectController,
}

#[async_trait::async_trait]
impl<const ENGINE: bool> lash_core::AwaitEventResolver for JournaledCommitController<ENGINE> {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(format!("commit-controller:{:p}", &self.native))
    }

    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }
    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }
    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, RuntimeError> {
        self.native.peek_await_event(key).await
    }
    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, RuntimeError> {
        self.native.await_await_event(key, cancel, deadline).await
    }
}

#[async_trait::async_trait]
impl<const ENGINE: bool> lash_core::RuntimeEffectController for JournaledCommitController<ENGINE> {
    fn owns_commit_backpressure(&self) -> bool {
        ENGINE
    }
    async fn turn_control_participation(
        &self,
    ) -> Result<lash_core::TurnControlParticipation, RuntimeError> {
        Ok(lash_core::TurnControlParticipation::DurableJournaled)
    }
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        self.native.execute_effect(envelope, local_executor).await
    }
}

async fn assert_commit_placement(
    session_id: &SessionId,
    controller: Arc<dyn lash_core::RuntimeEffectController>,
    expected_entries: usize,
) {
    let store = Arc::new(RecordingStore::default());
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "committed".into(),
                response_meta: None,
            }],
            ..LlmResponse::default()
        }),
    }]);
    let host = match controller.turn_control_participation().await.unwrap() {
        lash_core::TurnControlParticipation::Local => {
            let mut config = test_runtime_host_config();
            config.control.effect_host = Arc::new(
                lash_core::facade_support::NativeEffectHost::new(controller.clone()),
            );
            EmbeddedRuntimeHost::new(config)
        }
        lash_core::TurnControlParticipation::DurableJournaled => {
            journal_replay_host(controller.clone())
        }
    };
    let mut runtime = TestRuntime::new(transport)
        .host(host)
        .store(store.clone())
        .with_session_id(session_id)
        .build()
        .await;
    let _ = lash_core::runtime::commit_admission::take_product_commit_admission_observations(
        session_id,
    );
    runtime
        .run_turn_assembled(
            TurnInput::text("commit"),
            CancellationToken::new(),
            lash_core::ScopedEffectController::shared(
                controller,
                lash_core::ExecutionScope::turn(session_id, "placement-turn"),
            )
            .unwrap(),
        )
        .await
        .expect("commit real turn");
    let observations =
        lash_core::runtime::commit_admission::take_product_commit_admission_observations(
            session_id,
        );
    assert_eq!(
        observations.len(),
        expected_entries,
        "turn commit coordinator entries"
    );
    enqueue_config_patch_command(
        store.as_ref(),
        session_id,
        lash_core::runtime::ApplyConfigPatch {
            model: Some(
                lash_core::ModelSpec::builder("placement-model")
                    .context_window_tokens(32_000)
                    .build()
                    .unwrap(),
            ),
            ..lash_core::runtime::ApplyConfigPatch::default()
        },
    )
    .await;
    let lease = lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        session_id,
        &lease_owner(session_id),
        "placement-command",
        lash_core::facade_support::LeaseTimings::default().ttl_ms(),
    )
    .await
    .unwrap()
    .acquired()
    .unwrap();
    runtime
        .drain_next_session_command(&lease.fence())
        .await
        .unwrap()
        .expect("command receipt");
    let observations =
        lash_core::runtime::commit_admission::take_product_commit_admission_observations(
            session_id,
        );
    assert_eq!(
        observations.len(),
        expected_entries,
        "session command coordinator entries"
    );
}

#[tokio::test]
async fn durable_journaled_engine_commits_bypass_local_admission() {
    assert_commit_placement(
        &SessionId::from("engine-commit-placement"),
        Arc::new(JournaledCommitController::<true>::default()),
        0,
    )
    .await;
}

#[tokio::test]
async fn native_commits_enter_local_admission() {
    assert_commit_placement(
        &SessionId::from("native-commit-placement"),
        Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
        1,
    )
    .await;
}

#[tokio::test]
async fn store_journaled_commits_keep_native_admission() {
    assert_commit_placement(
        &SessionId::from("store-journaled-commit-placement"),
        Arc::new(JournaledCommitController::<false>::default()),
        1,
    )
    .await;
}

#[test]
fn commit_admission_ownership_survives_controller_wrappers() {
    let controllers: [Arc<dyn lash_core::RuntimeEffectController>; 2] = [
        Arc::new(JournaledCommitController::<true>::default()),
        Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
    ];
    for controller in controllers {
        let expected = controller.owns_commit_backpressure();
        let host = lash_core::facade_support::NativeEffectHost::new(controller);
        assert_eq!(
            lash_core::RuntimeEffectController::owns_commit_backpressure(&host),
            expected
        );
        let (proxy, _requests) = lash_core::runtime::effect::EffectTaskController::scoped(
            &host,
            lash_core::ExecutionScope::turn("ownership", "turn"),
        )
        .unwrap();
        assert_eq!(proxy.controller().owns_commit_backpressure(), expected);
    }
}

#[tokio::test]
async fn invocation_controller_owns_session_command_admission_with_a_native_host() {
    let session_id = "invocation-command-placement";
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store_for_session(
        mock_provider(Vec::new()),
        &SessionId::from(session_id),
    )
    .await;
    enqueue_config_patch_command(
        store.as_ref(),
        &SessionId::from(session_id),
        lash_core::runtime::ApplyConfigPatch {
            model: Some(
                lash_core::ModelSpec::builder("engine-command-model")
                    .context_window_tokens(32_000)
                    .build()
                    .unwrap(),
            ),
            ..lash_core::runtime::ApplyConfigPatch::default()
        },
    )
    .await;
    let lease = lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from(session_id),
        &lease_owner(session_id),
        "invocation-command",
        lash_core::facade_support::LeaseTimings::default().ttl_ms(),
    )
    .await
    .unwrap()
    .acquired()
    .unwrap();
    let controller = JournaledCommitController::<true>::default();
    runtime
        .drain_next_session_command_with_cancellation(
            &lease.fence(),
            CancellationToken::new(),
            &controller,
        )
        .await
        .unwrap()
        .expect("engine-owned command committed");
    let observations =
        lash_core::runtime::commit_admission::take_product_commit_admission_observations(
            &SessionId::from(session_id),
        );
    assert!(
        observations.is_empty(),
        "the invocation owns command backpressure: {observations:?}"
    );
}
