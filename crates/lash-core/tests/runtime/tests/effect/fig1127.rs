use super::*;

#[tokio::test]
async fn controller_owned_non_tool_trigger_redrive_reemits_reserved_start_without_session_nodes() {
    #[derive(Clone, Default)]
    struct ControllerOwnedTriggerEmitter {
        process_starts: Arc<std::sync::atomic::AtomicUsize>,
        native: NativeRuntimeEffectController,
    }

    #[async_trait::async_trait]
    impl lash_core::AwaitEventResolver for ControllerOwnedTriggerEmitter {}

    #[async_trait::async_trait]
    impl RuntimeEffectController for ControllerOwnedTriggerEmitter {
        async fn runtime_effect_failure_disposition(
            &self,
            _code: lash_core::RuntimeErrorCode,
        ) -> Result<lash_core::RuntimeEffectFailureDisposition, lash_core::RuntimeError> {
            Ok(lash_core::RuntimeEffectFailureDisposition::AbortInvocation)
        }

        async fn turn_control_participation(
            &self,
        ) -> Result<lash_core::TurnControlParticipation, lash_core::RuntimeError> {
            Ok(lash_core::TurnControlParticipation::DurableJournaled)
        }

        async fn execute_effect(
            &self,
            envelope: RuntimeEffectEnvelope,
            local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
            assert!(
                matches!(&envelope.command, RuntimeEffectCommand::Process { .. }),
                "non-tool trigger emission issues only its reserved process start"
            );
            self.process_starts.fetch_add(1, Ordering::SeqCst);
            self.native.execute_effect(envelope, local_executor).await
        }

        async fn open_effect_group(
            &self,
            group: lash_core::RuntimeEffectGroup,
        ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
            self.native.open_effect_group(group).await
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

        async fn group_child_drain_blocked(
            &self,
            group_key: &str,
            commit_seq: u64,
        ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
            self.native
                .group_child_drain_blocked(group_key, commit_seq)
                .await
        }
    }

    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(lash_core::TestLocalProcessRegistry::default());
    let (process_env_store, process_env_ref) = lash_core::testing::process_execution_env_fixture();
    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
        .expect("empty trigger source key");
    let registration = lash_core::TriggerStore::execute_command(
        store.as_ref(),
        "fig806-non-tool-register",
        lash_core::TriggerCommand::Register {
            owner_scope: lash_core::TriggerOwnerScope::session("root"),
            actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new("root")),
            draft: lash_core::TriggerSubscriptionDraft::for_process(
                "fig806/non-tool",
                process_env_ref,
                "ui.button.pressed",
                source_key.clone(),
                lash_core::ProcessInput::Engine {
                    kind: "testing-fixture".to_string(),
                    payload: serde_json::json!({}),
                },
                lash_core::ProcessIdentity::new("testing-fixture"),
            )
            .with_payload_schema(lash_core::LashSchema::any()),
        },
    )
    .await
    .expect("register non-tool trigger")
    .expect("non-tool trigger mutation");
    assert!(matches!(
        registration,
        lash_core::TriggerCommandOutcome::Mutation { .. }
    ));

    let router = lash_core::facade_support::TriggerRouter::new(
        Arc::clone(&store) as Arc<dyn lash_core::TriggerStore>,
        lash_core::testing::process_work_wiring_for_registry(Arc::clone(&registry)),
    )
    .with_process_artifacts(
        process_env_store,
        lash_core::testing::process_engine_fixture(),
    );
    let controller = ControllerOwnedTriggerEmitter::default();
    let occurrence = || {
        lash_core::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key.clone(),
            serde_json::json!({ "pressed": true }),
            "fig806-non-tool-occurrence",
        )
    };
    let scoped_controller = lash_core::ScopedEffectController::borrowed(
        &controller,
        lash_core::AdmittedScope::runtime_operation("fig1127-trigger-emission"),
    )
    .expect("bind trigger emitter scope");
    let first = router
        .emit(occurrence(), &scoped_controller)
        .await
        .expect("emit non-tool trigger");
    let redrive = router
        .emit(occurrence(), &scoped_controller)
        .await
        .expect("redrive non-tool trigger");

    assert_eq!(first.deliveries.len(), 1);
    assert_eq!(redrive.deliveries.len(), 1);
    assert_eq!(
        first.deliveries[0].outcome,
        lash_core::facade_support::TriggerDeliveryEmitOutcome::Started
    );
    assert_eq!(
        redrive.deliveries[0].outcome,
        lash_core::facade_support::TriggerDeliveryEmitOutcome::AlreadyReserved
    );
    assert_eq!(
        first.deliveries[0].process_id,
        redrive.deliveries[0].process_id
    );
    assert_eq!(
        controller.process_starts.load(Ordering::SeqCst),
        2,
        "the controller-owned redrive re-emits the deterministic reserved start"
    );
    assert_eq!(
        lash_core::TriggerStore::list_deliveries(store.as_ref())
            .await
            .expect("list non-tool deliveries")
            .len(),
        1,
        "the repeated occurrence owns one delivery and no session-node side channel"
    );
}
