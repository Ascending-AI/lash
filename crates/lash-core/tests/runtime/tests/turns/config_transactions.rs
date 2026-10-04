use super::*;
use lash_core::plugin::config::core::{SetAutonomy, SetGeneration, SetLlmProfile, SetTurnBudget};
use lash_core::testing::TestTurnExecution as _;
use lash_core::testing::{Script, StoreOp};

const SEED: u64 = 0x5_f420;

#[tokio::test]
async fn command_enqueue_preserves_typed_session_state_version_refusal() {
    let backend = sqlite_memory_store_backend().await;
    let inner = recording_unbound_store_on(&backend).await;
    let script = Script::new();
    script
        .on(StoreOp::enqueue_queued_work_with_outcome)
        .before()
        .fail(
            || lash_core::StoreError::SessionStateVersionNewerThanRuntime {
                found: 13,
                current: 12,
            },
        );
    let store: Arc<dyn lash_core::RuntimeStore> = script.wrap("command", inner);
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
    let original_profile = runtime.session_policy().model.clone();
    let admitted_model = serve_llm_profile_beside(
        &mut runtime,
        "admitted-model",
        lash_core::LlmProfileMetadata::builder("admitted-model")
            .context_window_tokens(32_000)
            .build()
            .expect("model"),
    );
    let set_llm_profile = || {
        lash_core::ConfigTransaction::of(SetLlmProfile {
            model: admitted_model.clone(),
        })
    };

    let unnamed = runtime
        .apply_storeless_config_transaction("", 0, &set_llm_profile())
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
    assert_eq!(runtime.session_policy().model, original_profile);
    assert_eq!(runtime.config_revision(), 0);

    let applied =
        crate::runtime_support::apply_storeless_config(&mut runtime, set_llm_profile()).await;
    assert_eq!(
        applied,
        lash_core::ConfigTransactionOutcome::Applied {
            base_revision: 0,
            revision: 1,
            outputs: vec![serde_json::Value::Null],
        }
    );
    assert_eq!(
        runtime.session_policy().wire_model(),
        Some("admitted-model")
    );
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

    let mut reloaded = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "the restored budget applies".into(),
                    response_meta: None,
                }],
                ..LlmResponse::default()
            }),
        }]),
        test_host_config(&backend),
        runtime_store,
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::turn("root", "restored-budget"))
        .await
        .expect("open the restored Run");
    reloaded
        .execute_turn(
            TurnInput::text("run under the restored budget"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("the Run restores the durable budget");
    handler.close().await.expect("close the restored Run");
    assert_eq!(
        reloaded.session_policy().turn_budget,
        persisted_budget,
        "the transaction's durable budget must survive cold reload"
    );
}

#[tokio::test]
pub(super) async fn every_applied_config_transaction_emits_a_lifecycle_event() {
    let double = kernel_double(SEED + 21, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
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
    let store = double_unbound_recording_store(&double).await;
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        transport,
        test_host_config(&backend),
        store,
    )
    .await;

    let alt_provider = TestProvider::builder()
        .kind("alt")
        .complete_error("alt provider not wired")
        .build()
        .into_handle();
    let alt_model = lash_core::LlmProfileMetadata::builder("alt-model")
        .context_window_tokens(123_456)
        .build()
        .expect("valid model metadata");
    let combined_provider = TestProvider::builder()
        .kind("combined")
        .complete_error("combined provider not wired")
        .build()
        .into_handle();
    let combined_model = lash_core::LlmProfileMetadata::builder("combined-model")
        .context_window_tokens(234_567)
        .build()
        .expect("valid combined model metadata");
    // Two keys share one wire model on different transports: a model change
    // moves the transport only through the key the registry minted.
    serve_runtime_llm_profiles(
        &mut runtime,
        [
            (
                lash_core::LlmProfileKey::new("alt-model"),
                lash_core::RegisteredLlmProfile::new(
                    alt_model.clone(),
                    mock_provider(Vec::new()).into_handle(),
                ),
            ),
            (
                lash_core::LlmProfileKey::new("alt-model-on-alt"),
                lash_core::RegisteredLlmProfile::new(alt_model.clone(), alt_provider.clone()),
            ),
            (
                lash_core::LlmProfileKey::new("combined-model"),
                lash_core::RegisteredLlmProfile::new(
                    combined_model.clone(),
                    combined_provider.clone(),
                ),
            ),
        ],
    );
    apply(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(SetLlmProfile {
            model: lash_core::LlmProfileKey::new("alt-model"),
        }),
    )
    .await;
    apply(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(SetLlmProfile {
            model: lash_core::LlmProfileKey::new("alt-model-on-alt"),
        }),
    )
    .await;

    assert_eq!(observed.lock().await.len(), 2);

    apply(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(SetLlmProfile {
            model: lash_core::LlmProfileKey::new("combined-model"),
        }),
    )
    .await;

    assert_eq!(observed.lock().await.len(), 3);

    apply(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(SetAutonomy { autonomous: true }),
    )
    .await;

    assert_eq!(observed.lock().await.len(), 4);

    let generation = lash_core::GenerationOptions {
        seed: Some(42),
        ..Default::default()
    };
    apply(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Replace(generation.clone()),
        }),
    )
    .await;

    assert_eq!(observed.lock().await.len(), 5);

    apply(
        &mut runtime,
        &double,
        lash_core::ConfigTransaction::of(SetTurnBudget {
            turn_budget: lash_core::TurnBudget::bounded(9),
        }),
    )
    .await;

    let changes = observed.lock().await;
    assert_eq!(changes.len(), 6);
    let key = |policy: &lash_core::SessionPolicy| {
        policy
            .profile_key()
            .map(ToString::to_string)
            .unwrap_or_default()
    };
    let (previous, current) = &changes[0];
    assert_eq!(key(previous), "mock-model");
    assert_eq!(key(current), "alt-model");
    assert_ne!(
        previous.context_window_tokens(),
        current.context_window_tokens()
    );
    let (previous, current) = &changes[1];
    assert_eq!(key(previous), "alt-model");
    assert_eq!(key(current), "alt-model-on-alt");
    assert_eq!(previous.wire_model(), current.wire_model());
    let (previous, current) = &changes[2];
    assert_eq!(key(previous), "alt-model-on-alt");
    assert_eq!(
        current.model,
        Some(lash_core::testing::test_llm_profile_config(
            "combined-model",
            combined_model
        ))
    );
    let (previous, current) = &changes[3];
    assert_eq!(key(previous), "combined-model");
    assert!(!previous.autonomous);
    assert!(current.autonomous);
    let (previous, current) = &changes[4];
    assert!(previous.autonomous);
    assert_eq!(current.generation, generation);
    let (previous, current) = &changes[5];
    assert_eq!(previous.generation, generation);
    assert_eq!(
        current.turn_budget,
        lash_core::TurnBudget::bounded(9),
        "every core command emits SessionConfigChanged"
    );
}

/// Settle the transaction through its command Run.
async fn apply(
    runtime: &mut lash_core::runtime::LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    transaction: lash_core::ConfigTransaction,
) {
    let command = format!("lifecycle-config-{}", runtime.config_revision());
    let outcome =
        crate::runtime_support::apply_config(runtime, double, transaction, &command).await;
    assert!(
        matches!(outcome, lash_core::ConfigTransactionOutcome::Applied { .. }),
        "{outcome:?}"
    );
}
