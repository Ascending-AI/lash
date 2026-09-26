mod tests {
    use std::sync::{Arc, Mutex};

    use lash_sansio::sync::MutexExt;

    use crate::SessionId;
    use crate::triggers::*;

    const SEED: u64 = 0x5_f710;

    /// Every port of one memory backend the router tests route through.
    struct RouterWorld {
        store: Arc<dyn crate::TriggerStore>,
        registry: Arc<dyn crate::ProcessRegistry>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        env_ref: crate::ProcessExecutionEnvRef,
    }

    async fn router_world() -> RouterWorld {
        let stores = crate::support::memory_store_set().await;
        let process_env_store: Arc<dyn crate::ProcessExecutionEnvStore> =
            stores.process_env_store();
        let env_ref =
            crate::testing::process_execution_env_fixture(process_env_store.as_ref()).await;
        RouterWorld {
            store: stores.trigger_store(),
            registry: stores.process_registry(),
            process_env_store,
            env_ref,
        }
    }

    fn trigger_process_draft(
        source_key: &str,
        process_name: &str,
        env_ref: crate::ProcessExecutionEnvRef,
    ) -> TriggerSubscriptionDraft {
        TriggerSubscriptionDraft::for_process(
            format!("test/{process_name}"),
            env_ref,
            "ui.button.pressed",
            source_key,
            crate::ProcessInput::Engine {
                kind: "testing-fixture".to_string(),
                payload: serde_json::json!({ "process": process_name }),
            },
            crate::ProcessIdentity::labelled("testing-fixture", Some(process_name)),
        )
        .with_payload_schema(crate::LashSchema::any())
    }

    async fn register(
        store: &dyn crate::TriggerStore,
        operation_id: &str,
        draft: TriggerSubscriptionDraft,
    ) -> TriggerSubscriptionRecord {
        let outcome = store
            .execute_command(
                operation_id,
                TriggerCommand::Register {
                    owner_scope: TriggerOwnerScope::host("test").unwrap(),
                    actor: crate::ProcessOriginator::host_scoped("test"),
                    draft,
                },
            )
            .await
            .expect("execute registration")
            .expect("register subscription");
        let TriggerCommandOutcome::Mutation { receipt } = outcome else {
            panic!("expected mutation receipt")
        };
        receipt.record_snapshot
    }

    async fn register_for_session(
        store: &dyn crate::TriggerStore,
        operation_id: &str,
        session_id: &SessionId,
        draft: TriggerSubscriptionDraft,
    ) -> TriggerSubscriptionRecord {
        let outcome = store
            .execute_command(
                operation_id,
                TriggerCommand::Register {
                    owner_scope: TriggerOwnerScope::session(session_id),
                    actor: crate::ProcessOriginator::session(crate::SessionScope::new(session_id)),
                    draft,
                },
            )
            .await
            .expect("execute session registration")
            .expect("register session subscription");
        let TriggerCommandOutcome::Mutation { receipt } = outcome else {
            panic!("expected mutation receipt")
        };
        receipt.record_snapshot
    }

    fn button_occurrence(
        source_key: impl Into<String>,
        idempotency_key: impl Into<String>,
    ) -> TriggerOccurrenceRequest {
        TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key,
            serde_json::json!({ "button": "Blue" }),
            idempotency_key,
        )
    }

    fn captured_provider_source() -> TriggerSourceCapture {
        TriggerSourceCapture::provider(
            ["ui", "button"],
            crate::LashSchema::new(serde_json::json!({
                "type": "object",
                "properties": {"account": {"type": "string"}},
                "required": ["account"],
                "additionalProperties": false
            })),
            "ui-provider",
            serde_json::json!({"account": "a", "grant": "opaque"}),
        )
    }

    struct StubRestorer {
        refusal: Option<TriggerRouteRefusal>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        seen: Arc<Mutex<Vec<TriggerSourceCapture>>>,
    }

    #[async_trait::async_trait]
    impl TriggerRouteRestorer for StubRestorer {
        async fn restore(&self, capture: &TriggerSourceCapture) -> Result<(), TriggerRouteRefusal> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.seen.lock_recover().push(capture.clone());
            match &self.refusal {
                None => Ok(()),
                Some(refusal) => Err(refusal.clone()),
            }
        }
    }

    async fn router_with_restorer(
        store: Arc<dyn crate::TriggerStore>,
        registry: Arc<dyn crate::ProcessRegistry>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        restorer: Option<Arc<StubRestorer>>,
    ) -> TriggerRouter {
        let mut router = TriggerRouter::new(
            store,
            crate::testing::process_work_wiring_for_registry(registry),
        )
        .with_process_artifacts(process_env_store, crate::testing::process_engine_fixture());
        if let Some(restorer) = restorer {
            router = router.with_route_restorer(restorer);
        }
        router
    }

    /// FIG-2913: an explicit update after a delivery was reserved must not
    /// rewrite that delivery's captured contract or route.
    #[tokio::test]
    async fn update_after_reservation_leaves_the_reserved_delivery_capture_intact() {
        let world = router_world().await;
        let store = Arc::clone(&world.store);
        let env_ref = world.env_ref.clone();
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        let draft = trigger_process_draft(&source_key, "reserved", env_ref.clone())
            .with_source_capture(captured_provider_source());
        let registered = register(store.as_ref(), "reserved-register", draft).await;

        let receipt = store
            .ingest_occurrence(
                TriggerOccurrenceRequest::new(
                    "ui.button.pressed",
                    source_key.clone(),
                    serde_json::json!({"button": "Blue"}),
                    "reserve-first",
                )
                .with_source(serde_json::json!({"account": "a"})),
            )
            .await
            .expect("reserve delivery");
        assert_eq!(receipt.reservations.len(), 1);
        assert_eq!(
            receipt.reservations[0].subscription.source_capture,
            captured_provider_source(),
            "the reservation pins the capture that was live when it reserved"
        );

        let rerouted = TriggerSourceCapture::provider(
            ["ui", "button"],
            crate::LashSchema::any(),
            "other-provider",
            serde_json::json!({"account": "b"}),
        );
        let updated = store
            .execute_command(
                "reserved-update",
                TriggerCommand::Update {
                    owner_scope: TriggerOwnerScope::host("test").unwrap(),
                    actor: crate::ProcessOriginator::host_scoped("test"),
                    subscription_key: registered.subscription_key.clone(),
                    draft: trigger_process_draft(&source_key, "reserved", env_ref)
                        .with_source_capture(rerouted.clone()),
                    expected_revision: registered.revision,
                },
            )
            .await
            .expect("execute update")
            .expect("update subscription");
        let TriggerCommandOutcome::Mutation { receipt: updated } = updated else {
            panic!("expected mutation receipt")
        };
        assert_eq!(updated.record_snapshot.source_capture, rerouted);
        assert_ne!(
            updated.record_snapshot.definition_fingerprint, registered.definition_fingerprint,
            "a rerouted source is a different definition"
        );

        let replayed = store
            .ingest_occurrence(
                TriggerOccurrenceRequest::new(
                    "ui.button.pressed",
                    source_key,
                    serde_json::json!({"button": "Blue"}),
                    "reserve-first",
                )
                .with_source(serde_json::json!({"account": "a"})),
            )
            .await
            .expect("replay reservation");
        assert_eq!(
            replayed.reservations[0].subscription.source_capture,
            captured_provider_source(),
            "the already-reserved delivery keeps the capture it reserved against"
        );
    }

    /// FIG-2913: a delivery validates the occurrence against the captured
    /// source contract, not against the live catalog.
    #[tokio::test(flavor = "multi_thread")]
    async fn delivery_refuses_an_occurrence_that_leaves_the_captured_contract() {
        let world = router_world().await;
        let store = Arc::clone(&world.store);
        let registry = Arc::clone(&world.registry);
        let process_env_store = Arc::clone(&world.process_env_store);
        let env_ref = world.env_ref.clone();
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        register(
            store.as_ref(),
            "contract-register",
            trigger_process_draft(&source_key, "contract", env_ref)
                .with_source_capture(captured_provider_source()),
        )
        .await;
        let router = router_with_restorer(
            Arc::clone(&store),
            Arc::clone(&registry),
            process_env_store,
            None,
        )
        .await;
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::AdmittedScope::runtime_operation("captured-contract"))
            .await
            .expect("open the emit handler");
        let scoped = handler.scoped();

        let report = router
            .emit(
                TriggerOccurrenceRequest::new(
                    "ui.button.pressed",
                    source_key.clone(),
                    serde_json::json!({"button": "Blue"}),
                    "off-contract",
                )
                .with_source(serde_json::json!({"unexpected": true})),
                &scoped,
            )
            .await
            .expect("emit");
        assert!(
            matches!(
                &report.deliveries[0].outcome,
                TriggerDeliveryEmitOutcome::Failed { reason }
                    if reason.contains("captured source contract")
            ),
            "off-contract occurrence must refuse, got {:?}",
            report.deliveries[0].outcome
        );

        let on_contract = router
            .emit(
                TriggerOccurrenceRequest::new(
                    "ui.button.pressed",
                    source_key,
                    serde_json::json!({"button": "Blue"}),
                    "on-contract",
                )
                .with_source(serde_json::json!({"account": "a"})),
                &scoped,
            )
            .await
            .expect("emit on-contract");
        assert_eq!(
            on_contract.deliveries[0].outcome,
            TriggerDeliveryEmitOutcome::Started
        );
        drop(scoped);
        handler.close().await.expect("close the emit handler");
    }

    /// FIG-2913: a temporarily unavailable provider keeps the reserved work and
    /// retries the same delivery identity; a revoked route refuses visibly and
    /// never reports a false start.
    #[tokio::test(flavor = "multi_thread")]
    async fn transient_route_failure_retries_the_same_identity_and_revocation_refuses() {
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::AdmittedScope::runtime_operation("route-restore"))
            .await
            .expect("open the emit handler");
        for (refusal, marker) in [
            (
                TriggerRouteRefusal::Unavailable {
                    provider_id: "ui-provider".to_string(),
                    message: "connect timeout".to_string(),
                },
                "temporarily unavailable",
            ),
            (
                TriggerRouteRefusal::Revoked {
                    provider_id: "ui-provider".to_string(),
                    message: "grant withdrawn".to_string(),
                },
                "refuses the captured route",
            ),
        ] {
            let world = router_world().await;
            let store = Arc::clone(&world.store);
            let registry = Arc::clone(&world.registry);
            let process_env_store = Arc::clone(&world.process_env_store);
            let env_ref = world.env_ref.clone();
            let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
            register(
                store.as_ref(),
                "route-register",
                trigger_process_draft(&source_key, "route", env_ref)
                    .with_source_capture(captured_provider_source()),
            )
            .await;
            let restorer = Arc::new(StubRestorer {
                refusal: Some(refusal.clone()),
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: Arc::new(Mutex::new(Vec::new())),
            });
            let router = router_with_restorer(
                Arc::clone(&store),
                Arc::clone(&registry),
                Arc::clone(&process_env_store),
                Some(Arc::clone(&restorer)),
            )
            .await;
            let scoped = handler.scoped();
            let occurrence = || {
                TriggerOccurrenceRequest::new(
                    "ui.button.pressed",
                    source_key.clone(),
                    serde_json::json!({"button": "Blue"}),
                    "route-attempt",
                )
                .with_source(serde_json::json!({"account": "a"}))
            };

            let report = router.emit(occurrence(), &scoped).await.expect("emit");
            let delivery = &report.deliveries[0];
            assert!(
                matches!(
                    &delivery.outcome,
                    TriggerDeliveryEmitOutcome::Failed { reason } if reason.contains(marker)
                ),
                "expected a visible {marker} refusal, got {:?}",
                delivery.outcome
            );
            assert!(
                registry
                    .get_process(&delivery.process_id)
                    .await
                    .expect("read process")
                    .is_none(),
                "a refused route must not start the target process"
            );
            assert_eq!(
                restorer.seen.lock_recover()[0],
                captured_provider_source(),
                "the restorer sees the capture, never a re-resolved definition"
            );

            // The reservation stayed durable. A restored provider retries the
            // identical delivery identity rather than minting a new one.
            let restored = Arc::new(StubRestorer {
                refusal: None,
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: Arc::new(Mutex::new(Vec::new())),
            });
            let router = router_with_restorer(
                store,
                Arc::clone(&registry),
                process_env_store,
                Some(restored),
            )
            .await;
            let retry = router
                .emit(occurrence(), &scoped)
                .await
                .expect("retry emit");
            assert_eq!(retry.deliveries[0].process_id, delivery.process_id);
            assert_eq!(
                retry.deliveries[0].outcome,
                TriggerDeliveryEmitOutcome::AlreadyReserved
            );
            drop(scoped);
        }
        handler.close().await.expect("close the emit handler");
    }

    #[tokio::test]
    async fn trigger_store_accepts_a_target_label_independent_of_the_identity() {
        // FIG-2995: the target_label gate is gone. The label is host-facing
        // presentation only and no longer proves anything about the durable
        // identity, so a mismatch no longer refuses registration.
        let world = router_world().await;
        let store = Arc::clone(&world.store);
        let draft = TriggerSubscriptionDraft::for_process(
            "mismatched-label",
            crate::ProcessExecutionEnvRef::new("process-env:test"),
            "ui.button.pressed",
            "source-key",
            crate::ProcessInput::External {
                metadata: serde_json::json!({}),
            },
            crate::ProcessIdentity::labelled("external", Some("expected")),
        )
        .with_target_label("other");

        store
            .execute_command(
                "mismatched-label",
                TriggerCommand::Register {
                    owner_scope: TriggerOwnerScope::host("test").unwrap(),
                    actor: crate::ProcessOriginator::host_scoped("test"),
                    draft,
                },
            )
            .await
            .expect("store execution")
            .expect("a label no longer gates registration");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn trigger_emit_report_records_started_and_already_reserved_deliveries() {
        let world = router_world().await;
        let store = Arc::clone(&world.store);
        let registry = Arc::clone(&world.registry);
        let process_env_store = Arc::clone(&world.process_env_store);
        let env_ref = world.env_ref.clone();
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        let subscription = register(
            store.as_ref(),
            "started-register",
            trigger_process_draft(&source_key, "started", env_ref),
        )
        .await;
        let router = TriggerRouter::new(
            store,
            crate::testing::process_work_wiring_for_registry(Arc::clone(&registry)),
        )
        .with_process_artifacts(process_env_store, crate::testing::process_engine_fixture());
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::AdmittedScope::runtime_operation(
                "trigger-blue-report",
            ))
            .await
            .expect("open the emit handler");
        let scoped_controller = handler.scoped();

        let report = router
            .emit(
                button_occurrence(source_key.clone(), "button-blue-report"),
                &scoped_controller,
            )
            .await
            .expect("emit trigger");
        assert_eq!(report.deliveries.len(), 1);
        let delivery = &report.deliveries[0];
        assert_eq!(delivery.occurrence_id, report.occurrence_id);
        assert_eq!(delivery.subscription_id, subscription.subscription_id);
        assert_eq!(delivery.outcome, TriggerDeliveryEmitOutcome::Started);
        let record = registry
            .get_process(&delivery.process_id)
            .await
            .expect("read process")
            .expect("started process record");
        assert!(matches!(
            record.provenance.caused_by,
            Some(crate::CausalRef::TriggerOccurrence {
                occurrence_id,
                subscription_id: Some(subscription_id),
                ..
            }) if occurrence_id == report.occurrence_id
                && subscription_id == subscription.subscription_id
        ));

        let replay = router
            .emit(
                button_occurrence(source_key, "button-blue-report"),
                &scoped_controller,
            )
            .await
            .expect("replay trigger");
        assert_eq!(replay.deliveries.len(), 1);
        assert_eq!(
            replay.deliveries[0].outcome,
            TriggerDeliveryEmitOutcome::AlreadyReserved
        );
        assert_eq!(replay.deliveries[0].process_id, delivery.process_id);
        drop(scoped_controller);
        handler.close().await.expect("close the emit handler");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_trigger_process_is_observed_by_its_registrant() {
        let world = router_world().await;
        let store = Arc::clone(&world.store);
        let registry = Arc::clone(&world.registry);
        let process_env_store = Arc::clone(&world.process_env_store);
        let env_ref = world.env_ref.clone();
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        register_for_session(
            store.as_ref(),
            "session-register",
            &SessionId::from("session-owner"),
            trigger_process_draft(&source_key, "session-owned", env_ref),
        )
        .await;
        let router = TriggerRouter::new(
            store,
            crate::testing::process_work_wiring_for_registry(
                Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>
            ),
        )
        .with_process_artifacts(process_env_store, crate::testing::process_engine_fixture());
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::AdmittedScope::runtime_operation(
                "session-trigger-blue",
            ))
            .await
            .expect("open the emit handler");
        let scoped_controller = handler.scoped();

        let report = router
            .emit(
                button_occurrence(source_key, "session-button-blue"),
                &scoped_controller,
            )
            .await
            .expect("emit session trigger");
        let process_id = &report.deliveries[0].process_id;
        assert!(
            crate::ProcessObserverRegistry::is_observer(
                &*registry,
                &SessionId::from("session-owner"),
                process_id
            )
            .await
            .expect("read initial observer"),
            "the session that explicitly registered the trigger must observe its process"
        );
        drop(scoped_controller);
        handler.close().await.expect("close the emit handler");
    }
}
