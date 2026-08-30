use super::{
    CronSessionDisposition, WorkbenchCronRequest, classified_embed_handler_error,
    cron_occurrence_key, cron_session_disposition, emit_cron_occurrence_with_effect_controller,
};
use crate::AppError;
use lash_core as lash_test_internals;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct CountingProcessEffectController {
    process_starts: AtomicUsize,
}

impl lash::runtime::AwaitEventResolver for CountingProcessEffectController {}

#[async_trait::async_trait]
impl lash::runtime::RuntimeEffectController for CountingProcessEffectController {
    async fn execute_effect(
        &self,
        envelope: lash::runtime::RuntimeEffectEnvelope,
        local_executor: lash::runtime::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash::runtime::RuntimeEffectOutcome, lash::runtime::RuntimeEffectControllerError>
    {
        if matches!(
            &envelope.command,
            lash::runtime::RuntimeEffectCommand::Process { command }
                if matches!(command.as_ref(), lash::runtime::ProcessCommand::Start { .. })
        ) {
            self.process_starts.fetch_add(1, Ordering::SeqCst);
        }
        local_executor.execute(envelope).await
    }
}

struct OccurrenceFailureTriggerStore {
    inner: lash::triggers::InMemoryTriggerStore,
    failure: lash::plugins::PluginError,
}

impl OccurrenceFailureTriggerStore {
    fn new(failure: lash::plugins::PluginError) -> Self {
        Self {
            inner: lash::triggers::InMemoryTriggerStore::new(),
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
    ) -> Result<lash::triggers::TriggerIngressReceipt, lash::plugins::PluginError> {
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
    ) -> Result<Vec<lash::triggers::TriggerDeliveryRetentionCandidate>, lash::plugins::PluginError>
    {
        self.inner.list_delivery_retention_candidates().await
    }

    async fn list_session_owner_ids_for_retention(
        &self,
    ) -> Result<Vec<String>, lash::plugins::PluginError> {
        self.inner.list_session_owner_ids_for_retention().await
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[lash::triggers::TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[String],
    ) -> Result<lash::triggers::TriggerRetentionReconciliationReport, lash::plugins::PluginError>
    {
        self.inner
            .reconcile_trigger_retention(candidates, deleted_session_ids)
            .await
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[lash::triggers::TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, lash::plugins::PluginError> {
        self.inner
            .delete_delivery_retention_candidates(candidates)
            .await
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> lash::triggers::TriggerOccurrenceReclamationResult {
        self.inner
            .reclaim_trigger_occurrences(cutoff_epoch_ms)
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
        lash::runtime::RuntimeError::new(
            lash::runtime::RuntimeErrorCode::RuntimeStore,
            "retired controller-owned session",
        )
        .with_cause(lash::runtime::RuntimeErrorCause::SessionDeleted {
            session_id: "retired-session".to_string(),
        }),
    ));

    assert_eq!(error.verdict, crate::AppErrorVerdict::Terminal);
    assert_eq!(
        error.message,
        crate::deleted_session_message("retired-session")
    );
}

#[test]
fn foreign_effect_controller_codes_remain_explicit_extensions() {
    let error = lash::runtime::RuntimeEffectControllerError::foreign(
        "workbench_extension_abort",
        "extension refused the effect",
    );

    assert_eq!(
        error.code,
        lash::runtime::RuntimeErrorCode::from_wire_code("workbench_extension_abort")
    );
    assert_eq!(error.code.as_str(), "workbench_extension_abort");
}

#[test]
fn nested_deleted_session_details_preserve_controller_store_context() {
    let source = lash::persistence::StoreError::SessionDeleted {
        session_id: "retired-nested-context".to_string(),
    };
    let error = lash::EmbedError::Plugin(lash::plugins::PluginError::RuntimeEffectController(
        lash::runtime::RuntimeEffectControllerError::from(source),
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
                pending_observer_intents: Vec::new(),
                session_id: session_id.to_string(),
                relation: lash::persistence::SessionRelation::default(),
                policy: lash::runtime::SessionPolicy::new(lash::TurnBudget::Unbounded),
            })
            .await
            .expect("create session before retirement"),
    );
    store_factory
        .delete_session(session_id)
        .await
        .expect("retire queued-work session");
    let queued_work_driver =
        lash::runtime::NativeQueuedWork::new(Arc::new(crate::WorkbenchQueuedWorkSubmitter {
            sessions: crate::WorkbenchSessions::fresh(),
            store_factory,
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_http: reqwest::Client::new(),
            active_turns: crate::ActiveTurns::default(),
        }));

    let error = queued_work_driver
        .drain_session(session_id, "retired_session_regression")
        .await
        .expect_err("the queued-work wake must refuse the retired session");
    let classified = AppError::runtime(lash::EmbedError::Plugin(error.clone()));

    assert_eq!(classified.status, axum::http::StatusCode::CONFLICT);
    assert_eq!(
        classified.message,
        crate::deleted_session_message(session_id)
    );
    assert_eq!(classified.verdict, crate::AppErrorVerdict::Terminal);

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
                lash::runtime::RuntimeEffectControllerError::from(
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
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
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
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                "fig806-cron-session",
            )),
            draft: lash::triggers::TriggerSubscriptionDraft::for_process(
                "fig806/cron",
                lash::process::ProcessExecutionEnvRef::new("process-env:fig806-cron"),
                crate::CRON_SCHEDULE_SOURCE_TYPE,
                source_key,
                lash::process::ProcessInput::Engine {
                    kind: "fig806-cron-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                lash::process::ProcessIdentity::new("fig806-cron-engine"),
            )
            .with_payload_schema(lash::triggers::LashSchema::any()),
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
        let scoped = lash::runtime::ScopedEffectController::borrowed(
            &controller,
            lash::runtime::ExecutionScope::runtime_operation("fig806-cron-redrive"),
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

/// Replay ownership chooses where effects execute; it does not stop the facade
/// from deriving and handing the turn scope to the configured effect host.
/// A deployment-only Restate host can still reject an effect that needs a live
/// handler, but that refusal now comes from the scoped host controller rather
/// than from a facade ownership preflight.
#[tokio::test]
async fn turn_control_binding_routes_foreground_turns_through_the_configured_host() {
    let data_dir = tempfile::tempdir().expect("turn control binding tempdir");

    let native_host: Arc<dyn lash::durability::EffectHost> =
        Arc::new(lash::durability::NativeEffectHost::default());
    let durable_host: Arc<dyn lash::durability::EffectHost> =
        lash_restate::RestateTurnDeployment::new(lash_restate::RestateConnection::new(
            "http://127.0.0.1:8080",
        ))
        .effect_host();
    let scope = lash::runtime::ExecutionScope::turn("routing-session", "routing-turn");
    let native_scoped = native_host.scoped(scope.clone()).expect("inline scope");
    assert!(matches!(
        native_host
            .turn_control_binding(&native_scoped)
            .await
            .expect("inline binding"),
        lash::runtime::TurnControlBinding::HostOwned {
            resolver: _,
            peek: _,
        }
    ));
    let durable_scoped = durable_host.scoped(scope).expect("durable scope");
    assert!(matches!(
        durable_host
            .turn_control_binding(&durable_scoped)
            .await
            .expect("durable binding"),
        lash::runtime::TurnControlBinding::RunScoped {
            resolver: _,
            durable_cancel_after_llm: true,
        }
    ));

    let provider_calls = Arc::new(AtomicUsize::new(0));
    let ownership_core = |effect_host: Arc<dyn lash::durability::EffectHost>, name: &str| {
        let provider_calls = Arc::clone(&provider_calls);
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-effect-replay-ownership")
            .complete(move |_| {
                let provider_calls = Arc::clone(&provider_calls);
                async move {
                    provider_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(crate::tests::text_response(
                        "<lashlang>\nfinish \"ownership answer\"\n</lashlang>",
                    ))
                }
            })
            .build()
            .into_handle();
        crate::tests::explicit_durable_test_facets(data_dir.path())
            .provider(provider)
            .model(
                lash::ModelSpec::builder("test-model")
                    .context_window_tokens(4096)
                    .build()
                    .expect("model spec"),
            )
            .store_factory(Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
                data_dir.path().join("lash-sessions"),
            )))
            .effect_host(effect_host)
            .build(crate::test_core_owner())
            .unwrap_or_else(|error| panic!("build {name} ownership core: {error:?}"))
    };

    let controller_owned = ownership_core(durable_host.clone(), "controller-owned");
    let session = controller_owned
        .session("workbench-controller-owned-replay")
        .open()
        .await
        .expect("open the controller-owned session");
    let failure = session
        .turn(lash::TurnInput::text("drive me from the foreground"))
        .turn_id("controller-owned-foreground-turn")
        .run()
        .await
        .expect_err("the deployment-only host cannot execute an LLM effect");
    assert!(
        matches!(
            failure,
            lash::EmbedError::Runtime(ref error)
                if error.code
                    == lash::runtime::RuntimeErrorCode::RestateEffectHostRequiresHandlerScope
        ),
        "the scoped Restate host must issue the handler-scope refusal: {failure:?}"
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        0,
        "the scoped host refuses before invoking the local provider executor"
    );
    session.close().await.expect("close the failed session");

    // A runtime-owned host uses the same facade entry point and executes the
    // local provider body instead.
    let runtime_owned = ownership_core(Arc::clone(&native_host), "runtime-owned");
    let session = runtime_owned
        .session("workbench-runtime-owned-replay")
        .open()
        .await
        .expect("reopen the session under runtime-owned replay");
    let mut stream = session
        .turn(lash::TurnInput::text("drive me from the foreground"))
        .stream()
        .expect("a runtime-owned host creates a scoped foreground stream");
    while let Some(activity) = stream.next_activity().await {
        activity.expect("the runtime-owned host streams turn activity");
    }
    let report = stream
        .finish()
        .await
        .expect("a runtime-owned host executes the same foreground turn");
    assert_eq!(
        report.final_value(),
        Some(&serde_json::json!("ownership answer"))
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        1,
        "runtime-owned replay runs the turn the controller-owned host refused"
    );
    session.close().await.expect("close the executed session");
}

async fn counted_settlement_attempts(
    state: &crate::AppState,
    session_id: &str,
    code: lash::runtime::RuntimeErrorCode,
) -> usize {
    for attempt in 1..=2 {
        let error = super::terminalize_turn_execution(
            state,
            session_id,
            "fig1058-settlement-turn",
            "fig1058.settlement",
            Ok(Err(AppError::runtime(lash::EmbedError::Runtime(
                lash::runtime::RuntimeError::new(code.clone(), "injected settlement failure"),
            )))),
        )
        .await
        .expect_err("the injected handler attempt must fail");
        let rendered =
            <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&error)
                .to_string();
        if rendered.starts_with("Terminal error") {
            return attempt;
        }
    }
    2
}

#[tokio::test]
async fn restate_turn_settlement_attempts_terminal_once_and_retryable_again() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::new(lash::triggers::InMemoryTriggerStore::default()),
    )
    .await;

    assert_eq!(
        counted_settlement_attempts(
            &state,
            "fig1058-terminal-settlement",
            lash::runtime::RuntimeErrorCode::PostgresEffectReplayLeaseLost,
        )
        .await,
        1,
        "unsafe post-effect lease loss must settle on its first handler attempt"
    );
    assert_eq!(
        counted_settlement_attempts(
            &state,
            "fig1058-retryable-settlement",
            lash::runtime::RuntimeErrorCode::RestateAwaitEventResolve,
        )
        .await,
        2,
        "a retryable Restate ingress failure must reach a second handler attempt"
    );
    assert_eq!(
        counted_settlement_attempts(
            &state,
            "fig1058-decode-settlement",
            lash::runtime::RuntimeErrorCode::RestateTurnTerminalDecode,
        )
        .await,
        1,
        "a deterministic Restate decode failure must settle on its first handler attempt"
    );
}

#[tokio::test]
async fn turn_body_reader_treats_ambiguous_errors_as_terminal() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::new(lash::triggers::InMemoryTriggerStore::default()),
    )
    .await;
    let session_id = state.current_session_id();
    state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open session for ambiguous turn-body test")
        .close()
        .await
        .expect("close session for ambiguous turn-body test");

    let error = super::terminalize_turn_execution(
        &state,
        &session_id,
        "fig1858-ambiguous-turn-body",
        "fig1858.ambiguous_turn_body",
        Ok(Err(AppError::internal("ambiguous turn failure"))),
    )
    .await
    .expect_err("an ambiguous turn-body error must be terminalized");
    let rendered =
        <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&error)
            .to_string();
    assert!(
        rendered.starts_with("Terminal error"),
        "ambiguous turn-body errors must terminate: {rendered}"
    );
}

#[test]
fn settlement_reader_treats_ambiguous_errors_as_retryable() {
    let error = super::settlement_handler_error(AppError::internal("ambiguous settlement failure"));
    let rendered =
        <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&error)
            .to_string();
    assert!(
        !rendered.starts_with("Terminal error"),
        "ambiguous settlement errors must remain retryable: {rendered}"
    );
}

include!("restate_cron_tests.rs");

#[async_trait::async_trait]
trait QueuedWorkExt {
    async fn drain_session(
        &self,
        session_id: &str,
        reason: &str,
    ) -> Result<(), lash::plugins::PluginError>;
}

#[async_trait::async_trait]
impl QueuedWorkExt for lash::runtime::NativeQueuedWork {
    async fn drain_session(
        &self,
        session_id: &str,
        reason: &str,
    ) -> Result<(), lash::plugins::PluginError> {
        lash::runtime::QueuedWorkSubstrate::drain_session_work(
            self,
            lash::runtime::SessionWorkTarget::Session(session_id.to_string()),
            reason,
        )
        .await
        .map(|_| ())
    }
}
