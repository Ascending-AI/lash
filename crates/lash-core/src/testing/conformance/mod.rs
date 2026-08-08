//! Backend-agnostic conformance suites for durable-backend traits.
//!
//! Each suite is parameterized over a factory that produces a *fresh* backend
//! instance and asserts the trait's contract invariants. Run the same suite
//! against every implementation (the production backend and any in-memory test
//! double) so the contract has one executable source of truth and the doubles
//! can't drift from production behavior.
//!
//! Reopen and recovery laws use distinct outer handles over one substrate.
//! [`runtime_persistence_recovery_laws`] certifies store behavior only across
//! claim, checkpoint, commit, and settlement boundaries. The distinct
//! [`turn_crash_matrix_level_1`] suite executes a real scripted turn through
//! conformance-owned store, provider, and effect-controller decorators; its
//! golden trace generates the crash points and its outcome table supplies the
//! recovery oracle. Backend helper processes run the selected level-2 points
//! under `SIGKILL`, including provider streaming, external-effect outcome loss,
//! and final turn control.
//!
//! Suites panic on the first violated invariant — call them from a
//! `#[tokio::test]`. Embedders with custom backends can run them via
//! `lash::testing::conformance`.

mod artifact_store;
mod attachment_owner;
mod attachment_store;
mod await_event_cold;
mod effect_host;
mod fence_integrity;
mod graph_integrity;
mod helpers;
mod live_replay;
mod observer_intent;
mod process_change_feed;
mod process_continuation_store;
mod process_filters;
mod process_references;
mod process_registry;
mod process_trigger_retention;
mod runtime_persistence;
mod runtime_persistence_state_machine;
mod session_graph_append;
mod session_graph_state_machine;
mod session_store_factory;
mod store_contract_state_machine;
mod store_recovery;
mod trigger_store;
mod turn_control;
mod turn_crash_matrix;
mod wake_delivery;

pub use artifact_store::*;
pub use attachment_owner::*;
pub use attachment_store::*;
pub use await_event_cold::*;
pub use effect_host::*;
pub use fence_integrity::*;
pub use graph_integrity::*;
pub use helpers::*;
pub use live_replay::*;
pub use observer_intent::*;
pub use process_continuation_store::*;
pub use process_registry::*;
pub use process_trigger_retention::*;
pub use runtime_persistence::*;
pub use runtime_persistence_state_machine::*;
pub use session_graph_append::*;
pub use session_graph_state_machine::*;
pub use session_store_factory::*;
pub use store_contract_state_machine::*;
pub use store_recovery::*;
pub use trigger_store::*;
pub use turn_control::*;
pub use turn_crash_matrix::*;
pub use wake_delivery::*;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    AgentFrameReason, AttachmentId, AttachmentIntent, AwaitEventWaitIdentity, DeliveryPolicy,
    EffectHost, ExecutionScope, LiveReplayGapReason, LiveReplayResult, LiveReplayStore,
    LiveReplayStoreError, LiveReplaySubscribeResult, MergeKey, ModelSpec, PluginSessionSnapshot,
    ProtocolEvent, ProtocolTurnOptions, QueuedWorkBatch, QueuedWorkBatchDraft,
    QueuedWorkClaimBoundary, QueuedWorkPayload, Resolution, ResolveOutcome, RuntimeCommit,
    RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeInvocation, RuntimePersistence, RuntimeScope, RuntimeSessionState, RuntimeSubject,
    RuntimeTurnCommitStamp, ScopedEffectController, SessionMeta, SessionNodePayload,
    SessionNodeRecord, SessionObservationEvent, SessionObservationEventPayload, SessionPolicy,
    SessionProcessEventKind, SessionQueueEventKind, SessionRelation, SessionRevision, SlotPolicy,
    StoreError, TokenLedgerEntry, TokenUsage, ToolState, TurnActivity, TurnEvent,
};
use crate::{AttachmentStore, AttachmentStoreError, AttachmentStorePersistence};
use crate::{
    CausalRef, LashSchema, ProcessAwaitOutput, ProcessChange, ProcessChangeCursor,
    ProcessCompletionAuthority, ProcessEventAppendRequest, ProcessEventSemanticsSpec,
    ProcessEventType, ProcessExecutionEnvRef, ProcessIdentity, ProcessInput, ProcessListFilter,
    ProcessLiveReferenceSummary, ProcessProvenance, ProcessRegistration, ProcessRegistry,
    ProcessStatus, ProcessStatusFilter, ProcessValueSelector, ProcessWakeDelivery, ProcessWakeSpec,
    RecoveryDisposition, SessionScope, WaitKind, WaitState,
};
use lash_sansio::{AttachmentCreateMeta, AttachmentTypeMetadata, MediaType};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionStoreFactory;
    use lash_sansio::sync::MutexExt;

    struct InMemoryFenceIntegrityInjector {
        runtime: Arc<crate::InMemorySessionStore>,
        triggers: Arc<crate::InMemoryTriggerStore>,
    }

    struct InMemoryGraphIntegrityInjector {
        runtime: Arc<crate::InMemorySessionStore>,
    }

    #[async_trait::async_trait]
    impl GraphIntegrityInjector for InMemoryGraphIntegrityInjector {
        async fn inject(&self, target: &GraphIntegrityTarget) {
            self.runtime.inject_graph_corruption_for_testing(target);
        }

        async fn load_whole_graph(
            &self,
            _session_id: &str,
        ) -> Result<crate::SessionGraph, crate::StoreError> {
            self.runtime.load_whole_graph_for_testing()
        }
    }

    #[tokio::test]
    async fn in_memory_graph_integrity_conformance() {
        graph_integrity_conformance(|_| async {
            let runtime = Arc::new(crate::InMemorySessionStore::new());
            GraphIntegrityHandles {
                runtime: Arc::clone(&runtime) as Arc<dyn crate::RuntimePersistence>,
                injector: Arc::new(InMemoryGraphIntegrityInjector { runtime }),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn in_memory_leafless_session_ignores_populated_sibling_catalog() {
        let factory = crate::InMemorySessionStoreFactory::new();
        let leafless_request = session_store_request(
            "leafless-sibling",
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
            "populated-sibling",
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

        let read = leafless
            .load_session()
            .await
            .expect("leafless sibling load is isolated")
            .expect("leafless sibling has a durable head");
        assert!(read.graph.nodes.is_empty());
        assert!(read.graph.leaf_node_id.is_none());
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

    #[tokio::test]
    async fn in_memory_fence_integrity_conformance() {
        fence_integrity_conformance(|_| async {
            let runtime = Arc::new(crate::InMemorySessionStore::new());
            let triggers = Arc::new(crate::InMemoryTriggerStore::default());
            FenceIntegrityHandles {
                runtime: Arc::clone(&runtime) as Arc<dyn crate::RuntimePersistence>,
                triggers: Arc::clone(&triggers) as Arc<dyn crate::TriggerStore>,
                injector: Arc::new(InMemoryFenceIntegrityInjector { runtime, triggers }),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn in_memory_attachment_store_satisfies_conformance() {
        attachment_store(
            || Arc::new(crate::InMemoryAttachmentStore::new()) as Arc<dyn AttachmentStore>,
            AttachmentStorePersistence::Ephemeral,
        )
        .await;
    }

    #[tokio::test]
    async fn in_memory_process_execution_env_store_satisfies_conformance() {
        process_execution_env_store(|| {
            Arc::new(crate::InMemoryProcessExecutionEnvStore::new())
                as Arc<dyn crate::ProcessExecutionEnvStore>
        })
        .await;
    }

    #[tokio::test]
    async fn in_memory_process_continuation_store_satisfies_conformance() {
        let storage = Arc::new(crate::TestLocalProcessRegistry::default());
        let registry = Arc::clone(&storage) as Arc<dyn crate::ProcessRegistry>;
        let store = storage as Arc<dyn crate::ProcessContinuationStore>;
        process_continuation_store(registry, store).await;
    }

    #[tokio::test]
    async fn in_memory_trigger_store_satisfies_conformance() {
        // Independent in-memory instances cannot reopen shared state, so the
        // durable-only `trigger_store_reopenable` vector is genuinely N/A.
        trigger_store(|| {
            Arc::new(crate::InMemoryTriggerStore::default()) as Arc<dyn crate::TriggerStore>
        })
        .await;
    }

    #[tokio::test]
    async fn in_memory_live_replay_store_satisfies_conformance() {
        live_replay_store(|| {
            Arc::new(crate::InMemoryLiveReplayStore::default()) as Arc<dyn LiveReplayStore>
        })
        .await;
        live_replay_store_capacity_trim(|| {
            Arc::new(crate::InMemoryLiveReplayStore::with_bounds(
                1,
                Duration::from_secs(120),
            )) as Arc<dyn LiveReplayStore>
        })
        .await;
        live_replay_store_ttl_trim(
            || {
                Arc::new(crate::InMemoryLiveReplayStore::with_bounds(
                    16,
                    Duration::from_millis(1),
                )) as Arc<dyn LiveReplayStore>
            },
            Duration::from_millis(20),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_memory_process_registry_satisfies_conformance() {
        process_registry(|| {
            Arc::new(crate::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_memory_process_trigger_retention_satisfies_conformance() {
        process_trigger_retention(|| async {
            let triggers = Arc::new(crate::InMemoryTriggerStore::default());
            let registry = Arc::new(crate::TestLocalProcessRegistry::default());
            ProcessTriggerRetentionHandles { registry, triggers }
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_memory_store_contract_state_machine_properties() {
        store_contract_state_machine("in-memory", |_| async {
            StoreContractHandles {
                registry: Arc::new(crate::TestLocalProcessRegistry::default())
                    as Arc<dyn ProcessRegistry>,
                runtime: Arc::new(crate::InMemorySessionStore::default())
                    as Arc<dyn RuntimePersistence>,
            }
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_memory_runtime_persistence_state_machine_properties() {
        runtime_persistence_state_machine("in-memory", |_| async {
            RuntimePersistenceStateMachineHandles::create(
                Arc::new(crate::InMemorySessionStoreFactory::new()),
                false,
            )
            .await
            .expect("create in-memory runtime-persistence property handles")
        })
        .await;
    }

    #[tokio::test]
    async fn in_memory_checkpoint_component_refs_survive_cold_reopens() {
        let substrate =
            Arc::new(crate::InMemorySessionStore::default()) as Arc<dyn RuntimePersistence>;
        checkpoint_component_refs_survive_cold_reopens(move || {
            crate::testing::checkpoint_observer::fresh_runtime_persistence_handle(Arc::clone(
                &substrate,
            ))
        })
        .await;
    }

    #[tokio::test]
    async fn in_memory_runtime_persistence_recovery_laws() {
        let substrates = Arc::new(Mutex::new(
            BTreeMap::<String, Arc<dyn RuntimePersistence>>::new(),
        ));
        runtime_persistence_recovery_laws(move |scenario| {
            let mut substrates = substrates.lock_recover();
            let substrate = Arc::clone(
                substrates
                    .entry(scenario.to_string())
                    .or_insert_with(|| Arc::new(crate::InMemorySessionStore::default())),
            );
            crate::testing::checkpoint_observer::fresh_runtime_persistence_handle(substrate)
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_memory_real_turn_crash_trace_is_current() {
        let substrates = Arc::new(Mutex::new(
            BTreeMap::<String, Arc<dyn RuntimePersistence>>::new(),
        ));
        Box::pin(turn_crash_trace_drift_check(move |scenario| {
            let mut substrates = substrates.lock_recover();
            let substrate = Arc::clone(
                substrates
                    .entry(scenario.to_string())
                    .or_insert_with(|| Arc::new(crate::InMemorySessionStore::default())),
            );
            crate::testing::checkpoint_observer::fresh_runtime_persistence_handle(substrate)
        }))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_memory_real_turn_crash_matrix() {
        let substrates = Arc::new(Mutex::new(
            BTreeMap::<String, Arc<dyn RuntimePersistence>>::new(),
        ));
        Box::pin(turn_crash_matrix_level_1(move |scenario| {
            let mut substrates = substrates.lock_recover();
            let substrate = Arc::clone(
                substrates
                    .entry(scenario.to_string())
                    .or_insert_with(|| Arc::new(crate::InMemorySessionStore::default())),
            );
            crate::testing::checkpoint_observer::fresh_runtime_persistence_handle(substrate)
        }))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_memory_session_graph_state_machine_properties() {
        session_graph_state_machine("in-memory", |_| async {
            Arc::new(crate::InMemorySessionStoreFactory::new())
                as Arc<dyn crate::SessionStoreFactory>
        })
        .await;
    }

    #[tokio::test]
    async fn in_memory_wake_delivery_crash_matrix() {
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
        ) as Arc<dyn ProcessRegistry>;
        let factory = Arc::new(crate::InMemorySessionStoreFactory::with_clock(
            Arc::clone(&clock) as Arc<dyn crate::Clock>,
        )) as Arc<dyn crate::SessionStoreFactory>;
        wake_delivery_crash_matrix(factory, registry, clock).await;
    }

    #[tokio::test]
    async fn in_memory_session_store_factory_satisfies_conformance() {
        session_store_factory(
            || {
                Arc::new(crate::InMemorySessionStoreFactory::new())
                    as Arc<dyn crate::SessionStoreFactory>
            },
            || {
                Arc::new(crate::InMemorySessionStore::default())
                    as Arc<dyn crate::RuntimePersistence>
            },
        )
        .await;
    }

    #[tokio::test]
    async fn in_memory_fork_observer_intent_transient_failure_conformance() {
        fork_observer_intent_transient_failure(Arc::new(crate::InMemorySessionStoreFactory::new()))
            .await;
    }

    #[tokio::test]
    async fn in_memory_session_graph_append_branch_liveness() {
        session_graph_append_branch_liveness(Arc::new(crate::InMemorySessionStoreFactory::new())
            as Arc<dyn crate::SessionStoreFactory>)
        .await;
    }

    #[tokio::test]
    async fn in_memory_session_store_uses_injected_clock_for_expiry() {
        let clock = Arc::new(crate::testing::TestClock::new(10_000));
        let store = Arc::new(crate::InMemorySessionStore::with_clock(clock.clone()))
            as Arc<dyn crate::RuntimePersistence>;
        runtime_persistence_clock_expiry(store, |duration_ms| clock.advance(duration_ms)).await;
    }

    #[tokio::test]
    async fn inline_effect_host_satisfies_conformance() {
        effect_host(|| Arc::new(crate::InlineEffectHost::default())).await;
        effect_host_await_events(|| Arc::new(crate::InlineEffectHost::default())).await;
        turn_work_driver(Arc::new(crate::InlineEffectHost::default())).await;
    }

    #[tokio::test]
    async fn recording_effect_host_records_selected_scope_and_envelope() {
        let host = RecordingEffectHost::default();
        let scope = ExecutionScope::runtime_operation("trigger:button-1");
        let scoped = host.scoped(scope.clone()).expect("scoped controller");
        let envelope = RuntimeEffectEnvelope::new(
            crate::RuntimeInvocation::effect(
                RuntimeScope::new("session-1"),
                "sleep-effect",
                RuntimeEffectKind::Sleep,
                "trigger:button-1:sleep-effect",
            ),
            RuntimeEffectCommand::Sleep { duration_ms: 0 },
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
        assert_eq!(records[0].runtime_scope, RuntimeScope::new("session-1"));
        assert_eq!(records[0].effect_id, "sleep-effect");
        assert_eq!(records[0].effect_kind, RuntimeEffectKind::Sleep);
        assert_eq!(
            records[0].replay_key.as_deref(),
            Some("trigger:button-1:sleep-effect")
        );
    }
}
