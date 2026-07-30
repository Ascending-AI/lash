use super::{
    WorkbenchCronRequest, classified_embed_handler_error, cron_occurrence_key,
    emit_cron_occurrence_with_effect_controller,
};
use crate::AppError;
use std::sync::Arc;

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
