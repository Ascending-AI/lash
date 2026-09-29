//! Host generation settings on Cloud Code: the reasoning mapping per Google
//! dialect and the summary request (ADR 0121).
use super::*;

fn dialect_request(
    dialect: lash_core::GoogleDialect,
    variant: lash_core::provider::ReasoningSelection,
    capability: ModelCapability,
) -> LlmRequest {
    let mut req = request(None);
    req.model_capability = ModelCapability {
        google_dialect: dialect,
        ..capability
    };
    req.model_variant = variant;
    req
}

#[test]
fn reasoning_maps_per_google_dialect() {
    use lash_core::GoogleDialect::{ClaudeOnVertex, Gemini3, Legacy};
    use lash_core::provider::ReasoningSelection::{Disabled, Effort};
    let mut effort = effort_capability(&["low", "high"]);
    if let Some(reasoning) = effort.reasoning.as_mut() {
        reasoning.disable = true;
    }
    let budget = budget_capability(&[("low", 1_024), ("high", 8_192)]);
    let provider = GoogleOAuthProvider::for_test();
    let thinking = |req: &LlmRequest| {
        GoogleOAuthProvider::build_request(&provider, req, Vec::new(), None)
            .map(|body| body["request"]["generationConfig"]["thinkingConfig"].clone())
    };
    let refused = |req: &LlmRequest| refusal_code(&thinking(req).expect_err("refused"));

    for dialect in [Legacy, Gemini3] {
        assert_eq!(
            thinking(&dialect_request(
                dialect,
                Effort("high".into()),
                effort.clone()
            ))
            .unwrap(),
            json!({ "thinkingLevel": "high" }),
            "{dialect:?}"
        );
        assert_eq!(
            thinking(&dialect_request(
                dialect,
                Effort("high".into()),
                budget.clone()
            ))
            .unwrap(),
            json!({ "thinkingBudget": 8_192 }),
            "{dialect:?}"
        );
    }
    assert_eq!(
        thinking(&dialect_request(Legacy, Disabled, effort.clone())).unwrap(),
        json!({ "thinkingBudget": 0 })
    );
    assert_eq!(
        refused(&dialect_request(Gemini3, Disabled, effort.clone())).as_deref(),
        Some("lash:reasoning_encoding_unrepresentable"),
        "Gemini 3 cannot turn thinking off"
    );

    // Claude on Vertex takes a budget below the cap, and nothing else.
    assert_eq!(
        thinking(&dialect_request(
            ClaudeOnVertex,
            Effort("low".into()),
            budget.clone()
        ))
        .unwrap(),
        json!({ "thinkingBudget": 1_024 })
    );
    assert_eq!(
        refused(&dialect_request(
            ClaudeOnVertex,
            Effort("high".into()),
            effort.clone()
        ))
        .as_deref(),
        Some("lash:reasoning_encoding_unrepresentable")
    );
    assert_eq!(
        refused(&dialect_request(ClaudeOnVertex, Disabled, effort.clone())).as_deref(),
        Some("lash:reasoning_encoding_unrepresentable")
    );
    let mut tight = dialect_request(ClaudeOnVertex, Effort("high".into()), budget.clone());
    tight.generation.output_token_cap = NonZeroUsize::new(8_192);
    assert_eq!(
        refused(&tight).as_deref(),
        Some("lash:reasoning_budget_exceeds_output_cap")
    );
    // Claude thinking pins sampling on Vertex as it does on Anthropic.
    let mut sampled = dialect_request(ClaudeOnVertex, Effort("low".into()), budget.clone());
    sampled.generation.temperature =
        Some(lash_core::NonNegativeFiniteF64::new(0.5).expect("finite"));
    assert_eq!(
        refused(&sampled).as_deref(),
        Some("lash:unsupported_generation_option")
    );

    // Off always needs the host capability's `disable`.
    let mut no_disable = effort.clone();
    if let Some(reasoning) = no_disable.reasoning.as_mut() {
        reasoning.disable = false;
    }
    assert_eq!(
        refused(&dialect_request(Legacy, Disabled, no_disable)).as_deref(),
        Some("lash:unsupported_effort")
    );
}

#[test]
fn expose_thinking_requests_thoughts_without_a_reasoning_selection() {
    let provider = GoogleOAuthProvider::for_test().with_options(ProviderOptions {
        expose_thinking: true,
        ..ProviderOptions::default()
    });
    let (body, receipt) =
        GoogleOAuthProvider::build_request_with_receipt(&provider, &request(None), vec![], None)
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
}

#[test]
fn an_effort_without_capability_is_refused() {
    let error = GoogleOAuthProvider::build_request(
        &GoogleOAuthProvider::for_test(),
        &request_with_capability(Some("medium"), ModelCapability::default()),
        Vec::new(),
        None,
    )
    .expect_err("an effort without capability is refused");

    assert_eq!(
        refusal_code(&error).as_deref(),
        Some("lash:effort_not_configurable")
    );
}
