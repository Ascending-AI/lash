mod tests {
    use std::sync::{Arc, Mutex};

    use lash_sansio::sync::MutexExt;

    use crate::triggers::*;

    /// Every port of one memory backend the router tests route through.
    struct RouterWorld {
        backend: crate::Backend,
        store: Arc<dyn crate::TriggerStore>,
        registry: Arc<dyn crate::ProcessRegistry>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        env_ref: crate::ProcessExecutionEnvRef,
    }

    impl RouterWorld {
        /// The context an emission commits its start through.
        fn emitter(&self) -> crate::ActorContext {
            crate::ActorContext::detached(self.backend.clone())
        }

        /// Every process the registry holds, any status.
        async fn processes(&self) -> Vec<crate::ProcessRecord> {
            self.registry
                .list_processes(&crate::ProcessListFilter {
                    status: crate::ProcessStatusFilter::Any,
                    ..crate::ProcessListFilter::default()
                })
                .await
                .expect("list processes")
        }
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
            backend: lash_conformance::backend_over(stores),
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

    fn pressed(source_key: &str, idempotency_key: &str) -> TriggerOccurrenceRequest {
        TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key,
            serde_json::json!({"button": "Blue"}),
            idempotency_key,
        )
        .with_source(serde_json::json!({"account": "a"}))
    }

    struct StubRestorer {
        refusal: Option<TriggerRouteRefusal>,
        calls: std::sync::atomic::AtomicUsize,
        seen: Mutex<Vec<TriggerSourceCapture>>,
    }

    impl StubRestorer {
        fn new(refusal: Option<TriggerRouteRefusal>) -> Arc<Self> {
            Arc::new(Self {
                refusal,
                calls: std::sync::atomic::AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
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

    fn router_with_restorer(world: &RouterWorld, restorer: Arc<StubRestorer>) -> TriggerRouter {
        TriggerRouter::new(
            Arc::clone(&world.store),
            crate::testing::process_work_wiring_for_registry(Arc::clone(&world.registry)),
        )
        .with_process_artifacts(
            Arc::clone(&world.process_env_store),
            crate::testing::process_engine_fixture(),
        )
        .with_route_restorer(restorer)
    }

    /// FIG-2913 and FIG-4554: an update after an occurrence started leaves
    /// its delivery's captured contract and route intact, and its emission
    /// again answers the process it started without asking the route.
    #[tokio::test]
    async fn a_held_occurrence_answers_the_capture_it_started_against_and_asks_no_route() {
        let world = router_world().await;
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        let registered = register(
            world.store.as_ref(),
            "held-register",
            trigger_process_draft(&source_key, "held", world.env_ref.clone())
                .with_source_capture(captured_provider_source()),
        )
        .await;
        let restorer = StubRestorer::new(None);
        let router = router_with_restorer(&world, Arc::clone(&restorer));

        let first = router
            .emit(pressed(&source_key, "held-first"), &world.emitter())
            .await
            .expect("the first emission starts its delivery");
        let [delivery] = first.deliveries.as_slice() else {
            panic!("one subscription matches: {first:?}");
        };
        let TriggerDeliveryEmitOutcome::Started { process_id } = &delivery.outcome else {
            panic!("the delivery started: {first:?}");
        };
        assert_eq!(restorer.calls(), 1, "a fresh start restores its route once");
        assert_eq!(
            *restorer.seen.lock_recover(),
            vec![captured_provider_source()]
        );

        let rerouted = TriggerSourceCapture::provider(
            ["ui", "button"],
            crate::JsonSchema::any(),
            "other-provider",
            serde_json::json!({"account": "b"}),
        );
        let updated = world
            .store
            .execute_command(
                "held-update",
                TriggerCommand::Update {
                    owner_scope: TriggerOwnerScope::host("test").unwrap(),
                    actor: crate::ProcessOriginator::host_scoped("test"),
                    subscription_key: registered.subscription_key.clone(),
                    draft: trigger_process_draft(&source_key, "held", world.env_ref.clone())
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

        let again = router
            .emit(pressed(&source_key, "held-first"), &world.emitter())
            .await
            .expect("the redelivered emission answers its occurrence");
        assert_eq!(again, first, "the held occurrence answers its first start");
        let held = world
            .store
            .list_deliveries_by_occurrence_id(&first.occurrence_id)
            .await
            .expect("read the delivery");
        let [held] = held.as_slice() else {
            panic!("the occurrence holds one delivery: {held:?}");
        };
        assert_eq!(held.subscription.source_capture, captured_provider_source());
        assert_eq!(&held.process_id, process_id);
        assert_eq!(
            restorer.calls(),
            1,
            "a held occurrence's emission asks the route nothing"
        );
        assert_eq!(world.processes().await.len(), 1);
    }

    /// FIG-4090: an unavailable route fails the emission before anything
    /// commits, so a retry starts the delivery's one process; a revoked
    /// route refuses the delivery for good: the occurrence is recorded with
    /// no delivery for it, and no emission of it starts a process.
    #[tokio::test]
    async fn an_unavailable_route_records_nothing_and_a_revoked_one_refuses_its_delivery() {
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
                "route-register",
                trigger_process_draft(&source_key, "route", world.env_ref.clone())
                    .with_source_capture(captured_provider_source()),
            )
            .await;
            let request = pressed(&source_key, "route-occurrence");

            let refused = router_with_restorer(&world, StubRestorer::new(Some(refusal.clone())))
                .emit(request.clone(), &world.emitter())
                .await;
            let occurrences = world
                .store
                .list_occurrences(TriggerOccurrenceFilter::default())
                .await
                .expect("list occurrences");
            if retryable {
                assert!(refused.is_err(), "{refusal:?} fails the emission");
                assert!(occurrences.is_empty(), "{refusal:?} records nothing");
            } else {
                let report = refused.expect("a revoked route refuses only its delivery");
                assert!(
                    matches!(
                        report.deliveries.as_slice(),
                        [TriggerDeliveryEmitReceipt {
                            outcome: TriggerDeliveryEmitOutcome::Failed { .. },
                            ..
                        }]
                    ),
                    "{report:?}"
                );
                assert_eq!(occurrences.len(), 1, "the occurrence is recorded");
            }
            assert!(
                world
                    .store
                    .list_deliveries()
                    .await
                    .expect("list deliveries")
                    .is_empty(),
                "{refusal:?} reserves no delivery"
            );
            assert!(
                world.processes().await.is_empty(),
                "{refusal:?} starts no process"
            );

            let restored = router_with_restorer(&world, StubRestorer::new(None))
                .emit(request, &world.emitter())
                .await
                .expect("the emission runs again");
            let processes = world.processes().await;
            if retryable {
                assert_eq!(restored.started_process_ids().len(), 1, "{restored:?}");
                assert_eq!(
                    processes
                        .iter()
                        .map(|record| &record.id)
                        .collect::<Vec<_>>(),
                    restored.started_process_ids().iter().collect::<Vec<_>>(),
                    "the retry started the delivery's one process"
                );
            } else {
                assert!(
                    restored.deliveries.is_empty(),
                    "the refused delivery holds nothing: {restored:?}"
                );
                assert!(processes.is_empty(), "nothing starts the refused delivery");
            }
        }
    }
}
