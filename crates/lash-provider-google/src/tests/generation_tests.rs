//! Host generation settings on Cloud Code: the reasoning mapping per Google
//! dialect and the summary request (ADR 0121).
use super::*;

fn dialect_request(
    dialect: lash_core::GoogleDialect,
    variant: lash_core::provider::ReasoningSelection,
    capability: LlmProfileCapability,
) -> LlmRequest {
    let mut req = request(None);
    req.model.metadata_mut().capability = LlmProfileCapability {
        google_dialect: dialect,
        ..capability
    };
    req.model.reasoning = variant;
    req
}

#[test]
fn reasoning_maps_per_google_dialect() {
    let provider = GoogleOAuthProvider::for_test();
    let mut tight = dialect_request(
        lash_core::GoogleDialect::ClaudeOnVertex,
        lash_core::provider::ReasoningSelection::Effort("high".into()),
        budget_capability(&[("low", 1_024), ("high", 8_192)]),
    );
    tight.generation.output_token_cap = NonZeroUsize::new(8_192);
    let error = GoogleOAuthProvider::build_request(&provider, &tight, Vec::new(), None)
        .expect_err("thinking budget must be strictly below the output cap");
    assert_eq!(
        refusal_code(&error).as_deref(),
        Some("lash:reasoning_budget_exceeds_output_cap")
    );
}

#[test]
fn expose_thinking_requests_thoughts_without_a_reasoning_selection() {
    let provider = GoogleOAuthProvider::for_test();
    let mut req = request(None);
    req.model.metadata_mut().request_defaults.expose_thinking = true;
    let (body, receipt) =
        GoogleOAuthProvider::build_request_with_receipt(&provider, &req, vec![], None)
            .expect("body");
    assert_eq!(
        body["request"]["generationConfig"]["thinkingConfig"],
        json!({ "includeThoughts": true })
    );
    assert_eq!(
        receipt.thinking_summary,
        lash_core::GenerationOptionOutcome::Applied
    );
    assert_eq!(
        receipt.reasoning,
        lash_core::GenerationOptionOutcome::NotRequested
    );

    let mut exposed_request = request_with_capability(
        Some("medium"),
        effort_capability(&["low", "medium", "high"]),
    );
    exposed_request
        .model
        .metadata_mut()
        .request_defaults
        .expose_thinking = true;
    let exposed = GoogleOAuthProvider::build_request(&provider, &exposed_request, Vec::new(), None)
        .expect("schema projection");
    assert_eq!(
        exposed["request"]["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
}
