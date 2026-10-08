//! Integration witnesses for the facade contracts from the second host API
//! pass, on the core's node over SQLite memory stores (FIG-5307).

use super::*;
use crate::support::TurnInput;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmResponse};
use lash_sansio::SessionId;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The RLM factory's plugin-owned `lashlang` engine is installed for a
/// root and for a child session alike, and again by a cold core; each
/// session's RLM options are exactly its create request's (FIG-5296: a
/// child inherits no default of its parent); a second engine of one
/// kind and unknown creation options are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_and_child_materialization_install_the_same_plugin_owned_engines() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let build = || {
        explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .build(crate::testing::runtime_lease_owner())
    };
    let core = build()?;
    core.session(crate::SessionId::parse("materialize-run").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await?;
    core.session(crate::SessionId::parse("materialize-child").expect("nonblank host identity"))
        .create(crate::SessionCreation::child_of(
            crate::plugins::SessionToolAccess::ambient(),
            "materialize-run".into(),
            mock_session_spec(),
        ))
        .await?;
    core.session(crate::SessionId::parse("materialize-stated").expect("nonblank host identity"))
        .create(crate::SessionCreation {
            tool_access: crate::plugins::SessionToolAccess::ambient(),
            parent: Some("materialize-run".into()),
            prompt_plan: None,
            spec: mock_session_spec().plugin_options(
                lash_core::PluginOptions::typed(
                    lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                    lash_rlm_types::RlmCreateExtras::default(),
                )
                .map_err(EmbedError::ProtocolTurnOptions)?,
            ),
        })
        .await?;
    for id in ["materialize-run", "materialize-child", "materialize-stated"] {
        let session = core
            .session(crate::SessionId::parse(id).expect("nonblank host identity"))
            .open()
            .await?;
        assert_eq!(
            session.runtime.process_engines.require("lashlang")?.kind(),
            "lashlang"
        );
    }
    core.shutdown().await?;
    let cold = build()?;
    assert_eq!(
        cold.host_process_engines.require("lashlang")?.kind(),
        "lashlang"
    );
    for id in ["materialize-run", "materialize-child"] {
        let session = cold
            .session(crate::SessionId::parse(id).expect("nonblank host identity"))
            .open()
            .await?;
        assert_eq!(
            session.runtime.process_engines.require("lashlang")?.kind(),
            "lashlang"
        );
    }
    let duplicate = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(rlm_factory(&backend)))
        .build(crate::testing::runtime_lease_owner());
    assert!(
        matches!(duplicate, Err(EmbedError::Plugin(_))),
        "two plugin-owned engines of one kind must be refused"
    );
    let bad = cold
        .session(crate::SessionId::parse("materialize-refused").expect("nonblank host identity"))
        .create(crate::SessionCreation {
            tool_access: crate::plugins::SessionToolAccess::ambient(),
            parent: Some("materialize-run".into()),
            prompt_plan: None,
            spec: mock_session_spec().plugin_options(
                lash_core::PluginOptions::typed(
                    lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                    serde_json::json!({"termination": {"kind": "unknown"}}),
                )
                .map_err(EmbedError::ProtocolTurnOptions)?,
            ),
        })
        .await;
    assert!(bad.is_err(), "unknown creation options must be refused");
    assert!(matches!(
        cold.session(
            crate::SessionId::parse("materialize-refused").expect("nonblank host identity")
        )
        .open()
        .await,
        Err(EmbedError::UnknownSession { .. })
    ));
    Ok(())
}

/// One turn whose first model call retries and whose second ends cancelled
/// reports each call's attempts with the evidence its provider served, and
/// the local report carries the same ledger with no invented turn-level
/// attribution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_model_turn_keeps_per_call_evidence() -> Result<()> {
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("per-call-evidence")
        .generation_retry_guarantee(lash_core::provider::GenerationRetryGuarantee::Idempotent)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(Some(2)).base_delay_ms(0).max_delay_ms(0),
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
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("per-call-evidence").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("use both served models"))
        .id(crate::TurnId::parse("evidence-run").expect("nonblank host identity"))
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
            .is_scheduled()
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
    Ok(())
}

/// A session parked on one core and resumed on a core over another
/// backend keeps its original backend's owners: its processes, its turn
/// inputs and the node that runs its turns are its own backend's, and the
/// receiving core's own session is untouched. (The effect host's
/// await-event keys and the cancel-request record this law also covered
/// were deleted with the engine double; waits are actor wait rows, ADR 0132
/// §6.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_session_observe_wait_cancel_shift_keep_original_owners() -> Result<()> {
    let source_backend = sqlite_memory_store_backend().await;
    let receiving_backend = sqlite_memory_store_backend().await;
    let build = |backend, answer: &'static str| {
        explicit_ephemeral_facets(LashCore::standard_builder(backend))
            .serve_test_llm_profile(
                text_provider("owner-matrix", answer),
                mock_llm_profile_spec(),
            )
            .build(crate::testing::runtime_lease_owner())
    };
    let source = build(source_backend.clone(), "source-answer")?;
    let receiving = build(receiving_backend.clone(), "receiving-answer")?;
    let id = "owner-operation-matrix";
    for core in [&source, &receiving] {
        core.session(crate::SessionId::parse(id).expect("nonblank host identity"))
            .create(crate::SessionCreation::root(
                crate::plugins::SessionToolAccess::ambient(),
                mock_session_spec(),
            ))
            .await?;
    }
    let session = source
        .session(crate::SessionId::parse(id).expect("nonblank host identity"))
        .open()
        .await?;
    let receiving_session = receiving
        .session(crate::SessionId::parse(id).expect("nonblank host identity"))
        .open()
        .await?;
    let receiving_output = receiving_session
        .send(TurnInput::text("receiving-row"))
        .id(crate::TurnId::parse("receiving-run").expect("nonblank host identity"))
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

    let source_process = source_backend
        .process_registry()
        .register_process_with_observers(
            lash_core::testing::held_engine_registration(
                serde_json::json!({"owner": "source"}),
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
            lash_core::testing::held_engine_registration(
                serde_json::json!({"owner": "receiving"}),
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
        vec![&source_process],
        "the resumed session lists its own backend's processes"
    );
    let original_row = resumed
        .admin()
        .processes()
        .get(&source_process)
        .await?
        .expect("the source process");
    assert!(matches!(
        original_row.input,
        lash_core::ProcessInput::Engine { payload, .. } if payload == serde_json::json!({"owner": "source"})
    ));
    let receiving_row = receiving_session
        .admin()
        .processes()
        .get(&receiving_process)
        .await?
        .expect("the receiving process");
    assert!(matches!(
        receiving_row.input,
        lash_core::ProcessInput::Engine { payload, .. } if payload == serde_json::json!({"owner": "receiving"})
    ));
    assert!(
        resumed
            .durable()
            .turn_input_applications()
            .await?
            .is_empty()
    );

    let output = resumed
        .durable()
        .send(TurnInput::text("run on the original backend"))
        .id(crate::TurnId::parse("source-run").expect("nonblank host identity"))
        .output()
        .await?;
    assert_eq!(
        output.assistant_message(),
        Some("source-answer"),
        "the turn runs on the node that serves the session's own backend"
    );
    assert_eq!(resumed.durable().turn_input_applications().await?.len(), 1);
    assert_eq!(
        receiving_session
            .durable()
            .turn_input_applications()
            .await?,
        receiving_before,
        "the receiving core's own session is untouched"
    );
    source.shutdown().await?;
    receiving.shutdown().await?;
    Ok(())
}
