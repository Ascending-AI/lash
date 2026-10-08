//! A session close's store-facing steps fail typed and resume (FIG-5307):
//! the law the facade's session-delete failure laws owed once a deletion
//! became the session actor's own close (L6b, FIG-5176).

use super::*;
use crate::store::{MaintenanceFailure, MaintenanceStop, SessionBlobReclaimReport};
use crate::triggers::*;
use lash_core_execution::testing::ProcessRegistryFaults;
use lash_sansio::sync::MutexExt as _;

/// A trigger store whose next `delete_session_subscriptions` answers an
/// injected refusal.
struct TriggerFault {
    inner: Arc<dyn TriggerStore>,
    next: Mutex<Option<crate::PluginError>>,
}

#[async_trait::async_trait]
impl TriggerStore for TriggerFault {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: TriggerCommand,
    ) -> Result<TriggerEffectResult, crate::PluginError> {
        self.inner.execute_command(operation_id, command).await
    }

    async fn list_subscriptions(
        &self,
        filter: TriggerSubscriptionFilter,
    ) -> Result<Vec<TriggerSubscriptionRecord>, crate::PluginError> {
        self.inner.list_subscriptions(filter).await
    }

    async fn subscriptions_changed_since(
        &self,
        cursor: TriggerSubscriptionChangeCursor,
        limit: usize,
    ) -> Result<
        (
            Vec<TriggerSubscriptionChange>,
            TriggerSubscriptionChangeCursor,
        ),
        crate::PluginError,
    > {
        self.inner.subscriptions_changed_since(cursor, limit).await
    }

    async fn list_subscriptions_with_cursor(
        &self,
    ) -> Result<
        (
            Vec<TriggerSubscriptionRecord>,
            TriggerSubscriptionChangeCursor,
        ),
        crate::PluginError,
    > {
        self.inner.list_subscriptions_with_cursor().await
    }

    async fn compact_subscription_tombstones(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, crate::PluginError> {
        self.inner
            .compact_subscription_tombstones(cutoff_epoch_ms)
            .await
    }

    async fn delete_session_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, crate::PluginError> {
        if let Some(error) = self.next.lock_recover().take() {
            return Err(error);
        }
        self.inner.delete_session_subscriptions(session_id).await
    }

    async fn plan_occurrence(
        &self,
        request: &TriggerOccurrenceRequest,
    ) -> Result<TriggerOccurrencePlan, crate::PluginError> {
        self.inner.plan_occurrence(request).await
    }

    async fn list_occurrences(
        &self,
        filter: TriggerOccurrenceFilter,
    ) -> Result<Vec<TriggerOccurrenceRecord>, crate::PluginError> {
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
        process_id: &ProcessId,
    ) -> Result<Vec<TriggerDeliveryReservation>, crate::PluginError> {
        self.inner.list_deliveries_by_process_id(process_id).await
    }

    async fn list_deliveries(&self) -> Result<Vec<TriggerDeliveryReservation>, crate::PluginError> {
        self.inner.list_deliveries().await
    }

    async fn list_delivery_process_ids(&self) -> Result<Vec<ProcessId>, crate::PluginError> {
        self.inner.list_delivery_process_ids().await
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<TriggerDeliveryRetentionCandidate>, crate::PluginError> {
        self.inner.list_delivery_retention_candidates().await
    }

    async fn list_session_owner_ids_for_retention(
        &self,
    ) -> Result<Vec<SessionId>, crate::PluginError> {
        self.inner.list_session_owner_ids_for_retention().await
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[SessionId],
    ) -> Result<TriggerRetentionReconciliationReport, crate::PluginError> {
        self.inner
            .reconcile_trigger_retention(candidates, deleted_session_ids)
            .await
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, crate::PluginError> {
        self.inner
            .delete_delivery_retention_candidates(candidates)
            .await
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> TriggerOccurrenceReclamationResult {
        self.inner
            .reclaim_trigger_occurrences(cutoff_epoch_ms)
            .await
    }

    async fn forget_trigger_tombstones(
        &self,
        written_before_epoch_ms: u64,
    ) -> Result<usize, crate::StoreError> {
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

/// The fault decorators one law arms.
#[derive(Default)]
struct Faults {
    triggers: Mutex<Option<Arc<TriggerFault>>>,
    storage: Mutex<Option<Arc<crate::testing::runtime_helpers::RecordingDeploymentStore>>>,
    registry: Mutex<Option<Arc<ProcessRegistryFaults>>>,
}

/// The injected refusal of the trigger and process-state steps.
fn injected(step: SessionCloseStep) -> crate::PluginError {
    crate::PluginError::Registration(format!("injected {step:?} failure"))
}

/// The partial report the injected storage failure witnessed.
fn partial() -> SessionBlobReclaimReport {
    SessionBlobReclaimReport {
        enumerated_blob_count: 4,
        retained_blob_count: 1,
        deleted_blob_count: 2,
    }
}

/// A close step that calls out of the durable store and meets a failure
/// answers that step's typed error with its source (and, for the storage
/// delete, the partial report the store witnessed); the steps before it
/// stand, nothing after it runs, and the next claim resumes at that step
/// and closes the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_close_step_answers_its_typed_cause_and_the_next_claim_resumes_there() {
    for failing in [
        SessionCloseStep::Triggers,
        SessionCloseStep::Artifacts,
        SessionCloseStep::Tombstone,
    ] {
        let faults = Arc::new(Faults::default());
        let world = {
            let faults = Arc::clone(&faults);
            World::layered(
                "faulted-close",
                DurableSettings::default(),
                Vec::new(),
                move |stores| {
                    let (triggers, storage, registry) =
                        (Arc::clone(&faults), Arc::clone(&faults), faults);
                    stores
                        .map_trigger_store(move |inner| {
                            let fault = Arc::new(TriggerFault {
                                inner,
                                next: Mutex::new(None),
                            });
                            *triggers.triggers.lock_recover() = Some(Arc::clone(&fault));
                            fault
                        })
                        .map_session_store_factory(move |inner| {
                            let recording = Arc::new(
                                crate::testing::runtime_helpers::RecordingDeploymentStore::over(
                                    inner,
                                ),
                            );
                            *storage.storage.lock_recover() = Some(Arc::clone(&recording));
                            recording
                        })
                        .map_process_registry(move |inner| {
                            let faulted = Arc::new(ProcessRegistryFaults::new(inner));
                            *registry.registry.lock_recover() = Some(Arc::clone(&faulted));
                            faulted
                        })
                },
            )
            .await
        };
        world
            .backend
            .session_store_factory()
            .admit_session(
                &lash_core_store::testing::store_fixtures::root_session_request(&world.session),
            )
            .await
            .expect("materialize the session");
        let cx = world.claim().await;
        request_session_close(&world.backend, &world.session)
            .await
            .expect("request the close");
        let mut tx = cx.begin().await.expect("begin");
        begin_session_close(&mut tx, &world.session);
        tx.ack_seen();
        cx.commit(tx, CommitLabel::SESSION_CLOSE_BEGIN)
            .await
            .expect("drain the close request");

        match failing {
            SessionCloseStep::Triggers => {
                let fault = faults
                    .triggers
                    .lock_recover()
                    .clone()
                    .expect("trigger fault");
                *fault.next.lock_recover() = Some(injected(failing));
            }
            SessionCloseStep::Artifacts => {
                faults
                    .storage
                    .lock_recover()
                    .clone()
                    .expect("storage fault")
                    .fail_next_delete(MaintenanceFailure::failed(
                        crate::StoreError::Backend("injected storage failure".to_owned()),
                        partial(),
                    ));
            }
            _ => faults
                .registry
                .lock_recover()
                .clone()
                .expect("registry fault")
                .fail_next_session_delete(injected(failing)),
        }

        let error = run_session_close(&cx, &world.session)
            .await
            .expect_err("the faulted step fails the close's pass");
        match (failing, &error) {
            (
                SessionCloseStep::Triggers,
                super::super::session_close::SessionCloseError::Triggers(source),
            )
            | (
                SessionCloseStep::Tombstone,
                super::super::session_close::SessionCloseError::Process(source),
            ) => {
                assert_eq!(
                    format!("{source:?}"),
                    format!("{:?}", injected(failing)),
                    "{failing:?}: the step keeps its source"
                );
            }
            (
                SessionCloseStep::Artifacts,
                super::super::session_close::SessionCloseError::Storage(failure),
            ) => {
                assert!(
                    matches!(
                        &failure.stop,
                        MaintenanceStop::Failed(crate::StoreError::Backend(message))
                            if message == "injected storage failure"
                    ),
                    "the typed maintenance stop is kept: {failure:?}"
                );
                assert_eq!(failure.partial, partial(), "the partial report is kept");
            }
            other => panic!("{failing:?}: the close answered another step's error: {other:?}"),
        }
        let row = world
            .backend
            .durable()
            .session_close(&world.session)
            .await
            .expect("read the close")
            .expect("the close row");
        assert_eq!(
            row.next(),
            Some(failing),
            "{failing:?}: the steps before the failed one stand, and it is next"
        );
        if failing != SessionCloseStep::Tombstone {
            assert!(
                !matches!(
                    world
                        .backend
                        .session_store_factory()
                        .lookup_session(&world.session)
                        .await
                        .expect("look the session up"),
                    SessionLookup::Deleted
                ),
                "{failing:?}: nothing after the failed step ran"
            );
        }

        let cx = world.claim().await;
        assert_eq!(
            run_session_close(&cx, &world.session)
                .await
                .expect("the next claim resumes the close"),
            Some(SessionCloseExit::Closed),
            "{failing:?}"
        );
        assert!(
            world
                .backend
                .durable()
                .session_close(&world.session)
                .await
                .expect("read the close")
                .expect("the tombstone")
                .is_tombstone(),
            "{failing:?}: the resumed close tombstoned the session"
        );
    }
}
