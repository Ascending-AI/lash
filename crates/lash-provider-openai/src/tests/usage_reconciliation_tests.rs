//! FIG-2765: host-invoked usage reconciliation against OpenRouter's
//! generation endpoint, driven by recorded transport fixtures only.

use super::*;
use lash_core::provider::Provider;
use lash_llm_transport::LlmHttpMethod;

/// A recorded `GET /generation` response for a generation the client
/// cancelled mid-stream: OpenRouter still bills it, and still reports the
/// native token counts.
const CANCELLED_GENERATION_BODY: &str = r#"{"data":{"id":"gen-123","model":"anthropic/claude-sonnet-4.5","streamed":true,"cancelled":true,"total_cost":0.00123,"tokens_prompt":1000,"tokens_completion":40,"native_tokens_prompt":1040,"native_tokens_completion":52,"native_tokens_reasoning":12,"native_tokens_cached":200,"finish_reason":null}}"#;

#[tokio::test]
async fn openrouter_reconciles_a_cancelled_generation_from_its_recorded_lookup() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        vec![("content-type".to_string(), "application/json".to_string())],
        CANCELLED_GENERATION_BODY,
    ));
    let mut provider = openrouter_provider().with_transport(transport.clone());

    let reconciled = provider
        .reconcile_usage("gen-123")
        .await
        .expect("lookup succeeds")
        .expect("generation is known to OpenRouter");

    let requests = transport.requests.lock_recover();
    assert_eq!(requests.len(), 1, "one lookup, no retry on success");
    let lookup = &requests[0];
    assert_eq!(lookup.method, LlmHttpMethod::Get);
    assert_eq!(
        lookup.url,
        "https://openrouter.ai/api/v1/generation?id=gen-123"
    );
    assert!(lookup.body.is_empty());
    assert!(lookup.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("authorization") && value == "Bearer key"
    }));
    assert!(lookup.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("accept") && value == "application/json"
    }));

    // Native counts win; cached prompt tokens are split out of the input.
    assert_eq!(reconciled.usage.input_tokens, 840);
    assert_eq!(reconciled.usage.cache_read_input_tokens, 200);
    assert_eq!(reconciled.usage.output_tokens, 52);
    assert_eq!(reconciled.usage.reasoning_output_tokens, 12);
    assert_eq!(reconciled.provider_usage["cancelled"], json!(true));
    assert_eq!(reconciled.provider_usage["total_cost"], json!(0.00123));
    assert_eq!(reconciled.provider_usage["id"], json!("gen-123"));
}

#[tokio::test]
async fn openrouter_generation_lookup_percent_encodes_the_id() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        CANCELLED_GENERATION_BODY,
    ));
    let mut provider = openrouter_provider().with_transport(transport.clone());
    provider
        .reconcile_usage("gen 1&2")
        .await
        .expect("lookup succeeds");
    let requests = transport.requests.lock_recover();
    assert_eq!(
        requests[0].url,
        "https://openrouter.ai/api/v1/generation?id=gen%201%262"
    );
}

#[tokio::test]
async fn openrouter_generation_lookup_retries_once_after_a_miss() {
    let transport = Arc::new(ScriptedHttpTransport {
        responses: std::sync::Mutex::new(VecDeque::from(vec![
            (404, Vec::new(), r#"{"error":{"message":"not found"}}"#),
            (200, Vec::new(), CANCELLED_GENERATION_BODY),
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut provider = openrouter_provider().with_transport(transport.clone());
    let reconciled = provider
        .reconcile_usage("gen-123")
        .await
        .expect("lookup succeeds")
        .expect("second attempt finds the generation");
    assert_eq!(transport.calls(), 2);
    assert_eq!(reconciled.usage.output_tokens, 52);
}

#[tokio::test]
async fn openrouter_generation_lookup_reports_a_persistent_miss_as_unresolved() {
    let transport = Arc::new(ScriptedHttpTransport {
        responses: std::sync::Mutex::new(VecDeque::from(vec![
            (404, Vec::new(), r#"{"error":{"message":"not found"}}"#),
            (404, Vec::new(), r#"{"error":{"message":"not found"}}"#),
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut provider = openrouter_provider().with_transport(transport.clone());
    let reconciled = provider
        .reconcile_usage("gen-missing")
        .await
        .expect("a miss is not an error");
    assert!(reconciled.is_none());
    assert_eq!(transport.calls(), 2, "bounded: one retry, then give up");
}

#[tokio::test]
async fn openrouter_generation_lookup_surfaces_server_failures() {
    let transport = Arc::new(ScriptedHttpTransport {
        responses: std::sync::Mutex::new(VecDeque::from(vec![
            (500, Vec::new(), r#"{"error":{"message":"boom"}}"#),
            (429, Vec::new(), r#"{"error":{"message":"slow down"}}"#),
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut provider = openrouter_provider().with_transport(transport.clone());
    let error = provider
        .reconcile_usage("gen-123")
        .await
        .expect_err("a non-404 failure is reported");
    assert_eq!(error.kind, ProviderFailureKind::Quota);
    assert_eq!(error.status, Some(429));
    assert_eq!(transport.calls(), 2);
}

#[tokio::test]
async fn non_openrouter_compat_has_no_usage_reconciliation() {
    let transport = Arc::new(RecordingHttpTransport::default());
    let mut provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_transport(transport.clone());
    let reconciled = provider
        .reconcile_usage("gen-123")
        .await
        .expect("no lookup, no error");
    assert!(reconciled.is_none());
    assert!(transport.requests.lock_recover().is_empty());

    let mut openai = OpenAiProvider::new("key").with_transport(transport.clone());
    assert!(
        openai
            .reconcile_usage("resp_123")
            .await
            .expect("no lookup, no error")
            .is_none()
    );
    assert!(transport.requests.lock_recover().is_empty());
}

#[test]
fn compat_usage_reconciliation_round_trips_and_stays_optional() {
    let compat = OpenAiCompat::openrouter();
    assert_eq!(
        compat.usage_reconciliation,
        Some(UsageReconciliation::OpenRouterGeneration)
    );
    let encoded = serde_json::to_value(&compat).expect("encode compat");
    assert_eq!(
        encoded["usage_reconciliation"],
        json!("open_router_generation")
    );
    let decoded: OpenAiCompat = serde_json::from_value(encoded).expect("decode compat");
    assert_eq!(decoded, compat);

    let legacy: OpenAiCompat = serde_json::from_value(json!({})).expect("legacy compat");
    assert_eq!(legacy.usage_reconciliation, None);
}
