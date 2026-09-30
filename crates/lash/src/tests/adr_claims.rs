//! Integration witnesses for the facade contracts from the second host API pass.

use super::*;
use crate::rlm::RlmSessionExt as _;

#[tokio::test]
async fn root_and_child_materialization_install_the_same_plugin_owned_engines() -> Result<()> {
    let double = restate_double(0x4143_0005).await;
    let backend = double.lash_backend();
    let build = || {
        explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
            .provider(mock_provider())
            .model(mock_model_spec())
            .build(crate::testing::runtime_lease_owner())
    };
    let core = build()?;
    core.session("materialize-root")
        .create(Default::default())
        .await?;
    core.session("materialize-child")
        .create(crate::SessionCreation {
            parent: Some("materialize-root".into()),
            ..Default::default()
        })
        .await?;
    core.session("materialize-stated")
        .create(crate::SessionCreation {
            parent: Some("materialize-root".into()),
            plugin_options: lash_core::PluginOptions::typed(
                lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                lash_rlm_types::RlmCreateExtras {
                    final_answer_format: Some(crate::rlm::RlmFinalAnswerFormat::Markdown),
                    ..Default::default()
                },
            )
            .map_err(EmbedError::ProtocolTurnOptions)?,
            ..Default::default()
        })
        .await?;
    for (id, format) in [
        (
            "materialize-root",
            crate::rlm::RlmFinalAnswerFormat::Markdown,
        ),
        (
            "materialize-child",
            crate::rlm::RlmFinalAnswerFormat::RawFinalValue,
        ),
        (
            "materialize-stated",
            crate::rlm::RlmFinalAnswerFormat::Markdown,
        ),
    ] {
        let session = core.session(id).open().await?;
        assert_eq!(
            session.rlm_config().unwrap().final_answer_format,
            Some(format)
        );
        assert_eq!(
            session.runtime.process_engines.require("lashlang")?.kind(),
            "lashlang"
        );
    }
    drop(core);
    let cold = build()?;
    assert_eq!(
        cold.host_process_engines.require("lashlang")?.kind(),
        "lashlang"
    );
    for (id, format) in [
        (
            "materialize-root",
            crate::rlm::RlmFinalAnswerFormat::Markdown,
        ),
        (
            "materialize-child",
            crate::rlm::RlmFinalAnswerFormat::RawFinalValue,
        ),
    ] {
        let session = cold.session(id).open().await?;
        assert_eq!(
            session.rlm_config().unwrap().final_answer_format,
            Some(format)
        );
        assert_eq!(
            session.runtime.process_engines.require("lashlang")?.kind(),
            "lashlang"
        );
    }
    let duplicate = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .provider(mock_provider())
        .model(mock_model_spec())
        .plugin(Arc::new(rlm_factory(&backend)))
        .build(crate::testing::runtime_lease_owner());
    assert!(
        matches!(duplicate, Err(EmbedError::Plugin(_))),
        "two plugin-owned engines of one kind must be refused"
    );
    let bad = cold
        .session("materialize-refused")
        .create(crate::SessionCreation {
            parent: Some("materialize-root".into()),
            plugin_options: lash_core::PluginOptions::typed(
                lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                serde_json::json!({"termination": {"kind": "unknown"}}),
            )
            .map_err(EmbedError::ProtocolTurnOptions)?,
            ..Default::default()
        })
        .await;
    assert!(bad.is_err(), "unknown creation options must be refused");
    assert!(matches!(
        cold.session("materialize-refused").open().await,
        Err(EmbedError::UnknownSession { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn multi_model_turn_and_remote_report_keep_per_call_evidence() -> Result<()> {
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("per-call-evidence")
        .generation_retry_guarantee(lash_core::provider::GenerationRetryGuarantee::Idempotent)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2).base_delay_ms(0).max_delay_ms(0),
            ..Default::default()
        })
        .complete({
            let attempts = Arc::clone(&attempts);
            move |_request| {
                let ordinal = attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if ordinal == 0 {
                        return Err(LlmTransportError::new("temporary admission failure")
                            .with_retry_verdict(lash_core::llm::transport::TransportRetryVerdict::RetryableTransient)
                            .with_partial_response(LlmResponse { execution_evidence: Some(lash_core::ExecutionEvidence {
                                served_model: Some("served-alpha".into()), provider_response_id: Some("response-0".into()), provider_request_id: Some("request-0".into()), ..Default::default()
                            }), ..Default::default() }));
                    }
                    let model = if ordinal == 1 { "served-alpha" } else { "served-beta" };
                    let evidence = lash_core::ExecutionEvidence {
                        served_model: Some(model.into()),
                        provider_response_id: Some(format!("response-{ordinal}")),
                        provider_request_id: Some(format!("request-{ordinal}")),
                        ..Default::default()
                    };
                    Ok(LlmResponse {
                        parts: if ordinal == 1 {
                            vec![LlmOutputPart::ToolCall {
                                call_id: "lookup-evidence".into(), tool_name: "app_lookup".into(),
                                input_json: "{}".into(), replay: None,
                            }]
                        } else {
                            vec![LlmOutputPart::Text { text: "partial beta".into(), response_meta: None }]
                        },
                        execution_evidence: Some(evidence),
                        terminal_reason: if ordinal == 1 { lash_core::LlmTerminalReason::Stop } else { lash_core::LlmTerminalReason::Cancelled },
                        ..Default::default()
                    })
                }
            }
        }).build().into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(provider)
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("per-call-evidence")
        .created()
        .await
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("use both served models"))
        .id("evidence-root")
        .output()
        .await?;
    let calls = &output.result.llm_calls;
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].attempts.len(), 2);
    assert_eq!(
        calls[0].attempts[0].outcome,
        lash_core::AttemptOutcome::Failed
    );
    assert!(
        calls[0].attempts[0]
            .retry_decision
            .as_ref()
            .unwrap()
            .scheduled
    );
    assert_eq!(
        calls[0].attempts[1].outcome,
        lash_core::AttemptOutcome::Completed
    );
    assert_eq!(
        calls[1].attempts[0].outcome,
        lash_core::AttemptOutcome::Aborted
    );
    for (attempt, ordinal, model) in [
        (&calls[0].attempts[0], 0, "served-alpha"),
        (&calls[0].attempts[1], 1, "served-alpha"),
        (&calls[1].attempts[0], 2, "served-beta"),
    ] {
        let evidence = attempt.evidence.as_ref().unwrap();
        assert_eq!(evidence.served_model.as_deref(), Some(model));
        assert_eq!(
            evidence.provider_response_id,
            Some(format!("response-{ordinal}"))
        );
        assert_eq!(
            evidence.provider_request_id,
            Some(format!("request-{ordinal}"))
        );
    }
    let remote = output.result.to_remote(
        &SessionId::from("per-call-evidence"),
        &lash_core::TurnId::from("evidence-root"),
        &output.activities,
    );
    let encoded = serde_json::to_value(&remote).unwrap();
    let expected = serde_json::to_value(calls).unwrap();
    let mut ledger = encoded["llm_calls"].clone();
    for (wire_call, native_call) in ledger
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip(expected.as_array().unwrap())
    {
        for (wire_attempt, native_attempt) in wire_call["attempts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .zip(native_call["attempts"].as_array().unwrap())
        {
            if let Some(evidence) = native_attempt.get("evidence") {
                for (key, value) in evidence.as_object().unwrap() {
                    if value.is_null() {
                        wire_attempt["evidence"]
                            .as_object_mut()
                            .unwrap()
                            .entry(key.clone())
                            .or_insert(serde_json::Value::Null);
                    }
                }
            }
            if let Some(retry) = wire_attempt.get_mut("retry_decision") {
                let delay_ms = retry
                    .as_object_mut()
                    .unwrap()
                    .remove("delay_ms")
                    .unwrap()
                    .as_u64()
                    .unwrap();
                retry["delay"] =
                    serde_json::to_value(std::time::Duration::from_millis(delay_ms)).unwrap();
            }
        }
    }
    assert_eq!(ledger, expected);
    let round_trip: lash_remote_protocol::RemoteTurnReport =
        serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(round_trip).unwrap(), encoded);
    for invented in [
        "producing_model",
        "primary_model",
        "served_model",
        "final_output_provenance",
    ] {
        assert!(
            encoded.get(invented).is_none(),
            "invented turn-level attribution: {invented}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn resumed_session_observe_wait_cancel_drive_keep_original_owners() -> Result<()> {
    let source_double = restate_double(0x0414_314a).await;
    let receiving_double = restate_double(0x0414_314b).await;
    let source_backend = source_double.lash_backend();
    let receiving_backend = receiving_double.lash_backend();
    let owner = crate::testing::runtime_lease_owner();
    let build = |backend, answer| {
        explicit_ephemeral_facets(LashCore::standard_builder(
            backend,
            crate::TurnBudget::Unbounded,
        ))
        .provider(text_provider("owner-matrix", "owner-model", answer))
        .model(model_spec("owner-model", None, 200_000))
        .build(owner.clone())
    };
    let source = build(source_backend.clone(), "source-answer")?;
    let receiving = build(receiving_backend.clone(), "receiving-answer")?;
    let id = "owner-operation-matrix";
    source.session(id).create(Default::default()).await?;
    receiving.session(id).create(Default::default()).await?;
    let session = source.session(id).open().await?;
    let receiving_session = receiving.session(id).open().await?;
    let receiving_output = receiving_session
        .send(TurnInput::text("receiving-row"))
        .id("receiving-root")
        .output()
        .await?;
    assert_eq!(
        receiving_output.assistant_message(),
        Some("receiving-answer")
    );
    let receiving_before = receiving_session
        .durable()
        .turn_input_applications()
        .await?;
    let scope = crate::TurnAddress::new(id, "owner-wait").execution_scope();
    let source_key = session
        .effect_host()
        .await_event_key(
            &scope,
            lash_core::AwaitEventWaitIdentity::Custom {
                key: "owner-wait".into(),
            },
        )
        .await?;
    let receiving_key = receiving_session
        .effect_host()
        .await_event_key(
            &scope,
            lash_core::AwaitEventWaitIdentity::Custom {
                key: "owner-wait".into(),
            },
        )
        .await?;
    assert_ne!(source_key, receiving_key);
    let mut waiters = Vec::new();
    for (host, key) in [
        (source_backend.effect_host(), source_key.clone()),
        (receiving_backend.effect_host(), receiving_key.clone()),
    ] {
        let waiter_host = Arc::clone(&host);
        let waiter_key = key.clone();
        waiters.push(lash_core::task::spawn(async move {
            waiter_host
                .await_await_event(&waiter_key, CancellationToken::new(), None)
                .await
        }));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if host
                    .list_outstanding_await_event_keys(&SessionId::from(id))
                    .await
                    .unwrap()
                    .contains(&key)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("wait registered before revocation");
    }
    let source_process = source_backend
        .process_registry()
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::json!({"owner":"source"}),
                },
                lash_core::ProcessProvenance::session(session.observe().process_scope()),
                lash_core::Lifetime::Detached,
            ),
            &[SessionId::from(id)],
        )
        .await?
        .id;
    let receiving_process = receiving_backend
        .process_registry()
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::json!({"owner":"receiving"}),
                },
                lash_core::ProcessProvenance::session(receiving_session.observe().process_scope()),
                lash_core::Lifetime::Detached,
            ),
            &[SessionId::from(id)],
        )
        .await?
        .id;
    let parked = Box::pin(session.park()).await?;
    let resumed = receiving.resume(parked).await?;
    let observed = resumed.admin().processes().list_all().await?;
    assert_eq!(
        observed
            .iter()
            .map(|row| &row.process_id)
            .collect::<Vec<_>>(),
        vec![&source_process]
    );
    let original_row = resumed
        .admin()
        .processes()
        .get(&source_process)
        .await?
        .unwrap();
    assert!(
        matches!(original_row.input, lash_core::ProcessInput::External { metadata } if metadata == serde_json::json!({"owner":"source"}))
    );
    let receiving_row = receiving_session
        .admin()
        .processes()
        .get(&receiving_process)
        .await?
        .unwrap();
    assert!(
        matches!(receiving_row.input, lash_core::ProcessInput::External { metadata } if metadata == serde_json::json!({"owner":"receiving"}))
    );
    assert!(
        resumed
            .durable()
            .turn_input_applications()
            .await?
            .is_empty()
    );
    resumed.revoke_durable_waits().await?;
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), waiters.remove(0))
            .await
            .unwrap()
            .unwrap()?,
        lash_core::Resolution::Cancelled
    ));
    assert!(matches!(
        source_backend
            .effect_host()
            .peek_await_event(&source_key)
            .await?,
        Some(lash_core::Resolution::Cancelled)
    ));
    assert_eq!(
        receiving_backend
            .effect_host()
            .peek_await_event(&receiving_key)
            .await?,
        None
    );
    let wait = resumed
        .effect_host()
        .await_await_event(&source_key, CancellationToken::new(), None)
        .await?;
    assert!(matches!(wait, lash_core::Resolution::Cancelled));
    receiving_backend
        .effect_host()
        .resolve_await_event(
            &receiving_key,
            lash_core::Resolution::Ok(serde_json::json!("receiving-wait")),
        )
        .await?;
    assert_eq!(
        waiters.remove(0).await.unwrap()?,
        lash_core::Resolution::Ok(serde_json::json!("receiving-wait"))
    );
    let sent = resumed
        .durable()
        .send(TurnInput::text("drive the original catalog"))
        .id("source-drive")
        .await?;
    let output = sent.output().await?;
    assert_eq!(output.assistant_message(), Some("receiving-answer"));
    assert_eq!(resumed.durable().turn_input_applications().await?.len(), 1);
    assert_eq!(
        receiving_session
            .durable()
            .turn_input_applications()
            .await?,
        receiving_before
    );
    let cancel_root = lash_core::TurnId::from("matrix-cancel");
    assert!(matches!(
        resumed
            .cancel(crate::CancelTarget::Root(cancel_root.clone()))
            .request_id("matrix-cancel-request")
            .await?,
        crate::CancelReceipt::Requested { .. }
    ));
    let source_store = lash_core::store::SessionStore::new(
        source_backend.session_store_factory(),
        SessionId::from(id),
    )?;
    let source_driver = lash_core::facade_support::TurnWorkDriver::for_session(
        source_backend.effect_host(),
        SessionId::from(id),
        Arc::clone(source_store.store()),
    );
    let receipt = source_driver
        .request_cancel(crate::TurnCancelRequest::new(
            crate::TurnAddress::new(id, lash_core::drive::physical_turn_of(&cancel_root, 0)),
            "matrix-source-probe",
            None,
        ))
        .await?;
    assert!(matches!(
        receipt.outcome,
        crate::TurnCancelOutcome::AlreadyRequested(_)
    ));
    assert_eq!(
        receiving_session
            .durable()
            .turn_input_applications()
            .await?,
        receiving_before
    );
    Ok(())
}
