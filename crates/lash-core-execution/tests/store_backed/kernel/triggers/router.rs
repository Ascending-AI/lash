mod tests {
    use std::sync::{Arc, Mutex};

    use lash_sansio::sync::MutexExt;

    use crate::SessionId;
    use crate::triggers::*;

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
