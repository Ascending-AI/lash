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
mod process_coordination;
mod process_filters;
mod process_references;
mod process_registry;
mod runtime_persistence;
mod session_store_factory;
mod trigger_store;
mod turn_control;

pub use artifact_store::*;
pub use attachment_owner::*;
pub use attachment_store::*;
pub use await_event_cold::*;
pub use effect_host::*;
pub use helpers::*;
pub use live_replay::*;
pub use process_registry::*;
pub use runtime_persistence::*;
pub use session_store_factory::*;
pub use trigger_store::*;
pub use turn_control::*;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    AbandonEvidence, AbandonRequest, AbandonWriter, LashSchema, ProcessAwaitOutput,
    ProcessChangeCursor, ProcessCompletionAuthority, ProcessEventAppendRequest,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessExecutionEnvRef,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessHandleDescriptor, ProcessIdentity,
    ProcessInput, ProcessLeaseCompletion, ProcessListFilter, ProcessLiveReferenceSummary,
    ProcessProvenance, ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessStartOutcome,
    ProcessStarted, ProcessStatus, ProcessStatusFilter, ProcessTerminalState, ProcessValueSelector,
    ProcessWakeDedupeKey, ProcessWakeDelivery, ProcessWakeSpec, RecoveryDisposition, SessionScope,
    SessionScopeId, WaitKind, WaitState,
};
use crate::{
    AgentFrameReason, AttachmentId, AttachmentIntent, AwaitEventWaitIdentity, CausalRef,
    DeliveryPolicy, EffectHost, ExecutionScope, LiveReplayGapReason, LiveReplayResult,
    LiveReplayStore, LiveReplayStoreError, LiveReplaySubscribeResult, MergeKey, ModelSpec,
    PluginSessionSnapshot, ProtocolEvent, ProtocolTurnOptions, QueuedWorkBatch,
    QueuedWorkBatchDraft, QueuedWorkClaimBoundary, QueuedWorkPayload, Resolution, ResolveOutcome,
    RuntimeCommit, RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeInvocation, RuntimePersistence, RuntimeScope, RuntimeSessionState, RuntimeSubject,
    RuntimeTurnCommitStamp, ScopedEffectController, SessionMeta, SessionNodePayload,
    SessionNodeRecord, SessionObservationEvent, SessionObservationEventPayload, SessionPolicy,
    SessionProcessEventKind, SessionQueueEventKind, SessionReadScope, SessionRelation,
    SessionRevision, SlotPolicy, StoreError, TokenLedgerEntry, TokenUsage, ToolState, TurnActivity,
    TurnEvent,
};
use crate::{AttachmentStore, AttachmentStoreError, AttachmentStorePersistence};
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

    #[tokio::test]
    async fn in_memory_session_store_factory_satisfies_conformance() {
        session_store_factory(|| {
            Arc::new(crate::InMemorySessionStoreFactory::new())
                as Arc<dyn crate::SessionStoreFactory>
        })
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
