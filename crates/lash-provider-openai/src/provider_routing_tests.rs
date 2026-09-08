//! Provider-routing (`OpenAiCompat::provider_routing`) wire-shape tests.

use super::*;

/// A host that has declared the endpoint is OpenRouter *and* opted into
/// parameter-honoring routing. The preset alone does not opt in: that trade is
/// the host's to make.
fn host_configured_routing_provider() -> OpenAiCompatibleProvider {
    provider_with_routing(ProviderRoutingPrefs {
        require_parameters: true,
        ..ProviderRoutingPrefs::default()
    })
}

fn provider_with_routing(prefs: ProviderRoutingPrefs) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::new("key", OPENROUTER_BASE_URL).with_compat(OpenAiCompat {
        provider_routing: Some(prefs),
        ..OpenAiCompat::openrouter()
    })
}

#[test]
fn openrouter_chat_body_emits_zdr_when_enabled() {
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    let body = provider_with_routing(ProviderRoutingPrefs {
        zdr: true,
        ..ProviderRoutingPrefs::default()
    })
    .build_chat_request_body(&req, false)
    .unwrap();

    assert_eq!(
        body["provider"],
        json!({ "require_parameters": false, "zdr": true })
    );
}

#[test]
fn openrouter_chat_body_emits_only_allowlist_when_configured() {
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    let body = provider_with_routing(ProviderRoutingPrefs {
        only: vec!["deepinfra".to_string(), "fireworks".to_string()],
        ..ProviderRoutingPrefs::default()
    })
    .build_chat_request_body(&req, false)
    .unwrap();

    assert_eq!(
        body["provider"],
        json!({
            "require_parameters": false,
            "only": ["deepinfra", "fireworks"],
        })
    );
}

#[test]
fn openrouter_chat_body_emits_combined_provider_preferences() {
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);

    let body = provider_with_routing(ProviderRoutingPrefs {
        require_parameters: true,
        zdr: true,
        only: vec!["deepinfra".to_string(), "fireworks".to_string()],
    })
    .build_chat_request_body(&req, false)
    .unwrap();

    assert_eq!(
        body["provider"],
        json!({
            "require_parameters": true,
            "zdr": true,
            "only": ["deepinfra", "fireworks"],
        })
    );
}

#[test]
fn openrouter_chat_body_requires_supported_parameters_with_json_schema_output() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "extract")]);
    req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
        name: "extraction".to_string(),
        schema: json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"]
        })
        .into(),
        strict: true,
    }));

    let body = host_configured_routing_provider()
        .build_chat_request_body(&req, false)
        .unwrap();

    assert_eq!(body["provider"], json!({ "require_parameters": true }));
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["name"], "extraction");
}

#[test]
fn openrouter_chat_body_requires_supported_parameters_without_output_spec() {
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);

    let body = host_configured_routing_provider()
        .build_chat_request_body(&req, false)
        .unwrap();

    assert_eq!(body["provider"], json!({ "require_parameters": true }));
    assert!(body.get("response_format").is_none());
}

// An explicitly declared but empty `provider_routing` stays on the wire as
// `require_parameters: false` rather than vanishing: the body mirrors the
// declared config exactly. Absence of the whole object means "this endpoint
// does not do provider routing", which is a different statement.
#[test]
fn declared_empty_provider_routing_emits_require_parameters_false() {
    let compat: OpenAiCompat = serde_json::from_value(json!({ "provider_routing": {} })).unwrap();
    assert_eq!(
        compat.provider_routing,
        Some(ProviderRoutingPrefs {
            require_parameters: false,
            zdr: false,
            only: Vec::new(),
        })
    );

    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    let body = OpenAiCompatibleProvider::new("key", OPENROUTER_BASE_URL)
        .with_compat(compat)
        .build_chat_request_body(&req, false)
        .unwrap();

    assert_eq!(body["provider"], json!({ "require_parameters": false }));
}

#[test]
fn default_compat_preset_and_direct_openai_bodies_omit_provider_routing() {
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);

    let default_compat_body = OpenAiCompatibleProvider::new("key", OPENROUTER_BASE_URL)
        .build_chat_request_body(&req, false)
        .unwrap();
    // The OpenRouter preset states endpoint facts; opting into restricted
    // routing is a host decision, so the preset alone emits nothing.
    let preset_body = openrouter_provider()
        .build_chat_request_body(&req, false)
        .unwrap();
    let direct_openai_body = OpenAiProvider::new("key")
        .build_responses_request_body(&req, false)
        .unwrap();

    assert!(default_compat_body.get("provider").is_none());
    assert!(preset_body.get("provider").is_none());
    assert!(direct_openai_body.get("provider").is_none());
}
