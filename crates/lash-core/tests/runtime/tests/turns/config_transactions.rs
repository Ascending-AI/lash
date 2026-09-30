use super::*;
use lash_core::plugin::PluginSessionRequest;
use lash_core::plugin::config::core::{
    SetGeneration, SetModel, SetPrompt, SetPromptTemplate, SetProvider, SetTurnBudget,
};

const SEED: u64 = 0x5_f420;

struct RefuseCommandEnqueue {
    inner: Arc<RecordingStore>,
}

#[async_trait::async_trait]
impl lash_core::store::RuntimeStoreDecorator for RefuseCommandEnqueue {
    type Inner = dyn lash_core::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        _batch: lash_core::runtime::QueuedWorkBatchDraft,
    ) -> Result<lash_core::runtime::QueuedWorkEnqueueOutcome, lash_core::StoreError> {
        Err(lash_core::StoreError::SessionStateVersionNewerThanRuntime {
            found: 13,
            current: 12,
        })
    }
}

#[tokio::test]
async fn command_enqueue_preserves_typed_session_state_version_refusal() {
    let backend = sqlite_memory_store_backend().await;
    let inner = recording_unbound_store_on(&backend).await;
    let store: Arc<dyn lash_core::RuntimeStore> = Arc::new(RefuseCommandEnqueue { inner });
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(&backend),
        store,
    )
    .await;
    let submitted = runtime
        .submit_config_transaction(
            "generation-refusal",
            0,
            &lash_core::ConfigTransaction::of(SetTurnBudget {
                turn_budget: lash_core::TurnBudget::bounded(3),
            }),
        )
        .await;
    let Err(lash_core::runtime::ConfigTransactionSubmitError::Runtime(error)) = submitted else {
        panic!("enqueue must be rejected with the store's typed refusal: {submitted:?}");
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::SessionStateVersionNewerThanRuntime
    );
    assert_eq!(
        error.session_state_version_refusal(),
        Some(lash_core::SessionStateVersionRefusal {
            found: 13,
            current: 12,
        })
    );
}

/// A transaction of several commands, of one owner or many, publishes with
/// one head commit and one config revision step (ADR 0101 §12).
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn a_transaction_publishes_every_command_with_one_commit_and_one_revision_step() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, mock_provider(Vec::new()))
            .await;
    assert_eq!(runtime.config_revision(), 0);
    enqueue_config_transaction(
        store.as_ref(),
        &runtime,
        "one-step",
        lash_core::ConfigTransaction::of(SetModel {
            model: lash_core::ModelSpec::builder("transaction-model")
                .context_window_tokens(32_000)
                .build()
                .expect("model"),
        })
        .then(SetTurnBudget {
            turn_budget: lash_core::TurnBudget::bounded(7),
        })
        .then(SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Merge(
                lash_core::GenerationOptions {
                    seed: Some(42),
                    ..Default::default()
                },
            ),
        }),
    )
    .await;
    let commits_before = *store.runtime_commit_count.lock_recover();
    let request = lash_core::engine::DriveRequest {
        session: SessionId::from("root"),
        request: lash_core::engine::DriveRequestId::new("config-transaction"),
        build_generation: runtime.host.core.backend().build_generation().clone(),
    };
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root"),
            "session-command",
        ))
        .await
        .expect("open the drain's handler");
    let drive = lash_core::drive::drive_session(&mut runtime, &handler.scoped(), &request)
        .await
        .expect("engine drive settles the config transaction");
    handler.close().await.expect("close the drain's handler");
    assert!(
        !drive.ran.is_empty(),
        "engine must admit the queued command root"
    );

    assert_eq!(
        *store.runtime_commit_count.lock_recover(),
        commits_before + 1,
        "a transaction publishes with exactly one head commit"
    );
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list settled config commands")
        .is_empty(),
        "the transaction's command settles"
    );
    assert_eq!(runtime.config_revision(), 1, "one revision step");
    let policy = runtime.session_policy();
    assert_eq!(policy.model.id, "transaction-model");
    assert_eq!(policy.turn_budget, lash_core::TurnBudget::bounded(7));
    assert_eq!(policy.generation.seed, Some(42));
}

/// A transaction id names one request: resubmitting the same content while
/// the command is pending returns its receipt and enqueues nothing more, and
/// resubmitting other content under the id is refused typed.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn a_resubmitted_transaction_reuses_its_receipt_and_refuses_changed_content() {
    let double = kernel_double(SEED + 30, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, mock_provider(Vec::new()))
            .await;
    let budget = |turns| {
        lash_core::ConfigTransaction::of(SetTurnBudget {
            turn_budget: lash_core::TurnBudget::bounded(turns),
        })
    };

    let first = runtime
        .submit_config_transaction("resubmitted", 0, &budget(7))
        .await
        .expect("first submission is admitted");
    let again = runtime
        .submit_config_transaction("resubmitted", 0, &budget(7))
        .await
        .expect("an identical resubmission is admitted");
    assert_eq!(again, first, "the resubmission names the same command");
    let changed = runtime
        .submit_config_transaction("resubmitted", 0, &budget(9))
        .await;
    assert!(
        matches!(
            &changed,
            Err(lash_core::runtime::ConfigTransactionSubmitError::Refused(
                lash_core::ConfigSubmitError::ChangedContent { id }
            )) if id == "resubmitted"
        ),
        "{changed:?}"
    );
    let moved = runtime
        .submit_config_transaction("resubmitted", 1, &budget(7))
        .await;
    assert!(
        matches!(
            &moved,
            Err(lash_core::runtime::ConfigTransactionSubmitError::Refused(
                lash_core::ConfigSubmitError::ChangedContent { .. }
            ))
        ),
        "the expected revision is part of the request: {moved:?}"
    );
    let queued = lash_core::store::QueuedWorkStore::list_queued_work(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list queued config commands");
    assert_eq!(
        queued
            .iter()
            .map(|batch| batch.batch_id.clone())
            .collect::<Vec<_>>(),
        vec![first.batch_id.clone()],
        "one command is queued for the id"
    );
    assert_eq!(
        runtime.config_revision(),
        0,
        "nothing applied at submission"
    );
}

/// A transaction is admitted only when every command names a registered
/// owner and command and its arguments decode: a refusal enqueues nothing
/// and changes nothing. An admitted one applies.
#[tokio::test]
pub(super) async fn config_submission_refuses_what_no_owner_registers() {
    let backend = sqlite_memory_store_backend().await;
    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    let original_model = runtime.session_policy().model.clone();
    let set_model = || {
        lash_core::ConfigTransaction::of(SetModel {
            model: lash_core::ModelSpec::builder("admitted-model")
                .context_window_tokens(32_000)
                .build()
                .expect("model"),
        })
    };

    let unnamed = runtime
        .apply_storeless_config_transaction("", 0, &set_model())
        .await;
    assert!(
        matches!(
            &unnamed,
            Err(lash_core::runtime::ConfigTransactionSubmitError::Runtime(error))
                if error.code == lash_core::RuntimeErrorCode::SessionCommandIdempotencyKey
        ),
        "{unnamed:?}"
    );
    let entry = |owner: &str, command: &str, args: serde_json::Value| {
        lash_core::ConfigTransaction::new().then_entry(lash_core::ConfigCommandEntry {
            owner: owner.to_string(),
            command: command.to_string(),
            args,
        })
    };
    for (transaction, expected) in [
        (
            entry("absent_plugin", "set_anything", serde_json::json!({})),
            lash_core::ConfigSubmitError::UnknownOwner {
                owner: "absent_plugin".to_string(),
            },
        ),
        (
            entry("core", "set_everything", serde_json::json!({})),
            lash_core::ConfigSubmitError::UnknownCommand {
                owner: "core".to_string(),
                command: "set_everything".to_string(),
            },
        ),
        (
            lash_core::ConfigTransaction::new(),
            lash_core::ConfigSubmitError::Empty,
        ),
    ] {
        let refused = runtime
            .apply_storeless_config_transaction("refused", 0, &transaction)
            .await;
        assert!(
            matches!(
                &refused,
                Err(lash_core::runtime::ConfigTransactionSubmitError::Refused(refusal))
                    if *refusal == expected
            ),
            "{refused:?}"
        );
    }
    let malformed = runtime
        .apply_storeless_config_transaction(
            "malformed",
            0,
            &entry("core", "set_turn_budget", serde_json::json!({ "turn": 3 })),
        )
        .await;
    assert!(
        matches!(
            &malformed,
            Err(lash_core::runtime::ConfigTransactionSubmitError::Refused(
                lash_core::ConfigSubmitError::InvalidArgs { owner, command, .. }
            )) if owner == "core" && command == "set_turn_budget"
        ),
        "{malformed:?}"
    );
    assert_eq!(runtime.session_policy().model, original_model);
    assert_eq!(runtime.config_revision(), 0);

    let applied = crate::runtime_support::apply_storeless_config(&mut runtime, set_model()).await;
    assert_eq!(
        applied,
        lash_core::ConfigTransactionOutcome::Applied {
            base_revision: 0,
            revision: 1,
            outputs: vec![serde_json::Value::Null],
        }
    );
    assert_eq!(runtime.session_policy().model.id, "admitted-model");
}

/// A turn budget a config transaction sets is the session's durable budget:
/// it survives a park and a cold reload.
#[tokio::test]
pub(super) async fn a_set_turn_budget_survives_park_and_reload() {
    let double = kernel_double(SEED + 20, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let runtime_store: Arc<dyn lash_core::RuntimeStore> = store.clone();
    let persisted_budget = lash_core::TurnBudget::bounded(7);
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(&backend),
        Arc::clone(&runtime_store),
    )
    .await;

    let outcome = crate::runtime_support::apply_config(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(SetTurnBudget {
            turn_budget: persisted_budget,
        }),
        "set-turn-budget",
    )
    .await;
    assert!(
        matches!(outcome, lash_core::ConfigTransactionOutcome::Applied { .. }),
        "{outcome:?}"
    );
    assert_eq!(runtime.session_policy().turn_budget, persisted_budget);
    drop(
        Box::pin(runtime.park())
            .await
            .expect("park mutated session"),
    );

    let reloaded_state = durable_state(runtime_store.clone(), "root").await;
    let plugin_host = lash_core::testing::test_plugin_host(Vec::new());
    let plugins = match reloaded_state.plugin_state() {
        Some(snapshot) => plugin_host.build_session(PluginSessionRequest::rematerialization(
            "root",
            snapshot,
            lash_core::plugin::SessionAuthorityContext {
                plugin_config: reloaded_state.admitted_plugin_config(),
                ..Default::default()
            },
        )),
        None => {
            plugin_host.build_session(PluginSessionRequest::creation("root", Default::default()))
        }
    }
    .expect("reloaded plugins");
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::facade_support::PersistentRuntimeServices::new(
        plugins,
        session_view(runtime_store, "root"),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let reloaded = lash_core::facade_support::LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        reloaded_state,
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("reload parked runtime");
    assert_eq!(
        reloaded.session_policy().turn_budget,
        persisted_budget,
        "the transaction's durable budget must survive cold reload"
    );
}

#[tokio::test]
pub(super) async fn every_applied_config_transaction_emits_a_lifecycle_event() {
    let backend = sqlite_memory_store_backend().await;
    let observed = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let observed_hook = Arc::clone(&observed);
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            let observed = Arc::clone(&observed_hook);
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                presentation_steps: vec![],
                runtime_event: Some(Arc::new(move |event| {
                    let observed = Arc::clone(&observed);
                    Box::pin(async move {
                        if let lash_core::plugin::PluginLifecycleEvent::SessionConfigChanged(ctx) =
                            event
                        {
                            observed.lock().await.push((ctx.previous, ctx.current));
                        }
                        Ok(())
                    })
                })),
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(Vec::new());
    let mut runtime = runtime_with_plugins(&backend, vec![plugin], transport).await;

    let alt_provider = TestProvider::builder()
        .kind("alt")
        .complete_error("alt provider not wired")
        .build()
        .into_handle();
    let alt_model = lash_core::ModelSpec::builder("alt-model")
        .context_window_tokens(123_456)
        .build()
        .expect("valid model spec");
    apply(
        &mut runtime,
        lash_core::ConfigTransaction::of(SetModel {
            model: alt_model.clone(),
        }),
    )
    .await;
    serve_runtime_providers(&mut runtime, [alt_provider.clone()]);
    apply(
        &mut runtime,
        lash_core::ConfigTransaction::of(SetProvider {
            provider_id: alt_provider.kind().to_string(),
        }),
    )
    .await;

    assert_eq!(observed.lock().await.len(), 2);

    let combined_provider = TestProvider::builder()
        .kind("combined")
        .complete_error("combined provider not wired")
        .build()
        .into_handle();
    let combined_model = lash_core::ModelSpec::builder("combined-model")
        .context_window_tokens(234_567)
        .build()
        .expect("valid combined model spec");
    serve_runtime_providers(&mut runtime, [combined_provider.clone()]);
    apply(
        &mut runtime,
        lash_core::ConfigTransaction::of(SetProvider {
            provider_id: combined_provider.kind().to_string(),
        })
        .then(SetModel {
            model: combined_model.clone(),
        }),
    )
    .await;

    assert_eq!(observed.lock().await.len(), 3);

    let prompt = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Patch", "prompt-only session config"),
    );
    apply(
        &mut runtime,
        lash_core::ConfigTransaction::of(SetPrompt {
            prompt: prompt.clone(),
        }),
    )
    .await;

    assert_eq!(observed.lock().await.len(), 4);

    let generation = lash_core::GenerationOptions {
        seed: Some(42),
        ..Default::default()
    };
    apply(
        &mut runtime,
        lash_core::ConfigTransaction::of(SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Replace(generation.clone()),
        }),
    )
    .await;

    assert_eq!(observed.lock().await.len(), 5);

    let helper_template =
        lash_core::PromptTemplate::new(vec![lash_core::PromptTemplateSection::untitled(vec![
            lash_core::PromptTemplateEntry::text("prompt helper template"),
        ])]);
    apply(
        &mut runtime,
        lash_core::ConfigTransaction::of(SetPromptTemplate {
            template: helper_template.clone(),
        }),
    )
    .await;

    let changes = observed.lock().await;
    assert_eq!(changes.len(), 6);
    let (previous, current) = &changes[0];
    assert_eq!(previous.provider_id, "mock");
    assert_eq!(current.provider_id, "mock");
    assert_eq!(current.model.id, "alt-model");
    assert_ne!(
        previous.context_window_tokens(),
        current.context_window_tokens()
    );
    let (previous, current) = &changes[1];
    assert_eq!(previous.provider_id, "mock");
    assert_eq!(previous.model.id, "alt-model");
    assert_eq!(current.provider_id, "alt");
    assert_eq!(current.model.id, "alt-model");
    let (previous, current) = &changes[2];
    assert_eq!(previous.provider_id, "alt");
    assert_eq!(previous.model.id, "alt-model");
    assert_eq!(current.provider_id, "combined");
    assert_eq!(current.model, combined_model);
    let (previous, current) = &changes[3];
    assert_eq!(previous.model.id, "combined-model");
    assert_eq!(current.prompt, prompt);
    let (previous, current) = &changes[4];
    assert_eq!(previous.prompt, prompt);
    assert_eq!(current.generation, generation);
    let (previous, current) = &changes[5];
    assert_eq!(previous.generation, generation);
    assert_eq!(
        current.prompt.template,
        Some(helper_template),
        "prompt template commands emit SessionConfigChanged"
    );
}

/// Apply `transaction` to a storeless runtime, which must apply it.
async fn apply(
    runtime: &mut lash_core::runtime::LashRuntime,
    transaction: lash_core::ConfigTransaction,
) {
    let outcome = crate::runtime_support::apply_storeless_config(runtime, transaction).await;
    assert!(
        matches!(outcome, lash_core::ConfigTransactionOutcome::Applied { .. }),
        "{outcome:?}"
    );
}
