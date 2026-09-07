use super::*;

#[derive(Default)]
struct JournaledCommitController<const ENGINE: bool> {
    native: crate::NativeRuntimeEffectController,
}

#[async_trait::async_trait]
impl<const ENGINE: bool> crate::AwaitEventResolver for JournaledCommitController<ENGINE> {
    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }
    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }
    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, RuntimeError> {
        self.native.peek_await_event(key).await
    }
    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<crate::Resolution, RuntimeError> {
        self.native.await_await_event(key, cancel, deadline).await
    }
}

#[async_trait::async_trait]
impl<const ENGINE: bool> crate::RuntimeEffectController for JournaledCommitController<ENGINE> {
    fn owns_commit_backpressure(&self) -> bool {
        ENGINE
    }
    async fn turn_control_participation(
        &self,
    ) -> Result<crate::TurnControlParticipation, RuntimeError> {
        Ok(crate::TurnControlParticipation::DurableJournaled)
    }
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        self.native.execute_effect(envelope, local_executor).await
    }
}

async fn assert_commit_placement(
    session_id: &str,
    controller: Arc<dyn crate::RuntimeEffectController>,
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
    let mut runtime = TestRuntime::new(transport)
        .host(journal_replay_host(controller.clone()))
        .store(store.clone())
        .with_session_id(session_id)
        .build()
        .await;
    let _ =
        crate::runtime::commit_admission::take_product_commit_admission_observations(session_id);
    runtime
        .run_turn_assembled(
            TurnInput::text("commit"),
            CancellationToken::new(),
            crate::ScopedEffectController::shared(
                controller,
                crate::ExecutionScope::turn(session_id, "placement-turn"),
            )
            .unwrap(),
        )
        .await
        .expect("commit real turn");
    let observations =
        crate::runtime::commit_admission::take_product_commit_admission_observations(session_id);
    assert_eq!(
        observations.len(),
        expected_entries,
        "turn commit coordinator entries"
    );
    enqueue_config_patch_command(
        store.as_ref(),
        session_id,
        crate::runtime::ApplyConfigPatch {
            model: Some(
                crate::ModelSpec::builder("placement-model")
                    .context_window_tokens(32_000)
                    .build()
                    .unwrap(),
            ),
            ..crate::runtime::ApplyConfigPatch::default()
        },
    )
    .await;
    let lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        session_id,
        &lease_owner(session_id),
        "placement-command",
        crate::LeaseTimings::default().ttl_ms(),
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
        crate::runtime::commit_admission::take_product_commit_admission_observations(session_id);
    assert_eq!(
        observations.len(),
        expected_entries,
        "session command coordinator entries"
    );
}

#[tokio::test]
async fn durable_journaled_engine_commits_bypass_local_admission() {
    assert_commit_placement(
        "engine-commit-placement",
        Arc::new(JournaledCommitController::<true>::default()),
        0,
    )
    .await;
}

#[tokio::test]
async fn native_commits_enter_local_admission() {
    assert_commit_placement(
        "native-commit-placement",
        Arc::new(crate::NativeRuntimeEffectController::default()),
        1,
    )
    .await;
}

#[tokio::test]
async fn store_journaled_commits_keep_native_admission() {
    assert_commit_placement(
        "store-journaled-commit-placement",
        Arc::new(JournaledCommitController::<false>::default()),
        1,
    )
    .await;
}

#[test]
fn commit_admission_ownership_survives_controller_wrappers() {
    let controllers: [Arc<dyn crate::RuntimeEffectController>; 2] = [
        Arc::new(JournaledCommitController::<true>::default()),
        Arc::new(crate::NativeRuntimeEffectController::default()),
    ];
    for controller in controllers {
        let expected = controller.owns_commit_backpressure();
        let host = crate::NativeEffectHost::new(controller);
        assert_eq!(
            crate::RuntimeEffectController::owns_commit_backpressure(&host),
            expected
        );
        let (proxy, _requests) = crate::runtime::effect::EffectTaskController::scoped(
            &host,
            crate::ExecutionScope::turn("ownership", "turn"),
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
        session_id,
    )
    .await;
    enqueue_config_patch_command(
        store.as_ref(),
        session_id,
        crate::runtime::ApplyConfigPatch {
            model: Some(
                crate::ModelSpec::builder("engine-command-model")
                    .context_window_tokens(32_000)
                    .build()
                    .unwrap(),
            ),
            ..crate::runtime::ApplyConfigPatch::default()
        },
    )
    .await;
    let lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        session_id,
        &lease_owner(session_id),
        "invocation-command",
        crate::LeaseTimings::default().ttl_ms(),
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
        crate::runtime::commit_admission::take_product_commit_admission_observations(session_id);
    assert!(
        observations.is_empty(),
        "the invocation owns command backpressure: {observations:?}"
    );
}
