//! Compiled sources for the Rust snippets on `docs/architecture/providers.html`.

use std::sync::Arc;

use lash::provider::{
    LlmRequest, LlmResponse, LlmTransportError, Provider, ProviderComponents, ProviderHandle,
    ProviderOptions,
};

// docs:start:admission-window
/// Host-owned admission: bounded in-flight windows per traffic class,
/// wrapped around the provider the host installs. Breakers, AIMD windows,
/// and backpressure metrics slot into the same `complete()` seam.
#[derive(Debug)]
struct AdmissionGate {
    inner: Box<dyn Provider>,
    interactive_slots: Arc<tokio::sync::Semaphore>,
    batch_slots: Arc<tokio::sync::Semaphore>,
}

impl Clone for AdmissionGate {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone_boxed(),
            // Shared handles: every clone admits through the same windows.
            interactive_slots: Arc::clone(&self.interactive_slots),
            batch_slots: Arc::clone(&self.batch_slots),
        }
    }
}

#[async_trait::async_trait]
impl Provider for AdmissionGate {
    async fn complete(&mut self, request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        // Class traffic by the session identity the host already owns:
        // deliberate ids for its own pipelines, and a default lane for ids
        // it did not mint (lash-spawned child sessions).
        let lane = if request.scope.session_id.starts_with("batch:") {
            &self.batch_slots
        } else {
            &self.interactive_slots
        };
        // The permit drops on every exit path — success, failure, or a
        // cancelled turn — so an aborted call never leaks a slot.
        let _slot = lane.acquire().await.expect("admission gate closed");
        self.inner.complete(request).await
    }

    // Forward `close()` explicitly: the default impl is a no-op and would
    // silently skip the inner provider's transport shutdown.
    async fn close(&self) -> Result<(), LlmTransportError> {
        self.inner.close().await
    }

    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn options(&self) -> ProviderOptions {
        self.inner.options()
    }
    fn set_options(&mut self, options: ProviderOptions) {
        self.inner.set_options(options);
    }
    fn serialize_config(&self) -> serde_json::Value {
        self.inner.serialize_config()
    }
    fn requires_streaming(&self) -> bool {
        self.inner.requires_streaming()
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}
// docs:end:admission-window

fn install_admission_gate(components: ProviderComponents) -> ProviderHandle {
    // docs:start:admission-wrap
    let interactive_slots = Arc::new(tokio::sync::Semaphore::new(8));
    let batch_slots = Arc::new(tokio::sync::Semaphore::new(2));
    let handle = ProviderHandle::new(components.map_provider(|inner| {
        Box::new(AdmissionGate {
            inner,
            interactive_slots,
            batch_slots,
        })
    }));
    // docs:end:admission-wrap
    handle
}

#[cfg(test)]
mod asserted_examples {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use lash::provider::{
        CacheControlDialect, ModelCapability, ProviderFailureKind, ProviderOptions,
        ProviderRateLimitPolicy, ProviderReliability, ProviderRetryPolicy, ReasoningCapability,
        ReasoningDisableEncoding, ReasoningEncoding, ReasoningSelection, RequestTimeout,
        SamplingCapability, StreamTermination,
    };

    #[test]
    fn provider_policy_resolves_host_capabilities_into_runtime_limits_and_wire_values() {
        let empty_capability = ModelCapability::default();
        assert!(ModelCapability::is_empty(&empty_capability));
        assert!(ModelCapability::allows_caller_temperature(
            &empty_capability
        ));

        let reasoning = ReasoningCapability {
            efforts: vec!["low".to_string(), "high".to_string()],
            default_effort: Some("low".to_string()),
            aliases: BTreeMap::from([("xhigh".to_string(), "high".to_string())]),
            encoding: ReasoningEncoding::Budget(BTreeMap::from([
                ("low".to_string(), 1_024),
                ("high".to_string(), 8_192),
            ])),
            disable: Some(ReasoningDisableEncoding::Budget(0)),
            mandatory: true,
        };
        assert_eq!(reasoning.default_effort.as_deref(), Some("low"));
        assert_eq!(reasoning.efforts, ["low", "high"]);
        assert_eq!(reasoning.aliases["xhigh"], "high");
        assert!(reasoning.mandatory);

        let capability = ModelCapability {
            reasoning: Some(reasoning),
            cache_control: Some(CacheControlDialect::Anthropic),
            stream_termination: Some(StreamTermination::RequireTerminalEvidence),
            sampling: SamplingCapability::Pinned,
        };
        assert!(!ModelCapability::is_empty(&capability));
        assert!(!ModelCapability::allows_caller_temperature(&capability));
        assert_eq!(
            ModelCapability::resolve_effort(&capability, " XHIGH ").as_deref(),
            Some("high")
        );
        let resolved = ModelCapability::validate_selection(
            &capability,
            "reasoning-model",
            "provider:test",
            &ReasoningSelection::Effort("xhigh".to_string()),
        )
        .expect("the advertised alias must resolve");
        assert_eq!(ReasoningSelection::effort(&resolved), Some("high"));
        assert!(
            ModelCapability::validate_selection(
                &capability,
                "reasoning-model",
                "provider:test",
                &ReasoningSelection::ProviderDefault,
            )
            .is_err()
        );
        assert_eq!(
            ModelCapability::validate_selection(
                &capability,
                "reasoning-model",
                "provider:test",
                &ReasoningSelection::Disabled,
            )
            .expect("the capability defines a disabled wire encoding"),
            ReasoningSelection::Disabled
        );

        let capability_wire = serde_json::to_value(&capability).expect("capability must serialize");
        assert_eq!(capability_wire["cache_control"], "anthropic");
        assert_eq!(
            capability_wire["stream_termination"],
            "require_terminal_evidence"
        );
        assert_eq!(capability_wire["sampling"], "pinned");
        assert_eq!(
            capability_wire["reasoning"]["encoding"]["budget"]["high"],
            8_192
        );
        assert_eq!(capability_wire["reasoning"]["disable"]["budget"], 0);

        let capability_variants =
            serde_json::to_value([CacheControlDialect::Anthropic, CacheControlDialect::Gemini])
                .expect("cache dialects must serialize");
        assert_eq!(
            capability_variants,
            serde_json::json!(["anthropic", "gemini"])
        );
        assert_eq!(
            serde_json::to_value([SamplingCapability::Configurable, SamplingCapability::Pinned,])
                .expect("sampling capabilities must serialize"),
            serde_json::json!(["configurable", "pinned"])
        );
        assert!(SamplingCapability::is_default(
            &SamplingCapability::Configurable
        ));
        assert_eq!(
            serde_json::to_value([
                StreamTermination::RequireTerminalEvidence,
                StreamTermination::EofTolerated,
            ])
            .expect("stream policies must serialize"),
            serde_json::json!(["require_terminal_evidence", "eof_tolerated"])
        );
        assert_eq!(
            serde_json::to_value([
                ReasoningDisableEncoding::Native,
                ReasoningDisableEncoding::Omit,
                ReasoningDisableEncoding::Effort("none".to_string()),
                ReasoningDisableEncoding::Budget(0),
                ReasoningDisableEncoding::ToggleFalse,
            ])
            .expect("disable encodings must serialize"),
            serde_json::json!(["native", "omit", { "effort": "none" }, { "budget": 0 }, "toggle_false"])
        );
        assert_eq!(
            serde_json::to_value([
                ReasoningEncoding::Effort,
                ReasoningEncoding::Budget(BTreeMap::from([("high".to_string(), 8_192)])),
            ])
            .expect("reasoning encodings must serialize"),
            serde_json::json!(["effort", { "budget": { "high": 8192 } }])
        );

        let reliability = ProviderReliability::codex()
            .request_timeout(Some(RequestTimeout::Millis(45_000)))
            .stream_chunk_timeout_ms(Some(7_500))
            .max_attempts(6)
            .base_delay_ms(125)
            .max_delay_ms(2_000)
            .retry_after_cap_ms(Some(15_000))
            .throttle_wait_budget_ms(30_000)
            .max_concurrency(Some(8))
            .requests_per_window(Some(120), Some(60_000))
            .tokens_per_window(Some(200_000), Some(60_000));
        assert_eq!(
            reliability.request_timeout,
            Some(RequestTimeout::Millis(45_000))
        );
        assert_eq!(reliability.chunk_timeout, Some(7_500));
        assert_eq!(reliability.retry.max_attempts, 6);
        assert_eq!(reliability.retry.base_delay_ms, 125);
        assert_eq!(reliability.retry.max_delay_ms, 2_000);
        assert_eq!(reliability.retry.jitter_ms, 0);
        assert_eq!(reliability.retry.retry_after_cap_ms, Some(15_000));
        assert_eq!(reliability.retry.throttle_wait_budget_ms, 30_000);
        assert!(reliability.retry.enabled);
        assert_eq!(reliability.rate_limits.max_concurrency, Some(8));
        assert_eq!(reliability.rate_limits.requests_per_window, Some(120));
        assert_eq!(reliability.rate_limits.request_window_ms, Some(60_000));
        assert_eq!(reliability.rate_limits.tokens_per_window, Some(200_000));
        assert_eq!(reliability.rate_limits.token_window_ms, Some(60_000));
        let timeouts = ProviderReliability::llm_timeouts(&reliability);
        assert_eq!(timeouts.request_timeout, Some(Duration::from_secs(45)));
        assert_eq!(timeouts.chunk_timeout, Duration::from_millis(7_500));

        let disabled =
            ProviderReliability::disabled().request_timeout(Some(RequestTimeout::Disabled));
        assert!(!disabled.retry.enabled);
        assert_eq!(disabled.llm_timeouts().request_timeout, None);
        assert_eq!(ProviderRetryPolicy::disabled().max_attempts, 1);

        let retry = ProviderRetryPolicy {
            enabled: true,
            max_attempts: 3,
            base_delay_ms: 50,
            max_delay_ms: 500,
            jitter_ms: 5,
            retry_after_cap_ms: Some(1_000),
            throttle_wait_budget_ms: 2_000,
        };
        let rate_limits = ProviderRateLimitPolicy {
            max_concurrency: Some(4),
            requests_per_window: Some(20),
            request_window_ms: Some(10_000),
            tokens_per_window: Some(50_000),
            token_window_ms: Some(10_000),
        };
        let options = ProviderOptions {
            reliability: ProviderReliability {
                request_timeout: Some(RequestTimeout::Millis(9_000)),
                chunk_timeout: Some(3_000),
                retry,
                rate_limits,
            },
            expose_thinking: true,
            max_output_tokens: Some(4_096),
            ..ProviderOptions::default()
        };
        assert!(!ProviderOptions::is_default(&options));
        assert!(options.expose_thinking);
        assert_eq!(options.max_output_tokens, Some(4_096));
        assert_eq!(
            serde_json::to_value(options.cache_retention).unwrap(),
            "short"
        );
        assert_eq!(
            ProviderOptions::llm_timeouts(&options).request_timeout,
            Some(Duration::from_secs(9))
        );
        let options_wire = serde_json::to_value(&options).expect("provider policy must serialize");
        assert_eq!(options_wire["reliability"]["retry"]["max_attempts"], 3);
        assert_eq!(
            options_wire["reliability"]["rate_limits"]["max_concurrency"],
            4
        );
        assert_eq!(options_wire["max_output_tokens"], 4_096);

        assert_eq!(
            serde_json::to_value(RequestTimeout::Disabled).unwrap(),
            false
        );
        assert_eq!(
            serde_json::to_value(RequestTimeout::Millis(750)).unwrap(),
            750
        );
        let failure_kinds = [
            ProviderFailureKind::Transport,
            ProviderFailureKind::Timeout,
            ProviderFailureKind::Http,
            ProviderFailureKind::Stream,
            ProviderFailureKind::Auth,
            ProviderFailureKind::Validation,
            ProviderFailureKind::Quota,
            ProviderFailureKind::Unsupported,
            ProviderFailureKind::Unknown,
        ];
        assert_eq!(
            failure_kinds.map(ProviderFailureKind::code),
            [
                "transport",
                "timeout",
                "http",
                "stream",
                "auth",
                "validation",
                "quota",
                "unsupported",
                "unknown",
            ]
        );
    }
}
