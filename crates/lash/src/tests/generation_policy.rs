//! A session's reasoning and generation options reach its provider requests
//! as the session recorded them, are judged against the recorded model, and
//! are reported back on the turn's call record. On facade turns the session
//! actor runs.

use super::*;
use std::num::NonZeroUsize;

fn ok_response() -> LlmResponse {
    text_response("ok")
}

/// A session over a core serving `metadata` through `provider`, created
/// from `spec` or, when `None`, from the spec that names `metadata`.
async fn session_serving(
    id: &str,
    provider: ProviderHandle,
    metadata: lash_core::LlmProfileMetadata,
    spec: Option<crate::SessionSpec>,
) -> Result<(LashCore, crate::LashSession)> {
    let spec = spec.unwrap_or_else(|| session_spec_for(&metadata));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, metadata)
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(id).expect("nonblank host identity"))
        .created_with(spec)
        .await
        .open()
        .await?;
    Ok((core, session))
}

fn with_efforts(efforts: &[&str]) -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .expect("valid model spec")
        .with_capability(lash_core::LlmProfileCapability {
            reasoning: Some(lash_core::ReasoningCapability {
                efforts: efforts.iter().map(|effort| effort.to_string()).collect(),
                ..Default::default()
            }),
            ..Default::default()
        })
}

fn generation(
    transaction: lash_core::facade_support::GenerationOverlay,
) -> crate::config::ConfigTransaction {
    crate::config::ConfigTransaction::of(crate::config::SetGeneration {
        generation: transaction,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_driver_sends_an_exact_effort_unchanged() -> Result<()> {
    let captured = Arc::new(StdMutex::new(None));
    let provider = crate::testing::TestProvider::builder()
        .kind("capability-capture")
        .complete({
            let captured = Arc::clone(&captured);
            move |request| {
                *captured.lock_recover() = Some(request.model.reasoning.clone());
                async { Ok(ok_response()) }
            }
        })
        .build()
        .into_handle();
    let (_core, session) = session_serving(
        "exact-effort",
        provider,
        with_efforts(&["low", "medium", "high", "max"]),
        None,
    )
    .await?;
    session
        .admin()
        .config()
        .configure(crate::config::ConfigTransaction::of(
            crate::config::SetReasoning {
                reasoning: lash_core::ReasoningSelection::Effort("max".to_string()),
            },
        ))
        .await?;

    let output = session.send(TurnInput::text("hello")).output().await?;

    assert_eq!(output.assistant_message(), Some("ok"));
    assert_eq!(
        captured
            .lock_recover()
            .clone()
            .expect("provider must be called"),
        lash_core::ReasoningSelection::Effort("max".to_string()),
        "an advertised effort travels to the provider exactly as selected"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_driver_rejects_unsupported_effort_before_provider_call() -> Result<()> {
    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = crate::testing::TestProvider::builder()
        .kind("capability-reject")
        .complete({
            let called = Arc::clone(&called);
            move |_| {
                called.store(true, Ordering::SeqCst);
                async { Ok(LlmResponse::default()) }
            }
        })
        .build()
        .into_handle();
    let metadata = with_efforts(&["low", "medium", "high"]);
    let mut spec = session_spec_for(&metadata);
    spec.reasoning = Some(lash_core::ReasoningSelection::Effort("turbo".to_string()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, metadata)
    .build(crate::testing::runtime_lease_owner())?;

    // A session records only a reasoning its model advertises: the
    // selection is judged when the session is created, so no turn ever
    // carries it to the provider.
    let Err(refused) = core
        .session(crate::SessionId::parse("unsupported-effort").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(spec))
        .await
    else {
        panic!("an unsupported effort is refused");
    };
    let EmbedError::ReasoningRefused(refusal) = &refused else {
        panic!("the refusal is typed: {refused:?}");
    };
    assert_eq!(
        refusal.category,
        lash_core::facade_support::LlmProfileEffortValidationCategory::UnsupportedEffort
    );
    assert!(refusal.message.contains("Unsupported effort `turbo`"));
    assert!(
        !called.load(Ordering::SeqCst),
        "an unsupported effort must be rejected before the provider is called"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_generation_options_reach_every_provider_request() -> Result<()> {
    type Captured = (lash_core::GenerationOptions, Option<u64>);
    let captured: Arc<StdMutex<Vec<Captured>>> = Arc::new(StdMutex::new(Vec::new()));
    let provider = crate::testing::TestProvider::builder()
        .kind("generation-capture")
        .complete({
            let captured = Arc::clone(&captured);
            move |request| {
                captured.lock_recover().push((
                    request.generation.clone(),
                    request
                        .model
                        .metadata()
                        .limits
                        .output_tokens
                        .default_cap()
                        .map(|cap| cap.get() as u64),
                ));
                async { Ok(ok_response()) }
            }
        })
        .build()
        .into_handle();
    // A recorded model's output-cap default is model configuration, not
    // request intent: it rides the recorded limits, never the generation
    // options, and resolution layers it under them.
    let mut metadata = lash_core::testing::test_llm_profile_metadata("mock-model");
    metadata.limits.output_tokens =
        lash_core::OutputTokenLimits::new(None, Some(1024)).expect("valid recorded cap");
    let (_core, session) = session_serving("generation-capture", provider, metadata, None).await?;

    session.send(TurnInput::text("hello")).output().await?;
    let requested = lash_core::GenerationOptions {
        output_token_cap: NonZeroUsize::new(64),
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
        seed: Some(1234),
        stop_sequences: Vec::new(),
        ..Default::default()
    };
    session
        .admin()
        .config()
        .configure(generation(
            lash_core::facade_support::GenerationOverlay::Replace(requested.clone()),
        ))
        .await?;
    session.send(TurnInput::text("hello")).output().await?;

    let seen = captured.lock_recover().clone();
    assert_eq!(seen.len(), 2, "each turn issues one provider call");
    assert_eq!(
        seen[0],
        (lash_core::GenerationOptions::default(), Some(1_024)),
        "a session that requested nothing must not have its model's defaults echoed back as request intent"
    );
    assert_eq!(
        seen[1],
        (requested.clone(), Some(1_024)),
        "the session's generation options must reach the provider request verbatim"
    );
    assert_eq!(
        committed(&session).await.policy().generation,
        requested,
        "the requested options are durable session policy, not per-turn state"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mid_run_generation_patch_merges_like_the_spec_overlay_does() -> Result<()> {
    // Both surfaces that set generation options speak one vocabulary. A
    // patch naming only a cap must not drop a temperature and seed the
    // session pinned, the loss `SessionSpec`'s overlay exists to prevent,
    // and replacing stays available for a host that means it.
    let (_core, session) = session_serving(
        "generation-patch",
        mock_provider(),
        mock_llm_profile_spec(),
        None,
    )
    .await?;
    let config = session.admin().config();
    let pinned = lash_core::GenerationOptions {
        output_token_cap: None,
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
        seed: Some(42),
        stop_sequences: Vec::new(),
        ..Default::default()
    };
    config
        .configure(generation(
            lash_core::facade_support::GenerationOverlay::Replace(pinned.clone()),
        ))
        .await?;
    config
        .configure(generation(
            lash_core::facade_support::GenerationOverlay::Merge(lash_core::GenerationOptions {
                output_token_cap: NonZeroUsize::new(4_096),
                ..Default::default()
            }),
        ))
        .await?;
    assert_eq!(
        committed(&session).await.policy().generation,
        lash_core::GenerationOptions {
            output_token_cap: NonZeroUsize::new(4_096),
            temperature: pinned.temperature.clone(),
            seed: Some(42),
            stop_sequences: Vec::new(),
            ..Default::default()
        },
        "a patch that names only a cap keeps the sampling the session pinned"
    );

    config
        .configure(generation(
            lash_core::facade_support::GenerationOverlay::Replace(
                lash_core::GenerationOptions::default(),
            ),
        ))
        .await?;
    assert_eq!(
        committed(&session).await.policy().generation,
        lash_core::GenerationOptions::default(),
        "an explicit replace still clears every option"
    );
    Ok(())
}
