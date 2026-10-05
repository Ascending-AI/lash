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
        let stores = crate::support::sqlite_memory_store_set().await;
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
        .with_payload_schema(crate::JsonSchema::any())
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
        receipt.record
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
        receipt.record
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
            crate::JsonSchema::admit(serde_json::json!({
                "type": "object",
                "properties": {"account": {"type": "string"}},
                "required": ["account"],
                "additionalProperties": false
            }))
            .expect("valid declared payload schema"),
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
            crate::JsonSchema::any(),
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
        assert_eq!(updated.record.source_capture, rerouted);
        assert_ne!(
            updated.record.definition_fingerprint, registered.definition_fingerprint,
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

        let request = TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key.clone(),
            serde_json::json!({"button": "Blue"}),
            "off-contract",
        )
        .with_source(serde_json::json!({"unexpected": true}));
        let report = router.emit(request.clone(), &scoped).await.expect("emit");
        assert!(
            serde_json::to_value(&report.deliveries[0].outcome)
                .expect("the delivery report serializes")["failed"]["value_mismatch"]
                .is_object(),
            "a trigger delivery report must retain its typed value mismatch"
        );
        assert!(
            matches!(
                &report.deliveries[0].outcome,
                TriggerDeliveryEmitOutcome::Failed { value_mismatch: Some(source), .. }
                    if source.instance_path.is_empty() && source.message.contains("account")
            ),
            "off-contract occurrence must refuse, got {:?}",
            report.deliveries[0].outcome
        );

        let refusal = router
            .emit_recorded(request, &scoped)
            .await
            .expect_err("recorded emission refuses an unstarted delivery");
        let refusal = crate::RuntimeEffectControllerError::from(refusal);
        assert!(
            matches!(refusal.cause,
            Some(crate::RuntimeErrorCause::ValueMismatch { source, .. })
                if source.instance_path.is_empty() && source.message.contains("account")),
            "recorded emission retains the typed value mismatch"
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
        assert!(matches!(
            on_contract.deliveries[0].outcome,
            TriggerDeliveryEmitOutcome::Started { .. }
        ));
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
                    TriggerDeliveryEmitOutcome::Failed { reason, .. } if reason.contains(marker)
                ),
                "expected a visible {marker} refusal, got {:?}",
                delivery.outcome
            );
            assert!(
                delivery.process_id().is_none(),
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
            // The refused attempt started nothing; the retry realizes the same
            // reservation under its start key (ADR 0107).
            assert_eq!(delivery.process_id(), None);
            assert!(retry.deliveries[0].process_id().is_some());
            assert!(matches!(
                retry.deliveries[0].outcome,
                TriggerDeliveryEmitOutcome::Started { .. }
            ));
            drop(scoped);
        }
        handler.close().await.expect("close the emit handler");
    }

    /// FIG-4090: a delivery reserved before a crash is recovered from its
    /// reservation, and the recovery keeps the route's two failures apart: an
    /// unavailable provider leaves the delivery owed for a retry under the
    /// same identity, a revoked route refuses it for good. Neither starts a
    /// process; a restored provider's recovery starts and binds the one
    /// process the delivery's start key names.
    #[tokio::test]
    async fn a_recovered_delivery_retries_an_unavailable_route_and_refuses_a_revoked_one() {
        for (refusal, retryable) in [
            (
                TriggerRouteRefusal::Unavailable {
                    provider_id: "ui-provider".to_string(),
                    message: "connect timeout".to_string(),
                },
                true,
            ),
            (
                TriggerRouteRefusal::Revoked {
                    provider_id: "ui-provider".to_string(),
                    message: "grant withdrawn".to_string(),
                },
                false,
            ),
        ] {
            let world = router_world().await;
            let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
            register(
                world.store.as_ref(),
                "recover-register",
                trigger_process_draft(&source_key, "recover", world.env_ref.clone())
                    .with_source_capture(captured_provider_source()),
            )
            .await;
            // The emit reserved the delivery; the deployment died before its
            // start.
            let reservation = world
                .store
                .ingest_occurrence(
                    TriggerOccurrenceRequest::new(
                        "ui.button.pressed",
                        source_key,
                        serde_json::json!({"button": "Blue"}),
                        "recover-occurrence",
                    )
                    .with_source(serde_json::json!({"account": "a"})),
                )
                .await
                .expect("reserve the delivery")
                .reservations
                .remove(0);
            let occurrence_id = reservation.occurrence.occurrence_id.clone();
            let subscription_id = reservation.subscription.subscription_id.clone();
            let router = |restorer: StubRestorer| {
                router_with_restorer(
                    Arc::clone(&world.store),
                    Arc::clone(&world.registry),
                    Arc::clone(&world.process_env_store),
                    Some(Arc::new(restorer)),
                )
            };
            let stub = |refusal: Option<TriggerRouteRefusal>| StubRestorer {
                refusal,
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: Arc::new(Mutex::new(Vec::new())),
            };

            let failed = Box::pin(
                router(stub(Some(refusal.clone())))
                    .await
                    .recover_delivery(&occurrence_id, &subscription_id),
            )
            .await
            .expect_err("a refused route starts nothing");
            assert_eq!(
                matches!(failed, TriggerDeliveryRecoveryError::Retryable(_)),
                retryable,
                "{refusal:?} classifies as retryable={retryable}, got {failed:?}"
            );
            let start_key = trigger_delivery_start_key(&reservation);
            assert!(
                world
                    .registry
                    .get_process_by_start_key(&start_key)
                    .await
                    .expect("read the start key")
                    .is_none(),
                "a refused route registers no process"
            );

            let process_id = Box::pin(
                router(stub(None))
                    .await
                    .recover_delivery(&occurrence_id, &subscription_id),
            )
            .await
            .expect("a restored route recovers the delivery");
            assert_eq!(
                world
                    .registry
                    .get_process_by_start_key(&start_key)
                    .await
                    .expect("read the start key")
                    .map(|record| record.id),
                Some(process_id.clone()),
                "recovery registered the one process the start key names"
            );
            assert_eq!(
                world
                    .store
                    .list_deliveries_by_occurrence_id(&occurrence_id)
                    .await
                    .expect("read the delivery")[0]
                    .process_id,
                Some(process_id.clone()),
                "recovery bound the delivery"
            );
            assert_eq!(
                Box::pin(
                    router(stub(None))
                        .await
                        .recover_delivery(&occurrence_id, &subscription_id)
                )
                .await
                .expect("a bound delivery answers at once"),
                process_id,
                "recovering a bound delivery again answers its process"
            );
        }
    }

    /// FIG-4554: the route restorer serves new work only. A delivery whose
    /// start registered before its bind was lost is redriven from the process
    /// its start key holds, and a route revoked since is never asked.
    #[tokio::test]
    async fn a_redriven_delivery_whose_start_registered_never_asks_a_revoked_route() {
        let world = router_world().await;
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        register(
            world.store.as_ref(),
            "redrive-register",
            trigger_process_draft(&source_key, "redrive", world.env_ref.clone())
                .with_source_capture(captured_provider_source()),
        )
        .await;
        let reservation = world
            .store
            .ingest_occurrence(
                TriggerOccurrenceRequest::new(
                    "ui.button.pressed",
                    source_key,
                    serde_json::json!({"button": "Blue"}),
                    "redrive-occurrence",
                )
                .with_source(serde_json::json!({"account": "a"})),
            )
            .await
            .expect("reserve the delivery")
            .reservations
            .remove(0);
        let occurrence_id = reservation.occurrence.occurrence_id.clone();
        let subscription_id = reservation.subscription.subscription_id.clone();
        let stub = |refusal: Option<TriggerRouteRefusal>| {
            Arc::new(StubRestorer {
                refusal,
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: Arc::new(Mutex::new(Vec::new())),
            })
        };

        // The first shift restores the route and registers; its bind is lost.
        let granted = stub(None);
        router_with_restorer(
            Arc::new(BindFailsOnce::new(Arc::clone(&world.store))),
            Arc::clone(&world.registry),
            Arc::clone(&world.process_env_store),
            Some(Arc::clone(&granted)),
        )
        .await
        .recover_delivery(&occurrence_id, &subscription_id)
        .await
        .expect_err("the first shift's bind is lost");
        assert_eq!(
            granted.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the fresh start restored its route once"
        );
        let started = world
            .registry
            .get_process_by_start_key(&trigger_delivery_start_key(&reservation))
            .await
            .expect("read the start key")
            .expect("the first shift registered the delivery's process");

        // The provider revokes the route; the redrive binds the started
        // process and asks nothing.
        let revoked = stub(Some(TriggerRouteRefusal::Revoked {
            provider_id: "ui-provider".to_string(),
            message: "grant withdrawn".to_string(),
        }));
        let redriven = router_with_restorer(
            Arc::clone(&world.store),
            Arc::clone(&world.registry),
            Arc::clone(&world.process_env_store),
            Some(Arc::clone(&revoked)),
        )
        .await
        .recover_delivery(&occurrence_id, &subscription_id)
        .await
        .expect("the redrive answers the process the first shift started");
        assert_eq!(redriven, started.id);
        assert_eq!(
            revoked.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a started delivery's redrive never asks the restorer"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replayed_trigger_emission_reports_the_delivery_its_first_emission_started() {
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
        assert!(matches!(
            delivery.outcome,
            TriggerDeliveryEmitOutcome::Started { .. }
        ));
        let record = registry
            .get_process(
                delivery
                    .process_id()
                    .expect("the delivery started a process"),
            )
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
        // The replay finds the reservation already held and reports the
        // settled delivery, identical to the first report (FIG-4272).
        assert_eq!(replay, report);
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
        let process_id = report.deliveries[0]
            .process_id()
            .expect("the delivery started a process");
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

    /// The immediate producer path pins its child until the bind commits (ADR
    /// 0021, FIG-4203): an emit whose attempt dies at a lost bind leaves the
    /// child pinned,
    /// the completed child survives a retention pass, and the recovery binds
    /// that child and releases the pin. A bound child is then pruned as usual.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_emit_whose_bind_is_lost_pins_its_child_until_recovery_binds_it() {
        let world = router_world().await;
        let registry = Arc::clone(&world.registry);
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        let subscription = register(
            world.store.as_ref(),
            "pinned-register",
            trigger_process_draft(&source_key, "pinned", world.env_ref.clone()),
        )
        .await;
        let failing = Arc::new(BindFailsOnce::new(Arc::clone(&world.store)));
        let router = TriggerRouter::new(
            Arc::clone(&failing) as Arc<dyn crate::TriggerStore>,
            crate::testing::process_work_wiring_for_registry(Arc::clone(&registry)),
        )
        .with_process_artifacts(
            Arc::clone(&world.process_env_store),
            crate::testing::process_engine_fixture(),
        );
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::AdmittedScope::runtime_operation("trigger-pinned"))
            .await
            .expect("open the emit handler");
        let scoped_controller = handler.scoped();

        // The bind's fault is its attempt's, so the emission never answers:
        // the engine would run the attempt again. This attempt dies instead.
        tokio::select! {
            answered = router.emit(
                button_occurrence(source_key, "button-pinned"),
                &scoped_controller,
            ) => panic!("an attempt whose bind is lost answers nothing: {answered:?}"),
            () = failing.bind_failed() => {}
        }
        let occurrence_id = world
            .store
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("list the occurrence")
            .remove(0)
            .occurrence_id;
        let reservation = world
            .store
            .list_deliveries_by_occurrence_id(&occurrence_id)
            .await
            .expect("list the delivery")
            .remove(0);
        assert_eq!(reservation.process_id, None, "the bind was lost");
        let child = registry
            .get_process_by_start_key(&trigger_delivery_start_key(&reservation))
            .await
            .expect("read the start key")
            .expect("the emit registered the child");
        assert_eq!(
            registry
                .list_trigger_delivery_pins()
                .await
                .expect("list pins"),
            vec![crate::PinnedTriggerDelivery {
                process_id: child.id.clone(),
                pin: crate::TriggerDeliveryPin {
                    occurrence_id: occurrence_id.clone(),
                    subscription_id: subscription.subscription_id.clone(),
                },
            }],
            "the emit's registration wrote the pin"
        );

        // The child completes, and a retention pass keeps it.
        registry
            .complete_process(
                &child.id,
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::Value::Null,
                )),
                crate::ProcessCompletionAuthority::WorkflowKey {
                    workflow_key: child.id.to_string(),
                },
            )
            .await
            .expect("complete the child");
        assert_eq!(
            crate::runtime::release_bound_trigger_delivery_pins(
                registry.as_ref(),
                world.store.as_ref(),
            )
            .await
            .expect("release bound pins"),
            0,
            "an unbound delivery keeps its pin"
        );
        let report_prune = registry
            .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
            .await
            .expect("prune");
        assert_eq!(report_prune.pruned_processes, 0, "the pinned child stays");

        // Recovery binds the child the emit registered and releases the pin.
        let recovered =
            Box::pin(router.recover_delivery(&occurrence_id, &subscription.subscription_id))
                .await
                .expect("recover the delivery");
        assert_eq!(recovered, child.id);
        assert_eq!(
            registry
                .list_trigger_delivery_pins()
                .await
                .expect("list pins"),
            Vec::new(),
            "the bind released the pin"
        );
        let report_prune = registry
            .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
            .await
            .expect("prune");
        assert_eq!(
            report_prune.pruned_processes, 1,
            "the bound child is pruned"
        );
        // The attempt died at its bind, so its handler ended with it.
        drop(scoped_controller);
        drop(handler);
    }

    /// A bind the store did not answer is its attempt's fault, never the
    /// delivery's recorded failure (FIG-4513): the engine runs the attempt
    /// again, which serves the ingest and the start from its journal, binds
    /// the child the first attempt registered and reports it started.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_emit_whose_bind_meets_an_outage_binds_when_its_attempt_runs_again() {
        let world = router_world().await;
        let registry = Arc::clone(&world.registry);
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        let subscription = register(
            world.store.as_ref(),
            "bind-outage-register",
            trigger_process_draft(&source_key, "bind-outage", world.env_ref.clone()),
        )
        .await;
        let failing = Arc::new(BindFailsOnce::new(Arc::clone(&world.store)));
        let router = Arc::new(
            TriggerRouter::new(
                Arc::clone(&failing) as Arc<dyn crate::TriggerStore>,
                crate::testing::process_work_wiring_for_registry(Arc::clone(&registry)),
            )
            .with_process_artifacts(
                Arc::clone(&world.process_env_store),
                crate::testing::process_engine_fixture(),
            ),
        );
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reports: Arc<std::sync::Mutex<Vec<TriggerEmitReport>>> = Arc::default();
        let attempt: lash_restate_test::HandlerAttempt = {
            let router = Arc::clone(&router);
            let attempts = Arc::clone(&attempts);
            let reports = Arc::clone(&reports);
            Arc::new(move |scoped| {
                let router = Arc::clone(&router);
                let attempts = Arc::clone(&attempts);
                let reports = Arc::clone(&reports);
                let source_key = source_key.clone();
                Box::pin(async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let report = router
                        .emit(button_occurrence(source_key, "button-bind-outage"), &scoped)
                        .await
                        .expect("an attempt that answers emits");
                    reports.lock().expect("reports lock").push(report);
                })
            })
        };
        double
            .run_in_handler(
                crate::AdmittedScope::runtime_operation("trigger-bind-outage"),
                attempt,
            )
            .await
            .expect("the emission completes once its bind is answered");

        assert!(
            attempts.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the engine ran the attempt again after the unanswered bind"
        );
        let reports = reports.lock().expect("reports lock").clone();
        assert_eq!(
            reports.len(),
            1,
            "only the attempt whose bind landed answered"
        );
        let report = &reports[0];
        let reservation = world
            .store
            .list_deliveries_by_occurrence_id(&report.occurrence_id)
            .await
            .expect("list the delivery")
            .remove(0);
        let child = registry
            .get_process_by_start_key(&trigger_delivery_start_key(&reservation))
            .await
            .expect("read the start key")
            .expect("the first attempt registered the child");
        assert_eq!(
            (
                report.deliveries[0].outcome.clone(),
                report.deliveries[0].subscription_id.clone(),
            ),
            (
                TriggerDeliveryEmitOutcome::Started {
                    process_id: child.id.clone()
                },
                subscription.subscription_id.clone(),
            ),
            "the delivery reports the child its first attempt registered"
        );
        assert_eq!(
            reservation.process_id,
            Some(child.id.clone()),
            "the delivery is bound to that child"
        );
        assert_eq!(
            registry
                .list_trigger_delivery_pins()
                .await
                .expect("list pins"),
            Vec::new(),
            "the bind released the pin"
        );
        assert_eq!(
            registry
                .list_processes(&crate::ProcessListFilter {
                    status: crate::ProcessStatusFilter::Any,
                    ..crate::ProcessListFilter::default()
                })
                .await
                .expect("list processes")
                .len(),
            1,
            "no second child was registered"
        );
    }

    /// A trigger store whose first delivery bind fails as a lost write would:
    /// the registration before it landed, the bind did not.
    struct BindFailsOnce {
        inner: Arc<dyn crate::TriggerStore>,
        failed: std::sync::atomic::AtomicBool,
    }

    impl BindFailsOnce {
        fn new(inner: Arc<dyn crate::TriggerStore>) -> Self {
            Self {
                inner,
                failed: std::sync::atomic::AtomicBool::new(false),
            }
        }

        /// Resolves once the first bind has failed.
        async fn bind_failed(&self) {
            while !self.failed.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::TriggerStore for BindFailsOnce {
        async fn execute_command(
            &self,
            operation_id: &str,
            command: TriggerCommand,
        ) -> Result<crate::TriggerEffectResult, crate::PluginError> {
            self.inner.execute_command(operation_id, command).await
        }

        async fn list_subscriptions(
            &self,
            filter: crate::TriggerSubscriptionFilter,
        ) -> Result<Vec<TriggerSubscriptionRecord>, crate::PluginError> {
            self.inner.list_subscriptions(filter).await
        }

        async fn subscriptions_changed_since(
            &self,
            cursor: crate::TriggerSubscriptionChangeCursor,
            limit: usize,
        ) -> std::result::Result<
            (
                Vec<crate::TriggerSubscriptionChange>,
                crate::TriggerSubscriptionChangeCursor,
            ),
            crate::PluginError,
        > {
            self.inner.subscriptions_changed_since(cursor, limit).await
        }
        async fn list_subscriptions_with_cursor(
            &self,
        ) -> std::result::Result<
            (
                Vec<crate::TriggerSubscriptionRecord>,
                crate::TriggerSubscriptionChangeCursor,
            ),
            crate::PluginError,
        > {
            self.inner.list_subscriptions_with_cursor().await
        }
        async fn compact_subscription_tombstones(
            &self,
            cutoff_epoch_ms: u64,
        ) -> std::result::Result<usize, crate::PluginError> {
            self.inner
                .compact_subscription_tombstones(cutoff_epoch_ms)
                .await
        }

        async fn delete_session_subscriptions(
            &self,
            session_id: &SessionId,
        ) -> Result<usize, crate::PluginError> {
            self.inner.delete_session_subscriptions(session_id).await
        }

        async fn ingest_occurrence(
            &self,
            request: TriggerOccurrenceRequest,
        ) -> Result<crate::TriggerIngressReceipt, crate::PluginError> {
            self.inner.ingest_occurrence(request).await
        }

        async fn list_occurrences(
            &self,
            filter: crate::TriggerOccurrenceFilter,
        ) -> Result<Vec<crate::TriggerOccurrenceRecord>, crate::PluginError> {
            self.inner.list_occurrences(filter).await
        }

        async fn list_deliveries_by_occurrence_id(
            &self,
            occurrence_id: &str,
        ) -> Result<Vec<TriggerDeliveryReservation>, crate::PluginError> {
            self.inner
                .list_deliveries_by_occurrence_id(occurrence_id)
                .await
        }

        async fn list_deliveries_by_subscription_id(
            &self,
            subscription_id: &str,
        ) -> Result<Vec<TriggerDeliveryReservation>, crate::PluginError> {
            self.inner
                .list_deliveries_by_subscription_id(subscription_id)
                .await
        }

        async fn list_deliveries_by_process_id(
            &self,
            process_id: &crate::ProcessId,
        ) -> Result<Vec<TriggerDeliveryReservation>, crate::PluginError> {
            self.inner.list_deliveries_by_process_id(process_id).await
        }

        async fn list_deliveries(
            &self,
        ) -> Result<Vec<TriggerDeliveryReservation>, crate::PluginError> {
            self.inner.list_deliveries().await
        }

        async fn bind_delivery_process(
            &self,
            occurrence_id: &str,
            subscription_id: &str,
            process_id: &crate::ProcessId,
        ) -> Result<(), crate::PluginError> {
            if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::PluginError::from(
                    crate::store::StoreFault::Backend {
                        message: "the delivery's bind was lost".to_string(),
                    },
                ));
            }
            self.inner
                .bind_delivery_process(occurrence_id, subscription_id, process_id)
                .await
        }

        async fn list_delivery_process_ids(
            &self,
        ) -> Result<Vec<crate::ProcessId>, crate::PluginError> {
            self.inner.list_delivery_process_ids().await
        }

        async fn list_delivery_retention_candidates(
            &self,
        ) -> Result<Vec<crate::TriggerDeliveryRetentionCandidate>, crate::PluginError> {
            self.inner.list_delivery_retention_candidates().await
        }

        async fn list_session_owner_ids_for_retention(
            &self,
        ) -> Result<Vec<SessionId>, crate::PluginError> {
            self.inner.list_session_owner_ids_for_retention().await
        }

        async fn reconcile_trigger_retention(
            &self,
            candidates: &[crate::TriggerDeliveryRetentionCandidate],
            deleted_session_ids: &[SessionId],
        ) -> Result<crate::TriggerRetentionReconciliationReport, crate::PluginError> {
            self.inner
                .reconcile_trigger_retention(candidates, deleted_session_ids)
                .await
        }

        async fn delete_delivery_retention_candidates(
            &self,
            candidates: &[crate::TriggerDeliveryRetentionCandidate],
        ) -> Result<usize, crate::PluginError> {
            self.inner
                .delete_delivery_retention_candidates(candidates)
                .await
        }

        async fn reclaim_trigger_occurrences(
            &self,
            cutoff_epoch_ms: u64,
        ) -> crate::TriggerOccurrenceReclamationResult {
            self.inner
                .reclaim_trigger_occurrences(cutoff_epoch_ms)
                .await
        }

        async fn forget_trigger_tombstones(
            &self,
            written_before_epoch_ms: u64,
        ) -> std::result::Result<usize, crate::StoreError> {
            self.inner
                .forget_trigger_tombstones(written_before_epoch_ms)
                .await
        }

        async fn prune_non_fired_occurrences(
            &self,
            cutoff_epoch_ms: u64,
        ) -> Result<usize, crate::PluginError> {
            self.inner
                .prune_non_fired_occurrences(cutoff_epoch_ms)
                .await
        }
    }
}
