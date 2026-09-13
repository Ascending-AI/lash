use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::*;
use lash_sansio::{EffectAddress, SessionId};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionStoreFactory;
    use lash_sansio::sync::MutexExt;
    use pretty_assertions::assert_eq;

    struct InMemoryFenceIntegrityInjector {
        runtime: Arc<crate::InMemorySessionStore>,
        triggers: Arc<crate::InMemoryTriggerStore>,
    }

    struct InMemoryGraphIntegrityInjector {
        runtime: Arc<crate::InMemorySessionStore>,
    }

    struct InMemoryTriggerFaultFixture {
        store: Arc<crate::InMemoryTriggerStore>,
    }

    #[async_trait::async_trait]
    impl LegacyTriggerMutationReceiptInjector for InMemoryTriggerFaultFixture {
        async fn insert_legacy_receipt(
            &self,
            operation_id: &str,
            request_fingerprint: &str,
            result_json: &str,
            created_at_ms: u64,
        ) {
            self.store.insert_legacy_mutation_receipt_for_testing(
                operation_id,
                request_fingerprint,
                result_json,
                created_at_ms,
            );
        }

        async fn receipt_exists(&self, operation_id: &str) -> bool {
            self.store.has_mutation_receipt_for_testing(operation_id)
        }
    }

    #[async_trait::async_trait]
    impl TriggerOccurrenceRetentionFaultInjector for InMemoryTriggerFaultFixture {
        async fn fail_occurrence_delete(&self, occurrence_id: &str) {
            self.store
                .fail_occurrence_delete_for_testing(occurrence_id.to_string());
        }

        async fn clear_occurrence_delete_failure(&self) {
            self.store.clear_occurrence_delete_failure_for_testing();
        }
    }

    #[async_trait::async_trait]
    impl GraphIntegrityInjector for InMemoryGraphIntegrityInjector {
        async fn inject(&self, target: &GraphIntegrityTarget) {
            self.runtime.inject_graph_corruption_for_testing(target);
        }

        async fn load_whole_graph(
            &self,
            _session_id: &SessionId,
        ) -> Result<crate::SessionGraph, crate::StoreError> {
            self.runtime.load_whole_graph_for_testing()
        }
    }

    crate::graph_integrity_tests!({
        ((), |_| async {
            let runtime = Arc::new(crate::InMemorySessionStore::new());
            GraphIntegrityHandles {
                runtime: Arc::clone(&runtime) as Arc<dyn crate::RuntimePersistence>,
                injector: Arc::new(InMemoryGraphIntegrityInjector { runtime }),
            }
        })
    });

    crate::session_read_view_tests!({
        let clock = Arc::new(crate::testing::TestClock::new(1_800_000_000_000));
        let factory = Arc::new(crate::InMemorySessionStoreFactory::with_clock(
            Arc::clone(&clock) as Arc<dyn crate::Clock>,
        ));
        ((), factory, move || clock.advance(1))
    });

    #[tokio::test]
    async fn in_memory_leafless_session_ignores_populated_sibling_catalog() {
        let factory = crate::InMemorySessionStoreFactory::new();
        let leafless_request = session_store_request(
            &SessionId::from("leafless-sibling"),
            "graph-integrity-model",
            crate::SessionRelation::Root,
        );
        let leafless = factory
            .create_store(&leafless_request)
            .await
            .expect("create leafless sibling");
        leafless
            .admit_and_bind_session(&crate::SessionBinding::from_create_request(
                &leafless_request,
            ))
            .await
            .expect("bind leafless sibling");
        let leafless_state = crate::RuntimeSessionState {
            session_id: leafless_request.session_id.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        leafless
            .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
                &leafless_state,
                &[],
            ))
            .await
            .expect("seed leafless sibling head");

        let populated_request = session_store_request(
            &SessionId::from("populated-sibling"),
            "graph-integrity-model",
            crate::SessionRelation::Root,
        );
        let populated = factory
            .create_store(&populated_request)
            .await
            .expect("create populated sibling");
        let mut state = crate::RuntimeSessionState {
            session_id: populated_request.session_id.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        state.ensure_agent_frame_initialized();
        populated
            .admit_and_bind_session(&crate::SessionBinding::from_create_request(
                &populated_request,
            ))
            .await
            .expect("bind populated sibling");
        populated
            .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await
            .expect("seed populated sibling");

        let late_leafless_request = session_store_request(
            &SessionId::from("late-leafless-sibling"),
            "graph-integrity-model",
            crate::SessionRelation::Root,
        );
        let late_leafless = factory
            .create_store(&late_leafless_request)
            .await
            .expect("create leafless sibling after populated history");
        late_leafless
            .admit_and_bind_session(&crate::SessionBinding::from_create_request(
                &late_leafless_request,
            ))
            .await
            .expect("bind late leafless sibling");
        let late_leafless_state = crate::RuntimeSessionState {
            session_id: late_leafless_request.session_id.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        late_leafless
            .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
                &late_leafless_state,
                &[],
            ))
            .await
            .expect("seed late leafless sibling head");

        let read = leafless
            .load_session()
            .await
            .expect("leafless sibling load is isolated")
            .expect("leafless sibling has a durable head");
        assert!(read.graph.nodes.is_empty());
        assert!(read.graph.leaf_node_id.is_none());

        let late_read = late_leafless
            .load_session()
            .await
            .expect("late leafless sibling load is isolated")
            .expect("late leafless sibling has a durable head");
        assert!(late_read.graph.nodes.is_empty());
        assert!(late_read.graph.leaf_node_id.is_none());
    }

    #[async_trait::async_trait]
    impl FenceIntegrityInjector for InMemoryFenceIntegrityInjector {
        async fn inject_raw_value(&self, target: &FenceIntegrityTarget, value: i64) {
            match target {
                FenceIntegrityTarget::QueuedWorkClaimFence { batch_id } => {
                    self.runtime.inject_raw_counter_for_testing(
                        "queued_work_claim_fencing_token",
                        batch_id,
                        value,
                    )
                }
                FenceIntegrityTarget::SessionHeadRevision { session_id } => self
                    .runtime
                    .inject_raw_counter_for_testing("session_head_revision", session_id, value),
                FenceIntegrityTarget::SessionLeaseFencingToken { session_id } => {
                    self.runtime.inject_raw_counter_for_testing(
                        "session_lease_fencing_token",
                        session_id,
                        value,
                    )
                }
                FenceIntegrityTarget::TriggerRevision { subscription_id } => self
                    .triggers
                    .inject_revision_for_testing(subscription_id, value),
            }
        }

        async fn observe_raw_value(
            &self,
            target: &FenceIntegrityTarget,
        ) -> FenceIntegrityObservation {
            let snapshot = match target {
                FenceIntegrityTarget::QueuedWorkClaimFence { batch_id } => self
                    .runtime
                    .raw_counter_snapshot_for_testing("queued_work_claim_fencing_token", batch_id),
                FenceIntegrityTarget::SessionHeadRevision { session_id } => self
                    .runtime
                    .raw_counter_snapshot_for_testing("session_head_revision", session_id),
                FenceIntegrityTarget::SessionLeaseFencingToken { session_id } => self
                    .runtime
                    .raw_counter_snapshot_for_testing("session_lease_fencing_token", session_id),
                FenceIntegrityTarget::TriggerRevision { subscription_id } => {
                    self.triggers.revision_snapshot_for_testing(subscription_id)
                }
            };
            let value = match target {
                FenceIntegrityTarget::TriggerRevision { .. } => {
                    serde_json::from_str::<serde_json::Value>(&snapshot)
                        .expect("decode in-memory trigger snapshot")["revision"]
                        .as_i64()
                        .expect("trigger revision is signed-domain compatible")
                }
                _ => snapshot
                    .split(':')
                    .next_back()
                    .filter(|_| snapshot.starts_with("defect:"))
                    .or_else(|| snapshot.split(':').next())
                    .expect("counter snapshot value")
                    .parse()
                    .expect("parse counter snapshot value"),
            };
            FenceIntegrityObservation {
                value,
                mutation_fingerprint: snapshot,
            }
        }
    }

    crate::fence_integrity_tests!({
        ((), |_| async {
            let runtime = Arc::new(crate::InMemorySessionStore::new());
            let triggers = Arc::new(crate::InMemoryTriggerStore::default());
            FenceIntegrityHandles {
                runtime: Arc::clone(&runtime) as Arc<dyn crate::RuntimePersistence>,
                triggers: Arc::clone(&triggers) as Arc<dyn crate::TriggerStore>,
                injector: Arc::new(InMemoryFenceIntegrityInjector { runtime, triggers }),
            }
        })
    });

    // No signed-counter invocation: that law is specific to SQL signed-write conversion.

    crate::attachment_store_tests!({
        (
            (),
            || Arc::new(crate::InMemoryAttachmentStore::new()) as Arc<dyn AttachmentStore>,
            AttachmentStorePersistence::Ephemeral,
        )
    });

    crate::process_execution_env_store_tests!({
        ((), || {
            Arc::new(crate::InMemoryProcessExecutionEnvStore::new())
                as Arc<dyn crate::ProcessExecutionEnvStore>
        })
    });

    crate::process_continuation_store_tests!({
        let storage = Arc::new(crate::TestLocalProcessRegistry::default());
        let registry = Arc::clone(&storage) as Arc<dyn crate::ProcessRegistry>;
        let store = storage as Arc<dyn crate::ProcessContinuationStore>;
        ((), registry, store)
    });

    crate::trigger_store_tests!({
        ((), || {
            Arc::new(crate::InMemoryTriggerStore::default()) as Arc<dyn crate::TriggerStore>
        })
    });

    // No reopenable trigger-store invocation: independent in-memory instances share no state.
    // No occurrence-listing corruption invocation: the in-memory store retains typed records.

    crate::trigger_retention_fault_tests!({
        let store = Arc::new(crate::InMemoryTriggerStore::default());
        let fixture = Arc::new(InMemoryTriggerFaultFixture {
            store: Arc::clone(&store),
        });
        (
            (),
            store as Arc<dyn crate::TriggerStore>,
            Arc::clone(&fixture) as Arc<dyn LegacyTriggerMutationReceiptInjector>,
            fixture as Arc<dyn TriggerOccurrenceRetentionFaultInjector>,
        )
    });

    crate::live_replay_tests!({
        let original = crate::InMemoryLiveReplayStore::default();
        let preserved = original.reopen_preserving_history();
        (
            (),
            || Arc::new(crate::InMemoryLiveReplayStore::default()) as Arc<dyn LiveReplayStore>,
            || {
                Arc::new(crate::InMemoryLiveReplayStore::with_bounds(
                    1,
                    Duration::from_secs(120),
                )) as Arc<dyn LiveReplayStore>
            },
            || {
                Arc::new(crate::InMemoryLiveReplayStore::with_bounds(
                    16,
                    Duration::from_millis(1),
                )) as Arc<dyn LiveReplayStore>
            },
            Duration::from_millis(20),
            (
                Arc::new(original) as Arc<dyn LiveReplayStore>,
                Arc::new(crate::InMemoryLiveReplayStore::default()) as Arc<dyn LiveReplayStore>,
                Arc::new(preserved) as Arc<dyn LiveReplayStore>,
            ),
        )
    });

    crate::process_trigger_retention_tests!({
        ((), || async {
            let triggers = Arc::new(crate::InMemoryTriggerStore::default());
            let registry = Arc::new(crate::TestLocalProcessRegistry::default());
            let sessions = Arc::new(crate::InMemorySessionStoreFactory::default());
            ProcessTriggerRetentionHandles {
                registry,
                triggers,
                sessions,
            }
        })
    });

    crate::store_contract_state_machine_tests!({
        ((), "in-memory", |_, _| async {
            StoreContractHandles {
                registry: Arc::new(crate::TestLocalProcessRegistry::default())
                    as Arc<dyn ProcessRegistry>,
                runtime: Arc::new(crate::InMemorySessionStore::default())
                    as Arc<dyn RuntimePersistence>,
            }
        })
    });

    crate::runtime_persistence_state_machine_tests!({
        ((), "in-memory", |_| async {
            RuntimePersistenceStateMachineHandles::create(
                Arc::new(crate::InMemorySessionStoreFactory::new()),
                false,
            )
            .await
            .expect("create in-memory runtime-persistence property handles")
        })
    });

    crate::checkpoint_component_reopen_tests!({
        let substrate =
            Arc::new(crate::InMemorySessionStore::default()) as Arc<dyn RuntimePersistence>;
        ((), move || {
            crate::testing::checkpoint_observer::fresh_runtime_persistence_handle(Arc::clone(
                &substrate,
            ))
        })
    });

    crate::store_recovery_tests!({
        let clock = Arc::new(crate::testing::TestClock::new(10_000));
        let store_clock = Arc::clone(&clock);
        let substrates = Arc::new(Mutex::new(
            BTreeMap::<String, Arc<dyn RuntimePersistence>>::new(),
        ));
        (
            (),
            move |scenario: &str| {
                let mut substrates = substrates.lock_recover();
                let store_clock = Arc::clone(&store_clock);
                let substrate =
                    Arc::clone(substrates.entry(scenario.to_string()).or_insert_with(|| {
                        Arc::new(crate::InMemorySessionStore::with_clock(
                            store_clock as Arc<dyn crate::Clock>,
                        ))
                    }));
                crate::testing::checkpoint_observer::fresh_runtime_persistence_handle(substrate)
            },
            StoreRecoveryLeaseTiming::controlled(move |duration_ms| clock.advance(duration_ms)),
        )
    });

    crate::turn_crash_matrix_tests!({
        let substrates = Arc::new(Mutex::new(
            BTreeMap::<String, Arc<dyn RuntimePersistence>>::new(),
        ));
        let make_substrates = Arc::clone(&substrates);
        (
            substrates,
            move |scenario: &str| {
                let mut substrates = make_substrates.lock_recover();
                let substrate = Arc::clone(
                    substrates
                        .entry(scenario.to_string())
                        .or_insert_with(|| Arc::new(crate::InMemorySessionStore::default())),
                );
                crate::testing::checkpoint_observer::fresh_runtime_persistence_handle(substrate)
            },
            |_: &str| ConformanceInvocation::native(),
        )
    });

    crate::session_graph_state_machine_tests!({
        ((), "in-memory", |_| async {
            Arc::new(crate::InMemorySessionStoreFactory::new())
                as Arc<dyn crate::SessionStoreFactory>
        })
    });

    crate::wake_delivery_crash_tests!({
        let clock = Arc::new(crate::testing::TestClock::new(1_800_000_000_000));
        let registry = Arc::new(
            crate::TestLocalProcessRegistry::default()
                .with_clock(Arc::clone(&clock) as Arc<dyn crate::Clock>)
                .with_wake_delivery_config(
                    crate::WakeDeliveryConfig::new(10_000)
                        .expect("valid wake expiry")
                        .with_enqueuing_stale_after_ms(25)
                        .expect("valid short stale-claim age"),
                ),
        ) as Arc<dyn crate::ConformanceProcessRegistry>;
        let factory = Arc::new(crate::InMemorySessionStoreFactory::with_clock(
            Arc::clone(&clock) as Arc<dyn crate::Clock>,
        )) as Arc<dyn crate::SessionStoreFactory>;
        let process_work = Arc::new(crate::NativeProcessWork::for_registry(
            registry.clone() as Arc<dyn ProcessRegistry>
        ));
        (
            (),
            factory,
            registry,
            clock,
            process_work,
            ProcessTerminalWaitWitness::Direct,
            || async {},
            || async {},
        )
    });

    crate::wake_delivery_ordering_tests!({
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        let process_work = Arc::new(crate::NativeProcessWork::for_registry(
            Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
        ));
        (
            (),
            Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
            registry as Arc<dyn WakeDeliveryOrderingGroupFaultInjector>,
            process_work,
            ProcessTerminalWaitWitness::Direct,
            || async {},
            || async {},
        )
    });

    crate::session_store_factory_tests!({
        let unbound = crate::InMemorySessionStore::default();
        (
            (),
            "in-memory",
            Some(Arc::new(unbound) as Arc<dyn crate::store::StoreMaintenance>),
            || {
                Arc::new(crate::InMemorySessionStoreFactory::new())
                    as Arc<dyn crate::store::ConformanceSessionStoreFactory>
            },
        )
    });

    #[tokio::test]
    async fn session_config_settlement_timeout_is_typed() {
        Box::pin(crate::session_config_settlement_timeout_is_typed()).await;
    }

    #[tokio::test]
    async fn cancelled_session_config_settlement_is_typed() {
        crate::cancelled_session_config_settlement_is_typed().await;
    }

    #[tokio::test]
    async fn superseded_config_settlement_adopts_the_newer_head() {
        Box::pin(crate::superseded_config_settlement_adopts_the_newer_head()).await;
    }

    crate::session_delete_blob_reclaim_tests!({
        ((), "in-memory", || {
            let factory = Arc::new(crate::InMemorySessionStoreFactory::new());
            SessionDeleteBlobHandles {
                factory: Arc::clone(&factory) as Arc<dyn crate::SessionStoreFactory>,
                probe: factory as Arc<dyn SessionDeleteBlobProbe>,
            }
        })
    });

    crate::fresh_session_admission_tests!({
        ((), |_session_id: &str| {
            Arc::new(crate::InMemorySessionStore::default()) as Arc<dyn crate::RuntimePersistence>
        })
    });

    crate::observer_intent_tests!({
        (
            (),
            Arc::new(crate::InMemorySessionStoreFactory::new())
                as Arc<dyn crate::SessionStoreFactory>,
        )
    });

    crate::session_graph_append_tests!({
        (
            (),
            Arc::new(crate::InMemorySessionStoreFactory::new())
                as Arc<dyn crate::SessionStoreFactory>,
        )
    });

    crate::runtime_persistence_clock_tests!({
        let clock = Arc::new(crate::testing::TestClock::new(10_000));
        let store = Arc::new(crate::InMemorySessionStore::with_clock(clock.clone()))
            as Arc<dyn crate::RuntimePersistence>;
        (
            (),
            store,
            move |duration_ms| clock.advance(duration_ms),
            |_store| async {},
        )
    });

    struct InMemorySessionExecutionLeaseRenewalZeroRowInjector {
        store: Arc<crate::InMemorySessionStore>,
    }

    #[async_trait::async_trait]
    impl SessionExecutionLeaseRenewalZeroRowInjector
        for InMemorySessionExecutionLeaseRenewalZeroRowInjector
    {
        async fn arm(&self, session_id: &SessionId) {
            assert_eq!(session_id, "zero-row-session-lease-renewal");
            self.store
                .force_next_session_execution_lease_renewal_zero_match();
        }

        async fn disarm(&self) {}
    }

    crate::session_execution_lease_renewal_tests!({
        let store = Arc::new(crate::InMemorySessionStore::new());
        (
            (),
            SessionExecutionLeaseRenewalZeroRowHandles {
                store: Arc::clone(&store) as Arc<dyn RuntimePersistence>,
                injector: Arc::new(InMemorySessionExecutionLeaseRenewalZeroRowInjector { store }),
            },
        )
    });

    crate::effect_host_tests!({
        ((), || {
            Arc::new(crate::NativeEffectHost::default()) as Arc<dyn crate::EffectHost>
        })
    });

    // No replay/retirement/fencing macros: the native host owns no durable journal.

    crate::effect_host_await_event_tests!({
        ((), || {
            Arc::new(crate::NativeEffectHost::default()) as Arc<dyn crate::EffectHost>
        })
    });

    // No cold-AwaitEvent invocation: the native host has no durable reopen boundary.

    crate::turn_work_driver_tests!({
        (
            (),
            Arc::new(crate::NativeEffectHost::default()) as Arc<dyn crate::EffectHost>,
            crate::await_event_registration_observed,
        )
    });

    #[tokio::test]
    async fn non_enumerable_effect_host_reports_typed_unsupported() {
        let error = RecordingEffectHost::default()
            .list_outstanding_await_event_keys(&SessionId::from("unsupported-session"))
            .await
            .expect_err("the default host implementation must not claim an empty registry");
        assert_eq!(error.code, crate::RuntimeErrorCode::AwaitEventUnsupported);
    }

    #[tokio::test]
    async fn recording_effect_host_records_selected_scope_and_envelope() {
        let host = RecordingEffectHost::default();
        let scope = ExecutionScope::runtime_operation("trigger:button-1");
        let scoped = host.scoped(scope.clone()).expect("scoped controller");
        let envelope = RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                EffectAddress::new(scope.clone(), "trigger:button-1:sleep-effect")
                    .expect("valid recording address"),
                RuntimeAttribution::for_session("session-1"),
                "sleep-effect",
            ),
            RuntimeEffectCommand::Sleep {
                spec: lash_core::SleepSpec::For { duration_ms: 0 },
            },
        );

        let outcome = scoped
            .controller()
            .execute_effect(envelope, RuntimeEffectLocalExecutor::unavailable())
            .await
            .expect("execute sleep");

        assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
        assert_eq!(host.selected_scopes(), vec![scope.clone()]);
        let records = host.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].execution_scope, scope);
        assert_eq!(
            records[0].runtime_attribution,
            RuntimeAttribution::for_session("session-1")
        );
        assert_eq!(records[0].effect_id, "sleep-effect");
        assert_eq!(records[0].effect_kind, RuntimeEffectKind::Sleep);
        assert_eq!(
            records[0].replay_key.as_deref(),
            Some("trigger:button-1:sleep-effect")
        );
    }
}
