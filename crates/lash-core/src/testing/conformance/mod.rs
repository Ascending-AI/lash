//! Backend-agnostic conformance suites for durable-backend traits.
//!
//! Each suite is parameterized over a factory that produces a *fresh* backend
//! instance and asserts the trait's contract invariants. Run the same suite
//! against every implementation (the production backend and any in-memory test
//! double) so the contract has one executable source of truth and the doubles
//! can't drift from production behavior.
//!
//! Suites panic on the first violated invariant — call them from a
//! `#[tokio::test]`. Embedders with custom backends can run them via
//! `lash::testing::conformance`.

mod artifact_store;
mod attachment_owner;
mod attachment_store;
mod await_event_cold;
mod effect_host;
mod helpers;
mod live_replay;
mod process_change_feed;
mod process_continuation_store;
mod process_filters;
mod process_references;
mod process_registry;
mod process_trigger_retention;
mod runtime_persistence;
mod session_graph_append;
mod session_store_factory;
mod store_contract_state_machine;
mod trigger_store;
mod turn_control;
mod wake_delivery;

pub use artifact_store::*;
pub use attachment_owner::*;
pub use attachment_store::*;
pub use await_event_cold::*;
pub use effect_host::*;
pub use helpers::*;
pub use live_replay::*;
pub use process_continuation_store::*;
pub use process_registry::*;
pub use process_trigger_retention::*;
pub use runtime_persistence::*;
pub use session_graph_append::*;
pub use session_store_factory::*;
pub use store_contract_state_machine::*;
pub use trigger_store::*;
pub use turn_control::*;
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

    #[derive(Debug)]
    struct ConformanceClock(std::sync::atomic::AtomicU64);

    impl ConformanceClock {
        fn new(timestamp_ms: u64) -> Self {
            Self(std::sync::atomic::AtomicU64::new(timestamp_ms))
        }

        fn advance(&self, duration_ms: u64) {
            self.0
                .fetch_add(duration_ms, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl crate::Clock for ConformanceClock {
        fn now(&self) -> std::time::Instant {
            std::time::Instant::now()
        }

        fn timestamp_ms(&self) -> u64 {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn timestamp_rfc3339(&self) -> String {
            self.timestamp_datetime().to_rfc3339()
        }

        fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::from(
                std::time::UNIX_EPOCH + Duration::from_millis(self.timestamp_ms()),
            )
        }

        async fn sleep(&self, duration: Duration) {
            tokio::time::sleep(duration).await;
        }

        async fn sleep_until(&self, deadline: std::time::Instant) {
            tokio::time::sleep_until(deadline.into()).await;
        }
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

    #[tokio::test]
    async fn in_memory_wake_delivery_crash_matrix() {
        let registry = Arc::new(
            crate::TestLocalProcessRegistry::default().with_wake_delivery_config(
                crate::WakeDeliveryConfig::new(10_000)
                    .expect("valid wake expiry")
                    .with_enqueuing_stale_after_ms(25)
                    .expect("valid short stale-claim age"),
            ),
        ) as Arc<dyn ProcessRegistry>;
        let factory = Arc::new(crate::InMemorySessionStoreFactory::new())
            as Arc<dyn crate::SessionStoreFactory>;
        wake_delivery_crash_matrix(factory, registry).await;
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
    async fn in_memory_session_graph_append_branch_liveness() {
        session_graph_append_branch_liveness(Arc::new(crate::InMemorySessionStoreFactory::new())
            as Arc<dyn crate::SessionStoreFactory>)
        .await;
    }

    #[tokio::test]
    async fn in_memory_session_store_uses_injected_clock_for_expiry() {
        let clock = Arc::new(ConformanceClock::new(10_000));
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
