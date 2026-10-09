use std::sync::Arc;
use std::time::Duration;

use lash_core::llm::types::{
    LlmEventSender, LlmJsonSchema, LlmMessage, LlmOutputSpec, LlmRequest, LlmRole,
    LlmTerminalReason, LlmToolChoice,
};
use lash_core::provider::{DefaultProviderFailureClassifier, Provider, ProviderFailureClassifier};
use lash_core::{ProviderFailureKind, facade_support::LlmTransportError};
use lash_provider_anthropic::AnthropicProvider;
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
        instructions: None,
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                {
                    let mut metadata =
                        lash_sansio::llm_profile::LlmProfileMetadata::builder(model.to_string())
                            .context_window_tokens(128_000)
                            .capability(lash_core::LlmProfileCapability::default())
                            .extra_body(Default::default())
                            .request_defaults(lash_core::provider::LlmProfileRequestDefaults {
                                ..lash_core::provider::LlmProfileRequestDefaults::new(
                                    lash_core::provider::CacheRetention::Short,
                                )
                            })
                            .build()
                            .expect("valid profile");
                    metadata.limits.output_tokens =
                        lash_sansio::llm_profile::OutputTokenLimits::new(
                            None,
                            model.starts_with("claude").then_some(4096),
                        )
                        .expect("valid limits");
                    metadata
                },
            ),
        )
        .with_reasoning(Default::default()),
        messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],

        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "recorded-session",
            "recorded-session:frame:1",
            "recorded-session:request:1",
        ),
        output_spec: structured.then(|| {
            LlmOutputSpec::JsonSchema(LlmJsonSchema {
                name: "answer".to_string(),
                schema: lash_sansio::SchemaContract::admit(json!({
                    "type": "object",
                    "properties": { "answer": { "type": "string" } }
                }))
                .expect("valid declared schema"),
                strict: true,
            })
        }),
        stream_events: stream.then(|| LlmEventSender::new(|_event| {})),
        provider_trace: None,
    }
}

fn classify(failure: LlmTransportError) -> LlmTransportError {
    DefaultProviderFailureClassifier.classify(failure)
}

#[tokio::test]
async fn google_per_minute_throttle_is_retryable_and_honors_retry_info() {
    let mut provider = GoogleOAuthProvider::new(std::sync::Arc::new(
        lash_core::provider::ProviderToken::new("access-token"),
    ))
    .with_project_id(Some("project-1".to_string()))
    .with_transport(transport(GOOGLE_PER_MINUTE));
    let failure = provider
        .complete(
            request("gemini-3.1-pro-preview", false, false),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect_err("recorded 429");
    assert_eq!(failure.retry_after(), Some(Duration::from_secs(55)));
    let failure = classify(failure);
    assert_eq!(failure.kind, ProviderFailureKind::Quota);
    assert!(failure.is_retryable());
    assert_eq!(failure.retry_after(), Some(Duration::from_secs(55)));
}

#[tokio::test]
async fn google_hard_quota_is_not_retried_as_a_per_minute_throttle() {
    let mut provider = GoogleOAuthProvider::new(std::sync::Arc::new(
        lash_core::provider::ProviderToken::new("access-token"),
    ))
    .with_project_id(Some("project-1".to_string()))
    .with_transport(transport(GOOGLE_HARD_QUOTA));
    let failure = classify(
        provider
            .complete(
                request("gemini-3.1-pro-preview", false, false),
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect_err("recorded hard quota"),
    );
    assert_eq!(failure.kind, ProviderFailureKind::Quota);
    assert!(!failure.is_retryable());
    assert_eq!(failure.retry_after(), None);
}

#[tokio::test]
async fn openai_per_minute_throttle_stays_retryable_without_inventing_backoff() {
    let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
        .with_transport(transport(OPENAI_PER_MINUTE));
    let failure = classify(
        provider
            .complete(
                request("gpt-5.4", false, false),
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect_err("recorded OpenAI throttle"),
    );
    assert_eq!(failure.kind, ProviderFailureKind::Quota);
    assert!(failure.is_retryable());
    assert_eq!(failure.retry_after(), None);
    assert!(
        failure
            .raw
            .as_deref()
            .is_some_and(|raw| raw.contains("3.646s"))
    );
}

#[tokio::test]
async fn anthropic_rate_limit_and_credit_exhaustion_take_different_retry_paths() {
    let mut rate_limited = AnthropicProvider::new("test-key")
        .with_base_url(Some("https://provider.test".to_string()))
        .with_transport(transport(ANTHROPIC_RATE_LIMIT));
    let rate_failure = classify(
        rate_limited
            .complete(
                request("claude-sonnet-4-20250514", true, false),
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect_err("recorded Anthropic rate limit"),
    );
    assert_eq!(rate_failure.kind, ProviderFailureKind::Quota);
    assert!(rate_failure.is_retryable());
    assert_eq!(rate_failure.retry_after(), None);

    let mut exhausted = AnthropicProvider::new("test-key")
        .with_base_url(Some("https://provider.test".to_string()))
        .with_transport(transport(ANTHROPIC_HARD_QUOTA));
    let quota_failure = classify(
        exhausted
            .complete(
                request("claude-sonnet-4-20250514", true, false),
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect_err("recorded Anthropic credit exhaustion"),
    );
    assert_eq!(quota_failure.kind, ProviderFailureKind::Quota);
    assert!(!quota_failure.is_retryable());
}

#[tokio::test]
async fn structured_output_refusal_is_content_filter_not_empty_provider_error() {
    let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
        .with_transport(transport(STRUCTURED_REFUSAL));
    let response = provider
        .complete(
            request("gpt-4o-2024-08-06", false, true),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect("documented refusal is a terminal response");
    assert_eq!(response.terminal_reason, LlmTerminalReason::ContentFilter);
    assert_eq!(
        response.full_text(),
        "I'm sorry, I cannot assist with that request."
    );
}

#[tokio::test]
async fn structured_output_truncation_is_output_limit_not_provider_error() {
    let mut provider =
        OpenAiProvider::new("test-key").with_transport(transport(STRUCTURED_TRUNCATION));
    let response = provider
        .complete(
            request("gpt-4o-mini-2024-07-18", true, true),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect("documented incomplete event is terminal evidence");
    assert_eq!(response.terminal_reason, LlmTerminalReason::OutputLimit);
    assert!(response.full_text().is_empty());
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
fn provider_fixture_manifest_records_capture_provenance() {
    let provenance = ProviderWireProvenance {
        kind: ProviderWireProvenanceKind::CapturedLive,
        source: "/v1/chat/completions".to_string(),
        captured_at: Some("2026-07-22T10:15:00Z".to_string()),
        notes: Some("Dedicated test project; redaction reviewed".to_string()),
    };
    assert!(validate_recorded_reality_provenance(&provenance).is_ok());
    let wire = serde_json::to_value(&provenance).unwrap();
    assert_eq!(
        wire,
        json!({"kind":"captured_live", "source":"/v1/chat/completions", "captured_at":"2026-07-22T10:15:00Z", "notes":"Dedicated test project; redaction reviewed"})
    );
    let mut script: serde_json::Value = serde_json::from_str(OPENAI_PER_MINUTE).unwrap();
    script["provenance"] = wire.clone();
    let entry = crate::runner::script_manifest_entry(
        "captured-provider.json",
        &serde_json::to_string(&script).unwrap(),
    )
    .unwrap();
    let manifest = serde_json::to_value(entry).unwrap();
    assert_eq!(manifest["provenance"], wire);
    assert_eq!(manifest["path"], "captured-provider.json");
    assert_eq!(manifest["sha256"].as_str().unwrap().len(), 64);
    for (field, value) in [
        ("source", json!("https://unreviewed.test/?secret=1")),
        ("captured_at", json!(null)),
        ("captured_at", json!("yesterday")),
        ("notes", json!(null)),
        ("notes", json!("not reviewed")),
    ] {
        let mut invalid = wire.clone();
        invalid[field] = value;
        let restored: ProviderWireProvenance = serde_json::from_value(invalid).unwrap();
        assert!(
            validate_recorded_reality_provenance(&restored).is_err(),
            "missing provenance gate for {field}"
        );
    }
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

/// Providers own the disclosure policy even when the credential header's name
/// is arbitrary. Both recording transports and provider traces obey it.
#[tokio::test]
async fn no_credentials_reach_request_debug_recordings_or_traces() {
    use lash_core::llm::types::{LlmProviderTraceEvent, LlmProviderTraceSender};
    use lash_core::provider::ProviderToken;
    use lash_provider_anthropic::AnthropicAuthScheme;
    use lash_provider_openai::{CodexProvider, OpenAiWireConfig};
    use lash_sansio::sync::MutexExt;
    use std::sync::Mutex;

    const MARKER: &str = "credential-disclosure-witness";
    const ACCOUNT: &str = "account-disclosure-witness";

    #[derive(Debug)]
    struct InspectRequest {
        inner: Arc<ScriptedLlmHttpTransport>,
        sent: Mutex<Vec<lash_llm_transport::LlmHttpRequest>>,
    }

    #[async_trait::async_trait]
    impl lash_llm_transport::LlmHttpTransport for InspectRequest {
        async fn send(
            &self,
            request: lash_llm_transport::LlmHttpRequest,
            timeout: Option<Duration>,
        ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
            let debug = format!("{request:?}");
            assert!(!debug.contains(MARKER));
            assert!(!debug.contains(ACCOUNT));
            self.sent.lock_recover().push(request.clone());
            self.inner.send(request, timeout).await
        }
    }

    for (lane, model, path, header, prefix) in [
        (
            "openai",
            "gpt-5.4",
            "/responses",
            "authorization",
            "Bearer ",
        ),
        (
            "compatible",
            "gpt-5.4",
            "/chat/completions",
            "x-arbitrary-credential",
            "Custom ",
        ),
        (
            "codex",
            "gpt-5.4",
            "/backend-api/codex/responses",
            "authorization",
            "Bearer ",
        ),
        (
            "google",
            "gemini-3.1-pro-preview",
            "/v1internal:generateContent",
            "authorization",
            "Bearer ",
        ),
        (
            "anthropic_key",
            "claude-sonnet-4-20250514",
            "/v1/messages",
            "x-api-key",
            "",
        ),
        (
            "anthropic_bearer",
            "claude-sonnet-4-20250514",
            "/v1/messages",
            "authorization",
            "Bearer ",
        ),
    ] {
        let mut script: serde_json::Value = serde_json::from_str(OPENAI_HARD_QUOTA).unwrap();
        script["endpoint"]["path"] = json!(path);
        script["request_match"] = json!({"any":true});
        script["timeline"][0]["status"] = json!(400);
        let scripted = transport(&script.to_string());
        let inspect = Arc::new(InspectRequest {
            inner: scripted.clone(),
            sent: Mutex::new(Vec::new()),
        });
        let output = tempfile::tempdir().unwrap();
        let recorder = Arc::new(crate::RecordingLlmHttpTransport::new(
            inspect.clone(),
            crate::ProviderRecordingConfig::new(output.path(), lane, lane).with_request_match(
                crate::ProviderWireRequestMatch {
                    any: false,
                    headers: [(
                        header.to_string(),
                        crate::provider::HeaderMatcher {
                            equals: Some(format!("{prefix}{MARKER}")),
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                },
            ),
        ));
        let mut provider: Box<dyn Provider> = match lane {
            "openai" => Box::new(OpenAiProvider::new(MARKER).with_transport(recorder.clone())),
            "compatible" => Box::new(
                OpenAiCompatibleProvider::new(MARKER, "https://provider.test")
                    .with_wire_config(OpenAiWireConfig {
                        auth_header_name: header.to_string(),
                        auth_value_prefix: prefix.to_string(),
                        ..Default::default()
                    })
                    .with_transport(recorder.clone()),
            ),
            "codex" => Box::new(
                CodexProvider::new(Arc::new(ProviderToken::new(MARKER).with_account(ACCOUNT)))
                    .force_sse_transport()
                    .with_http_transport(recorder.clone()),
            ),
            "google" => Box::new(
                GoogleOAuthProvider::new(Arc::new(ProviderToken::new(MARKER)))
                    .with_project_id(Some("project-1".to_string()))
                    .with_transport(recorder.clone()),
            ),
            "anthropic_key" => Box::new(
                AnthropicProvider::with_token_source(Arc::new(ProviderToken::new(MARKER)))
                    .with_transport(recorder.clone()),
            ),
            "anthropic_bearer" => Box::new(
                AnthropicProvider::with_token_source(Arc::new(ProviderToken::new(MARKER)))
                    .with_auth_scheme(AnthropicAuthScheme::Bearer)
                    .with_transport(recorder.clone()),
            ),
            _ => unreachable!(),
        };
        let traces = Arc::new(Mutex::new(Vec::<LlmProviderTraceEvent>::new()));
        let sink = traces.clone();
        let mut req = request(model, false, false);
        req.provider_trace = Some(LlmProviderTraceSender::new(move |event| {
            sink.lock_recover().push(event)
        }));
        let failure = provider
            .complete(
                req,
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect_err("scripted provider rejection");
        assert_eq!(failure.http_status, Some(400), "{lane}: {failure:?}");
        let sent = inspect.sent.lock_recover();
        assert_eq!(sent.len(), 1, "{lane}");
        let credential = sent[0]
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(header))
            .unwrap();
        assert_eq!(credential.1.as_str(), format!("{prefix}{MARKER}"));
        assert!(credential.1.is_sensitive());
        if lane == "codex" {
            let account = sent[0]
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("chatgpt-account-id"))
                .unwrap();
            assert_eq!(account.1.as_str(), ACCOUNT);
            assert!(account.1.is_sensitive());
        }
        if lane.starts_with("anthropic") {
            let other = if lane == "anthropic_key" {
                "authorization"
            } else {
                "x-api-key"
            };
            assert!(
                !sent[0]
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(other))
            );
            let beta =
                lash_llm_transport::first_header_value(&sent[0].headers, "anthropic-beta").unwrap();
            let bearer = lane == "anthropic_bearer";
            assert_eq!(
                beta.split(',').any(|token| token == "oauth-2025-04-20"),
                bearer
            );
            let config = provider.serialize_config();
            assert!(!config.to_string().contains(MARKER));
            assert_eq!(
                config
                    .get("auth_scheme")
                    .and_then(serde_json::Value::as_str),
                bearer.then_some("bearer")
            );
        }
        let paths = recorder.recorded_paths().unwrap();
        assert_eq!(paths.len(), 1, "{lane}");
        let exchanges = scripted.exchanges().unwrap();
        assert_eq!(exchanges.len(), 1, "{lane}");
        let trace_events = traces.lock_recover();
        assert!(
            trace_events
                .iter()
                .any(|event| event.request_endpoint().is_some())
        );
        for exported in [
            std::fs::read_to_string(&paths[0]).unwrap(),
            serde_json::to_string(&exchanges).unwrap(),
            serde_json::to_string(&trace_events.iter().map(|event| json!({"provider":event.provider, "direction":event.direction, "raw":event.raw})).collect::<Vec<_>>()).unwrap(),
            format!("{trace_events:?}"),
        ] {
            assert!(!exported.contains(MARKER), "{lane} credential leaked");
            assert!(!exported.contains(ACCOUNT), "{lane} account leaked");
        }
    }
}

#[tokio::test]
async fn wire_header_redaction_obeys_flags_instead_of_names_or_value_patterns() {
    use lash_llm_transport::{HttpHeaderValue, LlmHttpRequest, LlmHttpTransport};
    let mut script: serde_json::Value = serde_json::from_str(OPENAI_HARD_QUOTA).unwrap();
    script["request_match"] = json!({"any":true});
    let scripted = transport(&script.to_string());
    let output = tempfile::tempdir().unwrap();
    let recorder = crate::RecordingLlmHttpTransport::new(
        scripted.clone(),
        crate::ProviderRecordingConfig::new(output.path(), "flags", "openai-compatible")
            .with_request_match(
                serde_json::from_value(json!({"headers":{
                    "authorization":{"equals":"Bearer public-fixture"},
                    "x-arbitrary":{"equals":"opaque-marker"}
                }}))
                .unwrap(),
            ),
    );
    let request = LlmHttpRequest::post("https://provider.test/chat/completions", "{}")
        .with_header(
            "authorization",
            HttpHeaderValue::new("Bearer public-fixture"),
        )
        .with_header("x-arbitrary", HttpHeaderValue::sensitive("opaque-marker"));
    recorder.send(request.clone(), None).await.unwrap();
    let exchanges = scripted.exchanges().unwrap();
    assert_eq!(
        exchanges[0].request.headers[0].value,
        "Bearer public-fixture"
    );
    assert_eq!(exchanges[0].request.headers[1].value, "[redacted]");
    let recorded = std::fs::read_to_string(&recorder.recorded_paths().unwrap()[0]).unwrap();
    assert!(recorded.contains("Bearer public-fixture"));
    assert!(!recorded.contains("opaque-marker"));
    script["request_match"] = json!({"headers":{"x-arbitrary":{"equals":"different"}}});
    let rejected = transport(&script.to_string())
        .send(request, None)
        .await
        .unwrap_err();
    assert!(!format!("{rejected:?}").contains("opaque-marker"));
}
