//! A model's request defaults are recorded with the run's model binding
//! (FIG-4374), the response-metadata capture allowlists included (FIG-4397):
//! a run's model call carries the defaults the run recorded, never those of
//! the worker that runs it.
use super::*;
use pretty_assertions::assert_eq;

/// The request defaults the run records: the crashing execution's binding.
fn recorded_defaults() -> lash_core::provider::LlmProfileRequestDefaults {
    lash_core::provider::LlmProfileRequestDefaults {
        response_metadata_headers: vec!["x-recorded-cost".to_string()],
        response_metadata_body_paths: vec!["/recorded/cost".to_string()],
        ..lash_core::provider::LlmProfileRequestDefaults::default()
    }
}

/// The request defaults the redeployed worker states for the same model key:
/// no field agrees with [`recorded_defaults`].
fn redeployed_defaults() -> lash_core::provider::LlmProfileRequestDefaults {
    lash_core::provider::LlmProfileRequestDefaults {
        expose_thinking: true,
        cache_retention: lash_sansio::llm::capability::CacheRetention::Long,
        response_metadata_headers: vec!["x-redeployed-cost".to_string()],
        response_metadata_body_paths: vec!["/redeployed".to_string()],
    }
}

/// A model that answers every call and keeps each request it served.
fn capturing_model(
    requests: &Arc<std::sync::Mutex<Vec<crate::LlmRequest>>>,
) -> crate::ProviderHandle {
    let requests = Arc::clone(requests);
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let requests = Arc::clone(&requests);
            async move {
                lash_sansio::sync::MutexExt::lock_recover(&*requests).push(request);
                Ok(crate::LlmResponse {
                    parts: vec![crate::LlmOutputPart::Text {
                        text: "served".into(),
                        response_meta: None,
                    }],
                    ..crate::LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// A policy whose creation default binds [`FIRST_PROFILE`] with `defaults`.
fn policy_with_request_defaults(
    defaults: lash_core::provider::LlmProfileRequestDefaults,
    default_cap: std::num::NonZeroUsize,
) -> crate::SessionPolicy {
    crate::SessionPolicy {
        model: Some(crate::testing::test_llm_profile_config(FIRST_PROFILE, {
            let mut metadata = crate::testing::test_llm_profile_metadata(FIRST_PROFILE)
                .with_request_defaults(defaults);
            metadata.limits.output_tokens = crate::OutputTokenLimits::from_default_cap(default_cap);
            metadata
        })),
        ..crate::testing::mock_session_policy()
    }
}

/// A model call made on a redrive carries the request defaults its run
/// recorded (FIG-4567, ADR 0105 §1). The run's first execution records its
/// config, its model binding's request defaults included, and dies before its
/// model call. The redrive opens the session on a worker whose creation
/// default states other defaults for the same model key, capture allowlists
/// included, as a redeployed core would: the call reads the record.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_redrive_calls_the_model_with_the_request_defaults_its_run_recorded(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    // The session is created under the policy the crashing execution's
    // deployment would mint: the created head records its model binding,
    // request defaults included, and the redeployed worker's open adopts it
    // (FIG-4553).
    let parts = law_session_recording(
        prefix,
        "recorded-request-defaults-redrive",
        &effect_host,
        &stores,
        turn_config_llm_profiles(capturing_model(&requests)),
        policy_with_request_defaults(
            recorded_defaults(),
            std::num::NonZeroUsize::MIN.saturating_add(3332),
        ),
        Vec::new(),
    )
    .await;
    let run = TurnId::fixture(format!(
        "{prefix}-turn-config-recorded-request-defaults-run"
    ));
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    let crashing: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let run = run.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let run = run.clone();
            Box::pin(async move {
                let mut runtime = build_runtime_under(
                    parts,
                    policy_with_request_defaults(
                        recorded_defaults(),
                        std::num::NonZeroUsize::MIN.saturating_add(3332),
                    ),
                )
                .await;
                runtime.set_turn_phase_probe(Arc::new(CrashBeforeFirstModelCall));
                let _ = runtime
                    .execute_turn(
                        text_input(&run, "answer once"),
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                panic!("the crash fires before the run's model call");
            })
        })
    };
    let redriven: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let run = run.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let run = run.clone();
            let turn_tx = turn_tx.clone();
            Box::pin(async move {
                let mut runtime = build_runtime_under(
                    parts,
                    policy_with_request_defaults(
                        redeployed_defaults(),
                        std::num::NonZeroUsize::MIN.saturating_add(7776),
                    ),
                )
                .await;
                let turn = runtime
                    .execute_turn(
                        text_input(&run, "answer once"),
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                let end = crate::ConformanceTurnEnd::of(&turn);
                let _ = turn_tx.send(turn);
                end
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, &run)),
            crashing,
            redriven,
        )
        .await;
    let turn = turn_rx
        .recv()
        .await
        .expect("the tier's runner redrove the run")
        .unwrap_or_else(|error| panic!("the redriven run executes: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the redriven run answers: {:?}",
        turn.outcome
    );
    let requests = lash_sansio::sync::MutexExt::lock_recover(&*requests).clone();
    assert_eq!(requests.len(), 1, "the run made exactly one model call");
    assert_eq!(
        requests[0]
            .model
            .metadata()
            .limits
            .output_tokens
            .default_cap()
            .map(std::num::NonZeroUsize::get),
        Some(3333),
        "the redrive keeps the recorded cap"
    );
    assert_eq!(
        requests[0].model.metadata().request_defaults,
        recorded_defaults(),
        "the redriven call carries the request defaults its run recorded, capture allowlists included"
    );
}
