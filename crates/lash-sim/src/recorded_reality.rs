use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use lash_core::llm::types::{
    LlmEventSender, LlmJsonSchema, LlmMessage, LlmOutputSpec, LlmRequest, LlmRole,
    LlmTerminalReason, LlmToolChoice,
};
use lash_core::provider::{DefaultProviderFailureClassifier, Provider, ProviderFailureClassifier};
use lash_core::{ProviderFailureKind, facade_support::ProviderFailure};
use lash_llm_transport::{LlmHttpRequest, LlmHttpTransport, read_http_body_text};
use lash_provider_anthropic::AnthropicProvider;
use lash_provider_auth::{
    CredentialError, CredentialErrorKind, OAuthError, classify_oauth_refresh_error,
};
use lash_provider_google::GoogleOAuthProvider;
use lash_provider_openai::{OpenAiCompatibleProvider, OpenAiProvider};
use serde_json::json;

use crate::{
    ProviderWireProvenance, ProviderWireProvenanceKind, ProviderWireScript,
    ScriptedLlmHttpTransport,
};

const GOOGLE_PER_MINUTE: &str = include_str!(
    "../provider-scripts/recorded-reality/google.generate-content-per-minute-429.json"
);
const GOOGLE_HARD_QUOTA: &str = include_str!(
    "../provider-scripts/recorded-reality/google.generate-content-hard-quota-429.json"
);
const OPENAI_PER_MINUTE: &str =
    include_str!("../provider-scripts/recorded-reality/openai.chat-per-minute-429.json");
const OPENAI_HARD_QUOTA: &str =
    include_str!("../provider-scripts/recorded-reality/openai.chat-hard-quota-429.json");
const ANTHROPIC_RATE_LIMIT: &str =
    include_str!("../provider-scripts/recorded-reality/anthropic.messages-rate-limit-429.json");
const ANTHROPIC_HARD_QUOTA: &str =
    include_str!("../provider-scripts/recorded-reality/anthropic.messages-hard-quota-400.json");
const OAUTH_INVALID_GRANT: &str =
    include_str!("../provider-scripts/recorded-reality/oauth.token-invalid-grant-400.json");
const OAUTH_INVALID_CLIENT: &str =
    include_str!("../provider-scripts/recorded-reality/oauth.token-invalid-client-401.json");
const OAUTH_INVALID_SCOPE: &str =
    include_str!("../provider-scripts/recorded-reality/oauth.token-invalid-scope-400.json");
const STRUCTURED_REFUSAL: &str =
    include_str!("../provider-scripts/recorded-reality/openai.chat-structured-refusal.json");
const STRUCTURED_TRUNCATION: &str = include_str!(
    "../provider-scripts/recorded-reality/openai.responses-structured-truncation.json"
);

const ALL_RECORDED_REALITY_SCRIPTS: &[(&str, &str)] = &[
    (
        "google.generate-content-per-minute-429.json",
        GOOGLE_PER_MINUTE,
    ),
    (
        "google.generate-content-hard-quota-429.json",
        GOOGLE_HARD_QUOTA,
    ),
    ("openai.chat-per-minute-429.json", OPENAI_PER_MINUTE),
    ("openai.chat-hard-quota-429.json", OPENAI_HARD_QUOTA),
    (
        "anthropic.messages-rate-limit-429.json",
        ANTHROPIC_RATE_LIMIT,
    ),
    (
        "anthropic.messages-hard-quota-400.json",
        ANTHROPIC_HARD_QUOTA,
    ),
    ("oauth.token-invalid-grant-400.json", OAUTH_INVALID_GRANT),
    ("oauth.token-invalid-client-401.json", OAUTH_INVALID_CLIENT),
    ("oauth.token-invalid-scope-400.json", OAUTH_INVALID_SCOPE),
    ("openai.chat-structured-refusal.json", STRUCTURED_REFUSAL),
    (
        "openai.responses-structured-truncation.json",
        STRUCTURED_TRUNCATION,
    ),
];

fn transport(script: &str) -> Arc<ScriptedLlmHttpTransport> {
    Arc::new(ScriptedLlmHttpTransport::from_json_str(script).expect("valid recorded fixture"))
}

fn request(model: &str, stream: bool, structured: bool) -> LlmRequest {
    LlmRequest {
        model: model.to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "recorded-session",
            "recorded-session:frame:1",
            "recorded-session:request:1",
        ),
        output_spec: structured.then(|| {
            LlmOutputSpec::JsonSchema(LlmJsonSchema {
                name: "answer".to_string(),
                schema: json!({
                    "type": "object",
                    "properties": { "answer": { "type": "string" } }
                })
                .into(),
                strict: true,
            })
        }),
        stream_events: stream.then(|| LlmEventSender::new(|_event| {})),
        provider_trace: None,
    }
}

fn classify(failure: ProviderFailure) -> ProviderFailure {
    DefaultProviderFailureClassifier.classify(failure)
}

#[tokio::test]
async fn google_per_minute_throttle_is_retryable_and_honors_retry_info() {
    let mut provider = GoogleOAuthProvider::new(
        "access-token",
        "refresh-token",
        0,
        lash_provider_google::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    )
    .with_project_id(Some("project-1".to_string()))
    .with_transport(transport(GOOGLE_PER_MINUTE));
    let failure = provider
        .complete(request("gemini-3.1-pro-preview", false, false))
        .await
        .expect_err("recorded 429");
    assert_eq!(failure.retry_after, Some(Duration::from_secs(55)));
    let failure = classify(failure);
    assert_eq!(failure.kind, ProviderFailureKind::Quota);
    assert!(failure.retryable);
    assert_eq!(failure.retry_after, Some(Duration::from_secs(55)));
}

#[tokio::test]
async fn google_hard_quota_is_not_retried_as_a_per_minute_throttle() {
    let mut provider = GoogleOAuthProvider::new(
        "access-token",
        "refresh-token",
        0,
        lash_provider_google::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    )
    .with_project_id(Some("project-1".to_string()))
    .with_transport(transport(GOOGLE_HARD_QUOTA));
    let failure = classify(
        provider
            .complete(request("gemini-3.1-pro-preview", false, false))
            .await
            .expect_err("recorded hard quota"),
    );
    assert_eq!(failure.kind, ProviderFailureKind::Quota);
    assert!(!failure.retryable);
    assert_eq!(failure.retry_after, None);
}

#[tokio::test]
async fn openai_per_minute_throttle_stays_retryable_without_inventing_backoff() {
    let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
        .with_transport(transport(OPENAI_PER_MINUTE));
    let failure = classify(
        provider
            .complete(request("gpt-5.4", false, false))
            .await
            .expect_err("recorded OpenAI throttle"),
    );
    assert_eq!(failure.kind, ProviderFailureKind::Quota);
    assert!(failure.retryable);
    assert_eq!(failure.retry_after, None);
    assert!(
        failure
            .raw
            .as_deref()
            .is_some_and(|raw| raw.contains("3.646s"))
    );
}

#[tokio::test]
async fn openai_insufficient_quota_is_non_retryable() {
    let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
        .with_transport(transport(OPENAI_HARD_QUOTA));
    let failure = classify(
        provider
            .complete(request("gpt-5.4", false, false))
            .await
            .expect_err("recorded OpenAI hard quota"),
    );
    assert_eq!(failure.kind, ProviderFailureKind::Quota);
    assert!(!failure.retryable);
    assert_eq!(failure.retry_after, None);
}

#[tokio::test]
async fn anthropic_rate_limit_and_credit_exhaustion_take_different_retry_paths() {
    let mut rate_limited = AnthropicProvider::new("test-key")
        .with_base_url(Some("https://provider.test".to_string()))
        .with_transport(transport(ANTHROPIC_RATE_LIMIT));
    let rate_failure = classify(
        rate_limited
            .complete(request("claude-sonnet-4-20250514", true, false))
            .await
            .expect_err("recorded Anthropic rate limit"),
    );
    assert_eq!(rate_failure.kind, ProviderFailureKind::Quota);
    assert!(rate_failure.retryable);
    assert_eq!(rate_failure.retry_after, None);

    let mut exhausted = AnthropicProvider::new("test-key")
        .with_base_url(Some("https://provider.test".to_string()))
        .with_transport(transport(ANTHROPIC_HARD_QUOTA));
    let quota_failure = classify(
        exhausted
            .complete(request("claude-sonnet-4-20250514", true, false))
            .await
            .expect_err("recorded Anthropic credit exhaustion"),
    );
    assert_eq!(quota_failure.kind, ProviderFailureKind::Quota);
    assert!(!quota_failure.retryable);
}

async fn classify_oauth_fixture(script: &str) -> CredentialError {
    let response = transport(script)
        .send(
            LlmHttpRequest::post("https://oauth.example/token", Bytes::new()),
            None,
        )
        .await
        .expect("scripted token endpoint response");
    let status = response.status;
    let body = read_http_body_text(response.body, None, "read OAuth fixture")
        .await
        .expect("OAuth fixture body");
    classify_oauth_refresh_error(OAuthError::token_endpoint(
        status,
        &body,
        "token endpoint rejected refresh",
    ))
}

#[tokio::test]
async fn oauth_error_bodies_drive_the_structured_refresh_classifier() {
    let invalid_grant = classify_oauth_fixture(OAUTH_INVALID_GRANT).await;
    assert_eq!(invalid_grant.kind, CredentialErrorKind::InvalidGrant);
    assert!(!invalid_grant.retryable);

    for sibling in [OAUTH_INVALID_CLIENT, OAUTH_INVALID_SCOPE] {
        let classified = classify_oauth_fixture(sibling).await;
        assert_eq!(classified.kind, CredentialErrorKind::Other);
        assert!(!classified.retryable);
    }
}

#[tokio::test]
async fn structured_output_refusal_is_content_filter_not_empty_provider_error() {
    let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
        .with_transport(transport(STRUCTURED_REFUSAL));
    let response = provider
        .complete(request("gpt-4o-2024-08-06", false, true))
        .await
        .expect("documented refusal is a terminal response");
    assert_eq!(response.terminal_reason, LlmTerminalReason::ContentFilter);
    assert_eq!(
        response.full_text,
        "I'm sorry, I cannot assist with that request."
    );
}

#[tokio::test]
async fn structured_output_truncation_is_output_limit_not_provider_error() {
    let mut provider =
        OpenAiProvider::new("test-key").with_transport(transport(STRUCTURED_TRUNCATION));
    let response = provider
        .complete(request("gpt-4o-mini-2024-07-18", true, true))
        .await
        .expect("documented incomplete event is terminal evidence");
    assert_eq!(response.terminal_reason, LlmTerminalReason::OutputLimit);
    assert!(response.full_text.is_empty());
}

#[test]
fn every_recorded_reality_fixture_carries_reviewable_provenance() {
    let fixture_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("provider-scripts/recorded-reality");
    let files_on_disk = std::fs::read_dir(&fixture_dir)
        .expect("recorded-reality fixture directory")
        .map(|entry| entry.expect("recorded-reality directory entry").file_name())
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext == "json")
        })
        .map(|name| {
            name.into_string()
                .expect("recorded-reality fixture filename must be UTF-8")
        })
        .collect::<std::collections::BTreeSet<_>>();
    let listed_files = ALL_RECORDED_REALITY_SCRIPTS
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        files_on_disk, listed_files,
        "recorded-reality fixtures on disk must exactly match ALL_RECORDED_REALITY_SCRIPTS"
    );

    for (name, fixture) in ALL_RECORDED_REALITY_SCRIPTS {
        let script = ProviderWireScript::from_json_str(fixture).expect("valid fixture");
        let provenance = script.provenance.expect("fixture provenance");
        validate_recorded_reality_provenance(&provenance)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }
}

#[test]
fn reviewed_live_capture_has_valid_recorded_reality_provenance() {
    let provenance = ProviderWireProvenance {
        kind: ProviderWireProvenanceKind::CapturedLive,
        source: "/v1/chat/completions".to_string(),
        captured_at: Some("2026-07-22T10:15:00Z".to_string()),
        notes: Some("Dedicated test project; redaction reviewed".to_string()),
    };
    assert!(validate_recorded_reality_provenance(&provenance).is_ok());
}

fn validate_recorded_reality_provenance(provenance: &ProviderWireProvenance) -> Result<(), String> {
    match provenance.kind {
        ProviderWireProvenanceKind::ProviderDocumentation
        | ProviderWireProvenanceKind::RealWorldReport => {
            if !provenance.source.starts_with("https://") {
                return Err("documentary provenance source must be an HTTPS URL".to_string());
            }
        }
        ProviderWireProvenanceKind::CapturedLive => {
            if !provenance.source.starts_with('/')
                || provenance.source.contains('?')
                || provenance.source.contains('#')
            {
                return Err(
                    "captured-live provenance source must be an endpoint path without query or fragment"
                        .to_string(),
                );
            }
            let captured_at = provenance
                .captured_at
                .as_deref()
                .ok_or_else(|| "captured-live provenance requires captured_at".to_string())?;
            chrono::DateTime::parse_from_rfc3339(captured_at).map_err(|_| {
                "captured-live captured_at must be an RFC 3339 timestamp".to_string()
            })?;
            if !provenance
                .notes
                .as_deref()
                .is_some_and(|notes| notes.to_ascii_lowercase().contains("redaction reviewed"))
            {
                return Err(
                    "captured-live provenance notes must confirm `redaction reviewed`".to_string(),
                );
            }
        }
    }
    Ok(())
}
