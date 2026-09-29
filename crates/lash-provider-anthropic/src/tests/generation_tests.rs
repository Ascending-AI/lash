//! Host generation settings on Anthropic Messages: each is sent or refused
//! before any I/O (ADR 0121).
use super::*;

#[test]
fn requested_temperature_is_sent_and_refused_where_thinking_pins_sampling() {
    let provider = AnthropicProvider::new("key");
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.generation.temperature = Some(NonNegativeFiniteF64::new(0.25).expect("finite temperature"));

    let (plain, receipt) = provider.build_request(&req).expect("plain body");
    assert_eq!(plain["temperature"], json!(0.25));
    assert_eq!(
        receipt.temperature,
        lash_core::llm::types::GenerationOptionOutcome::Applied
    );

    let mut thinking_req = req.clone();
    thinking_req.model_variant =
        lash_core::provider::ReasoningSelection::Effort("medium".to_string());
    thinking_req.model_capability = effort_capability(&["low", "medium", "high"]);
    // Extended thinking pins sampling; the temperature is refused rather
    // than dropped.
    let error = provider
        .build_request(&thinking_req)
        .expect_err("active thinking pins sampling");
    assert_eq!(
        refusal_code(&error).as_deref(),
        Some("lash:unsupported_generation_option")
    );

    // Thinking turned off does not pin sampling.
    thinking_req.model_variant = lash_core::provider::ReasoningSelection::Disabled;
    let (off, _) = provider.build_request(&thinking_req).expect("off body");
    assert_eq!(off["temperature"], json!(0.25));
}

#[test]
fn a_seed_is_refused_because_messages_has_no_seed_field() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.generation.seed = Some(7);
    let error = AnthropicProvider::new("key")
        .build_request(&req)
        .expect_err("no seed field");
    assert_eq!(
        refusal_code(&error).as_deref(),
        Some("lash:unsupported_generation_option")
    );
}

#[test]
fn requested_temperature_is_refused_for_a_model_that_pins_sampling() {
    // Models released after Claude Opus 4.6 answer any caller-set
    // temperature with HTTP 400, thinking or no thinking. The host says so
    // through the capability; the adapter never reads the model name.
    let provider = AnthropicProvider::new("key");
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model = "claude-opus-4-7".to_string();
    req.generation.temperature = Some(NonNegativeFiniteF64::new(0.25).expect("finite temperature"));
    req.model_capability.sampling = lash_core::SamplingCapability::Pinned;

    let error = provider
        .build_request(&req)
        .expect_err("a pinned model refuses a temperature");
    assert_eq!(
        refusal_code(&error).as_deref(),
        Some("lash:unsupported_generation_option")
    );

    // The same request against a model that allows it still emits.
    req.model_capability.sampling = lash_core::SamplingCapability::Configurable;
    let (configurable, receipt) = provider.build_request(&req).expect("body");
    assert_eq!(configurable["temperature"], json!(0.25));
    assert_eq!(
        receipt.temperature,
        lash_core::llm::types::GenerationOptionOutcome::Applied
    );
}

#[test]
fn an_uncapped_call_is_refused_with_output_token_cap_required() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.generation.output_token_cap = None;
    let error = AnthropicProvider::new("key")
        .build_request(&req)
        .expect_err("Anthropic requires a cap and lash invents none");
    assert_eq!(
        refusal_code(&error).as_deref(),
        Some("lash:output_token_cap_required")
    );
    assert!(!error.is_retryable());

    let (body, receipt) = AnthropicProvider::new("key")
        .with_options(ProviderOptions {
            max_output_tokens: Some(8_000),
            ..ProviderOptions::default()
        })
        .build_request(&req)
        .expect("the provider cap is the fallback");
    assert_eq!(body["max_tokens"], 8_000);
    assert_eq!(
        receipt.output_token_cap,
        lash_core::llm::types::GenerationOptionOutcome::Applied
    );
}

#[test]
fn a_budget_that_does_not_fit_under_the_cap_is_refused() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "think")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    req.model_capability = budget_capability();
    req.generation.output_token_cap = std::num::NonZeroUsize::new(12_288);
    let error = AnthropicProvider::new("key")
        .build_request(&req)
        .expect_err("budget 12288 needs max_tokens above it");
    assert_eq!(
        refusal_code(&error).as_deref(),
        Some("lash:reasoning_budget_exceeds_output_cap")
    );
    req.generation.output_token_cap = std::num::NonZeroUsize::new(12_289);
    let (body, _) = AnthropicProvider::new("key")
        .build_request(&req)
        .expect("fits");
    assert_eq!(body["thinking"]["budget_tokens"], json!(12_288));
}

#[test]
fn parallel_tool_calls_rides_tool_choice_and_needs_tools() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.generation.parallel_tool_calls = Some(false);
    let error = AnthropicProvider::new("key")
        .build_request(&req)
        .expect_err("no tool_choice without tools");
    assert_eq!(
        refusal_code(&error).as_deref(),
        Some("lash:unsupported_generation_option")
    );
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "lookup".to_string(),
        description: "Lookup".to_string(),
        input_schema: json!({ "type": "object" }).into(),
        output_schema: json!({}).into(),
    }]);
    let (body, receipt) = AnthropicProvider::new("key")
        .build_request(&req)
        .expect("body");
    assert_eq!(
        body["tool_choice"]["disable_parallel_tool_use"],
        json!(true)
    );
    assert_eq!(
        receipt.parallel_tool_calls,
        lash_core::llm::types::GenerationOptionOutcome::Applied
    );
}

#[test]
fn expose_thinking_without_active_thinking_is_local_visibility_only() {
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    let (body, receipt) = AnthropicProvider::new("key")
        .with_options(ProviderOptions {
            expose_thinking: true,
            ..ProviderOptions::default()
        })
        .build_request(&req)
        .expect("expose_thinking is never refused");
    assert!(body.get("thinking").is_none());
    assert_eq!(
        receipt.thinking_summary,
        lash_core::llm::types::GenerationOptionOutcome::NotRequested
    );
    assert_eq!(
        receipt.thinking_visibility,
        lash_core::llm::types::GenerationOptionOutcome::Applied
    );
}

/// Every refusal lands before the transport sees a byte.
#[tokio::test]
async fn refused_settings_never_reach_the_transport() {
    #[derive(Debug, Default)]
    struct CountingTransport(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl lash_llm_transport::LlmHttpTransport for CountingTransport {
        async fn send(
            &self,
            _request: lash_llm_transport::LlmHttpRequest,
            _timeout: Option<std::time::Duration>,
        ) -> Result<lash_llm_transport::LlmHttpResponse, lash_core::llm::transport::LlmTransportError>
        {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(lash_core::llm::transport::LlmTransportError::new(
                "a refused call reached the transport",
            ))
        }
    }
    let mut uncapped = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    uncapped.generation.output_token_cap = None;
    let mut seeded = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    seeded.generation.seed = Some(1);
    let mut pinned = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    pinned.generation.temperature = Some(NonNegativeFiniteF64::new(0.5).expect("finite"));
    pinned.model_capability.sampling = lash_core::SamplingCapability::Pinned;
    let mut inexact = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    inexact.model_capability = effort_capability(&["low", "high"]);
    inexact.model_variant = lash_core::provider::ReasoningSelection::Effort("High".to_string());
    let mut off_unsupported = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    off_unsupported.model_capability = budget_capability();
    off_unsupported.model_variant = lash_core::provider::ReasoningSelection::Disabled;
    for (label, req, code) in [
        ("uncapped", uncapped, "lash:output_token_cap_required"),
        ("seed", seeded, "lash:unsupported_generation_option"),
        ("pinned", pinned, "lash:unsupported_generation_option"),
        ("inexact effort", inexact, "lash:unsupported_effort"),
        (
            "off without disable",
            off_unsupported,
            "lash:unsupported_effort",
        ),
    ] {
        let transport = Arc::new(CountingTransport::default());
        let mut provider = AnthropicProvider::new("key").with_transport(transport.clone());
        let error = provider.complete(req).await.expect_err(label);
        assert_eq!(refusal_code(&error).as_deref(), Some(code), "{label}");
        assert_eq!(
            transport.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{label} reached the transport"
        );
    }
}
