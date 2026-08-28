use super::*;

#[tokio::test]
async fn controller_owned_non_tool_trigger_redrive_reemits_reserved_start_without_session_nodes() {
    #[derive(Clone, Default)]
    struct ControllerOwnedTriggerEmitter {
        process_starts: Arc<std::sync::atomic::AtomicUsize>,
        native: NativeRuntimeEffectController,
    }

    #[async_trait::async_trait]
    impl crate::AwaitEventResolver for ControllerOwnedTriggerEmitter {}

    #[async_trait::async_trait]
    impl RuntimeEffectController for ControllerOwnedTriggerEmitter {
        async fn runtime_effect_failure_disposition(
            &self,
            _code: crate::RuntimeErrorCode,
        ) -> Result<crate::RuntimeEffectFailureDisposition, crate::RuntimeError> {
            Ok(crate::RuntimeEffectFailureDisposition::AbortInvocation)
        }

        async fn turn_control_participation(
            &self,
        ) -> Result<crate::TurnControlParticipation, crate::RuntimeError> {
            Ok(crate::TurnControlParticipation::DurableJournaled)
        }

        async fn execute_effect(
            &self,
            envelope: RuntimeEffectEnvelope,
            local_executor: crate::RuntimeEffectLocalExecutor<'_>,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
            assert!(
                matches!(&envelope.command, RuntimeEffectCommand::Process { .. }),
                "non-tool trigger emission issues only its reserved process start"
            );
            self.process_starts.fetch_add(1, Ordering::SeqCst);
            self.native.execute_effect(envelope, local_executor).await
        }
    }

    let store = Arc::new(crate::InMemoryTriggerStore::default());
    let registry: Arc<dyn crate::ProcessRegistry> =
        Arc::new(crate::TestLocalProcessRegistry::default());
    let source_key =
        crate::empty_trigger_source_key("ui.button.pressed").expect("empty trigger source key");
    let registration = crate::TriggerStore::execute_command(
        store.as_ref(),
        "fig806-non-tool-register",
        crate::TriggerCommand::Register {
            owner_scope: crate::TriggerOwnerScope::session("root"),
            actor: crate::ProcessOriginator::session(crate::SessionScope::new("root")),
            draft: crate::TriggerSubscriptionDraft::for_process(
                "fig806/non-tool",
                crate::ProcessExecutionEnvRef::new("process-env:fig806-non-tool"),
                "ui.button.pressed",
                source_key.clone(),
                crate::ProcessInput::Engine {
                    kind: "fig806-non-tool-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                crate::ProcessIdentity::new("fig806-non-tool-engine"),
            )
            .with_payload_schema(crate::LashSchema::any()),
        },
    )
    .await
    .expect("register non-tool trigger")
    .expect("non-tool trigger mutation");
    assert!(matches!(
        registration,
        crate::TriggerCommandOutcome::Mutation { .. }
    ));

    let router = crate::TriggerRouter::new(
        Arc::clone(&store) as Arc<dyn crate::TriggerStore>,
        crate::testing::process_work_wiring_for_registry(Arc::clone(&registry)),
    );
    let controller = ControllerOwnedTriggerEmitter::default();
    let occurrence = || {
        crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key.clone(),
            serde_json::json!({ "pressed": true }),
            "fig806-non-tool-occurrence",
        )
    };
    let first = router
        .emit(occurrence(), &controller)
        .await
        .expect("emit non-tool trigger");
    let redrive = router
        .emit(occurrence(), &controller)
        .await
        .expect("redrive non-tool trigger");

    assert_eq!(first.deliveries.len(), 1);
    assert_eq!(redrive.deliveries.len(), 1);
    assert_eq!(
        first.deliveries[0].outcome,
        crate::TriggerDeliveryEmitOutcome::Started
    );
    assert_eq!(
        redrive.deliveries[0].outcome,
        crate::TriggerDeliveryEmitOutcome::AlreadyReserved
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
        crate::TriggerStore::list_deliveries(store.as_ref())
            .await
            .expect("list non-tool deliveries")
            .len(),
        1,
        "the repeated occurrence owns one delivery and no session-node side channel"
    );
}
