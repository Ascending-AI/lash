use super::*;
use lash_core::plugin::config::core::{SetLlmProfile, SetTurnBudget};
use lash_core::testing::{Script, StoreOp};

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
