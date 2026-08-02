use super::{
    WorkbenchCronRequest, classified_embed_handler_error, cron_occurrence_key,
    emit_cron_occurrence_with_effect_controller,
};
use crate::AppError;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct CountingProcessEffectController {
    process_starts: AtomicUsize,
}

impl lash_core::AwaitEventResolver for CountingProcessEffectController {}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for CountingProcessEffectController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        if matches!(
            &envelope.command,
            lash_core::RuntimeEffectCommand::Process { command }
                if matches!(command.as_ref(), lash_core::ProcessCommand::Start { .. })
        ) {
            self.process_starts.fetch_add(1, Ordering::SeqCst);
        }
        local_executor.execute(envelope).await
    }
}

struct OccurrenceFailureTriggerStore {
    inner: lash_core::InMemoryTriggerStore,
    failure: lash::plugins::PluginError,
}

impl OccurrenceFailureTriggerStore {
    fn new(failure: lash::plugins::PluginError) -> Self {
        Self {
            inner: lash_core::InMemoryTriggerStore::new(),
            failure,
        }
    }
}

#[async_trait::async_trait]
impl lash::triggers::TriggerStore for OccurrenceFailureTriggerStore {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: lash::triggers::TriggerCommand,
    ) -> Result<lash::triggers::TriggerEffectResult, lash::plugins::PluginError> {
        self.inner.execute_command(operation_id, command).await
    }

    async fn list_subscriptions(
        &self,
        filter: lash::triggers::TriggerSubscriptionFilter,
    ) -> Result<Vec<lash::triggers::TriggerSubscriptionRecord>, lash::plugins::PluginError> {
        self.inner.list_subscriptions(filter).await
    }

    async fn delete_session_subscriptions(
        &self,
        session_id: &str,
    ) -> Result<usize, lash::plugins::PluginError> {
        self.inner.delete_session_subscriptions(session_id).await
    }

    async fn ingest_occurrence(
        &self,
        _request: lash::triggers::TriggerOccurrenceRequest,
    ) -> Result<lash_core::TriggerIngressResult, lash::plugins::PluginError> {
        Err(self.failure.clone())
    }

    async fn list_occurrences(
        &self,
        filter: lash::triggers::TriggerOccurrenceFilter,
    ) -> Result<Vec<lash::triggers::TriggerOccurrenceRecord>, lash::plugins::PluginError> {
        self.inner.list_occurrences(filter).await
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Vec<lash::triggers::TriggerDeliveryReservation>, lash::plugins::PluginError> {
        self.inner
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<Vec<lash::triggers::TriggerDeliveryReservation>, lash::plugins::PluginError> {
        self.inner
            .list_deliveries_by_subscription_id(subscription_id)
            .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &str,
    ) -> Result<Vec<lash::triggers::TriggerDeliveryReservation>, lash::plugins::PluginError> {
        self.inner.list_deliveries_by_process_id(process_id).await
    }

    async fn list_deliveries(
        &self,
    ) -> Result<Vec<lash::triggers::TriggerDeliveryReservation>, lash::plugins::PluginError> {
        self.inner.list_deliveries().await
    }

    async fn list_delivery_process_ids(&self) -> Result<Vec<String>, lash::plugins::PluginError> {
        self.inner.list_delivery_process_ids().await
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<lash_core::TriggerDeliveryRetentionCandidate>, lash::plugins::PluginError> {
        self.inner.list_delivery_retention_candidates().await
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[lash_core::TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, lash::plugins::PluginError> {
        self.inner
            .delete_delivery_retention_candidates(candidates)
            .await
    }

    async fn prune_mutation_receipts(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, lash::plugins::PluginError> {
        self.inner.prune_mutation_receipts(cutoff_epoch_ms).await
    }
}

#[test]
fn cron_occurrence_key_is_unique_per_tick() {
    let job = "session:source:cron.Schedule:sha256:abc";
    let first = cron_occurrence_key(job, "2026-06-09T22:30:30+00:00");
    let second = cron_occurrence_key(job, "2026-06-09T22:31:00+00:00");
    // Two ticks of one job must not collide: a constant key makes the
    // second tick fail its trigger emit with an idempotency conflict
    // before re-arming, killing the schedule after exactly one fire.
    assert_ne!(first, second);
    // A retried tick (same journaled fired_at) must dedupe.
    assert_eq!(first, cron_occurrence_key(job, "2026-06-09T22:30:30+00:00"));
    // Distinct jobs never collide on the same tick time.
    assert_ne!(
        first,
        cron_occurrence_key("other-job", "2026-06-09T22:30:30+00:00")
    );
}

#[test]
fn cron_sync_classifies_permanent_errors_terminal_and_unknown_errors_retryable() {
    let deleted = classified_embed_handler_error(lash::EmbedError::Store(
        lash::persistence::StoreError::SessionDeleted {
            session_id: "retired-session".to_string(),
        },
    ));
    let deleted_rendered =
        <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&deleted)
            .to_string();
    assert!(
        deleted_rendered.starts_with("Terminal error"),
        "SessionDeleted must terminate cron reconciliation, got {deleted_rendered}"
    );

    let unknown = classified_embed_handler_error(lash::EmbedError::Store(
        lash::persistence::StoreError::Backend("temporary outage".to_string()),
    ));
    let unknown_rendered =
        <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&unknown)
            .to_string();
    assert!(
        !unknown_rendered.starts_with("Terminal error"),
        "ambiguous store failures must remain retryable, got {unknown_rendered}"
    );
}

#[test]
fn runtime_shape_uses_the_shared_terminal_classifier() {
    let error = AppError::runtime(lash::EmbedError::Runtime(
        lash_core::RuntimeError::new("runtime_store", "retired controller-owned session")
            .with_cause(lash_core::RuntimeErrorCause::SessionDeleted {
                session_id: "retired-session".to_string(),
            }),
    ));

    assert!(error.terminal);
    assert!(!error.retryable);
    assert_eq!(
        error.message,
        crate::deleted_session_message("retired-session")
    );
}

#[test]
fn nested_deleted_session_details_preserve_controller_store_context() {
    let source = lash::persistence::StoreError::SessionDeleted {
        session_id: "retired-nested-context".to_string(),
    };
    let error = lash::EmbedError::Plugin(lash::plugins::PluginError::RuntimeEffectController(
        lash_core::RuntimeEffectControllerError::from(source),
    ));

    assert_eq!(
        crate::deleted_session_details(&error),
        Some(("retired-nested-context", Some("runtime_store"),))
    );
}

#[tokio::test]
async fn queued_work_wake_preserves_a_retired_session_terminal() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.path().join("lash-sessions")),
    );
    let session_id = "retired-queued-work-wake";
    drop(
        store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                session_id: session_id.to_string(),
                relation: lash::persistence::SessionRelation::default(),
                policy: lash::runtime::SessionPolicy::default(),
            })
            .await
            .expect("create session before retirement"),
    );
    store_factory
        .delete_session(session_id)
        .await
        .expect("retire queued-work session");
    let queued_work_driver =
        lash::runtime::QueuedWorkDriver::new(Arc::new(crate::WorkbenchQueuedWorkSubmitter {
            session_ids: crate::WorkbenchSessionIds::fresh(),
            store_factory,
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_http: reqwest::Client::new(),
            active_turns: crate::ActiveTurns::default(),
        }));

    let error = queued_work_driver
        .claim_and_run_pending(Some(session_id), "retired_session_regression")
        .await
        .expect_err("the queued-work wake must refuse the retired session");
    let classified = AppError::runtime(lash::EmbedError::Plugin(error.clone()));

    assert_eq!(classified.status, axum::http::StatusCode::CONFLICT);
    assert_eq!(
        classified.message,
        crate::deleted_session_message(session_id)
    );
    assert!(classified.terminal);
    assert!(!classified.retryable);

    let rendered = <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(
        &super::classified_plugin_handler_error(error),
    )
    .to_string();
    assert!(
        rendered.starts_with("Terminal error"),
        "the retired-session refusal must be terminal: {rendered}"
    );
    assert!(
        rendered.contains(&crate::deleted_session_message(session_id)),
        "the terminal must retain the canonical message: {rendered}"
    );

    let ambiguous = super::classified_plugin_handler_error(lash::plugins::PluginError::Session(
        "temporary queued-work outage".to_string(),
    ));
    let ambiguous_rendered =
        <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&ambiguous)
            .to_string();
    assert!(
        !ambiguous_rendered.starts_with("Terminal error"),
        "ambiguous queued-work failures must remain retryable: {ambiguous_rendered}"
    );
}

#[tokio::test]
async fn cron_occurrence_call_site_terminalizes_typed_refusals_and_retries_unknown_failures() {
    let session_id = "retired-cron-occurrence";
    let canonical = crate::deleted_session_message(session_id);
    let cases = [
        (
            "typed permanent refusal",
            lash::plugins::PluginError::RuntimeEffectController(
                lash_core::RuntimeEffectControllerError::from(
                    lash::persistence::StoreError::SessionDeleted {
                        session_id: session_id.to_string(),
                    },
                ),
            ),
            true,
        ),
        (
            "ambiguous backend failure",
            lash::plugins::PluginError::Session("temporary trigger-store outage".to_string()),
            false,
        ),
    ];

    for (case, failure, expected_terminal) in cases {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let trigger_store = Arc::new(OccurrenceFailureTriggerStore::new(failure))
            as Arc<dyn lash::triggers::TriggerStore>;
        let state = crate::tests::recoverable_chat_test_state_with_trigger_store(
            data_dir.path(),
            trigger_store,
        )
        .await;
        let effect_host = state.core.effect_host();
        let scoped_effect_controller = effect_host
            .scoped(lash::runtime::ExecutionScope::runtime_operation(
                "cron-occurrence-classification-test",
            ))
            .expect("scope inline trigger emission");
        let error = match emit_cron_occurrence_with_effect_controller(
            state,
            WorkbenchCronRequest {
                session_id: session_id.to_string(),
                source_key: "test-source".to_string(),
                expr: "*/10 * * * * *".to_string(),
                tz: Some("UTC".to_string()),
                name: Some("classification test".to_string()),
            },
            "2026-07-30T12:00:00+00:00".to_string(),
            "classification-test-job",
            scoped_effect_controller,
        )
        .await
        {
            Ok(_) => panic!("configured trigger store must reject the occurrence"),
            Err(error) => error,
        };
        let rendered =
            <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&error)
                .to_string();

        assert_eq!(
            rendered.starts_with("Terminal error"),
            expected_terminal,
            "{case} classification mismatch: {rendered}"
        );
        if expected_terminal {
            assert!(
                rendered.contains(&canonical),
                "{case} must retain the canonical refusal message: {rendered}"
            );
        }
    }
}

#[tokio::test]
async fn cron_occurrence_redrive_reemits_the_reserved_process_start() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash_core::InMemoryTriggerStore::default());
    let state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let source_key = "cron-source:fig806";
    let outcome = lash::triggers::TriggerStore::execute_command(
        trigger_store.as_ref(),
        "fig806-cron-register",
        lash::triggers::TriggerCommand::Register {
            owner_scope: lash::triggers::TriggerOwnerScope::session("fig806-cron-session"),
            actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
                "fig806-cron-session",
            )),
            draft: lash::triggers::TriggerSubscriptionDraft::for_process(
                "fig806/cron",
                lash_core::ProcessExecutionEnvRef::new("process-env:fig806-cron"),
                crate::CRON_SCHEDULE_SOURCE_TYPE,
                source_key,
                lash_core::ProcessInput::Engine {
                    kind: "fig806-cron-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                lash_core::ProcessIdentity::new("fig806-cron-engine"),
            )
            .with_payload_schema(lash_core::LashSchema::any()),
        },
    )
    .await
    .expect("register cron trigger")
    .expect("cron trigger mutation");
    assert!(matches!(
        outcome,
        lash::triggers::TriggerCommandOutcome::Mutation { .. }
    ));
    let controller = CountingProcessEffectController::default();
    let request = WorkbenchCronRequest {
        session_id: "fig806-cron-session".to_string(),
        source_key: source_key.to_string(),
        expr: "*/10 * * * * *".to_string(),
        tz: Some("UTC".to_string()),
        name: Some("FIG-806 cron replay".to_string()),
    };
    let fired_at = "2026-07-30T12:00:00+00:00".to_string();

    for attempt in 0..2 {
        let scoped = lash_core::ScopedEffectController::borrowed(
            &controller,
            lash_core::ExecutionScope::runtime_operation("fig806-cron-redrive"),
        )
        .expect("scope cron trigger emission");
        emit_cron_occurrence_with_effect_controller(
            state.clone(),
            request.clone(),
            fired_at.clone(),
            "fig806-cron-job",
            scoped,
        )
        .await
        .unwrap_or_else(|error| panic!("cron attempt {attempt} failed: {error:?}"));
    }

    assert_eq!(
        controller.process_starts.load(Ordering::SeqCst),
        2,
        "the reserved replay must emit the same process-start effect"
    );
    assert_eq!(
        lash::triggers::TriggerStore::list_deliveries(trigger_store.as_ref())
            .await
            .expect("list cron deliveries")
            .len(),
        1,
        "the repeated cron occurrence still owns one deterministic delivery"
    );
}
