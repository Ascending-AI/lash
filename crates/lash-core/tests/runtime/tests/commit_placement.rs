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
    fn effect_journaling(&self) -> lash_core::EffectJournaling {
        lash_core::EffectJournaling::Journaled
    }
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        self.native.execute_effect(envelope, local_executor).await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.native.open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: std::sync::Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.native.register_group_executors(executors)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.native.await_next_settlement(handle, cancel).await
    }
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.native.read_group_settlement(group_key, rank).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.native.close_effect_group(handle, disposition).await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.native.commit_group_child_final(commit).await
    }

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.native
            .await_group_child_drain_admission(group_key, commit_seq)
            .await
    }
}

async fn assert_commit_placement(
    session_id: &SessionId,
    effect_host: Arc<dyn lash_core::EffectHost>,
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
    let host = EmbeddedRuntimeHost::new(
        test_runtime_host_config().with_effect_host(Arc::clone(&effect_host)),
    );
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
            effect_host
                .scoped(lash_core::AdmittedScope::turn(session_id, "placement-turn"))
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

/// The host an engine-owned controller is lent through.
fn engine_commit_host() -> Arc<dyn lash_core::EffectHost> {
    effect::controller_effect_host(Arc::new(JournaledCommitController::<true>::default()))
}

/// A SQLite memory backend's effect host: a store-journaled host that owns
/// no commit backpressure.
async fn store_commit_host() -> Arc<dyn lash_core::EffectHost> {
    lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a memory backend")
        .effect_host()
}

#[tokio::test]
async fn durable_journaled_engine_commits_bypass_local_admission() {
    Box::pin(assert_commit_placement(
        &SessionId::from("engine-commit-placement"),
        engine_commit_host(),
        0,
    ))
    .await;
}

#[tokio::test]
async fn store_host_commits_enter_local_admission() {
    Box::pin(assert_commit_placement(
        &SessionId::from("store-host-commit-placement"),
        store_commit_host().await,
        1,
    ))
    .await;
}

#[tokio::test]
async fn store_journaled_commits_keep_native_admission() {
    Box::pin(assert_commit_placement(
        &SessionId::from("store-journaled-commit-placement"),
        effect::controller_effect_host(Arc::new(JournaledCommitController::<false>::default())),
        1,
    ))
    .await;
}

/// Passes every operation through untouched.
struct PassThrough;

impl lash_core::testing::EffectLayer for PassThrough {}

#[tokio::test]
async fn commit_admission_ownership_survives_controller_wrappers() {
    let hosts: [(Arc<dyn lash_core::EffectHost>, bool); 2] = [
        (engine_commit_host(), true),
        (store_commit_host().await, false),
    ];
    for (inner, expected) in hosts {
        let host = lash_core::testing::LayeredEffectHost::new(inner, Arc::new(PassThrough));
        let admitted = lash_core::AdmittedScope::turn("ownership", "turn");
        let scoped = lash_core::EffectHost::scoped_static(&host, admitted.clone())
            .unwrap()
            .expect("both inner hosts lend owned controllers");
        assert_eq!(scoped.controller().owns_commit_backpressure(), expected);
        let (proxy, _requests) =
            lash_core::runtime::effect::EffectTaskController::scoped(scoped.controller(), admitted)
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
