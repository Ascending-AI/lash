//! Explicitly opt-in, development-only LLM Provider scenarios for failure UX.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use async_trait::async_trait;
use lash::direct::{LlmOutputPart, LlmStreamEvent, LlmUsage};
use lash::provider::{
    GenerationRetryGuarantee, LlmRequest, LlmResponse, LlmTransportError, Provider,
    ProviderComponents, ProviderFailureKind, ProviderHandle, ProviderOptions, ProviderReliability,
};

pub(crate) const DEV_PROVIDER_SCENARIO_ENV: &str = "AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DevProviderScenario {
    AuthFailureOnce,
    RateLimitOnce,
    PartialOutputFailure,
    FailedProcess,
    ExecBlocked,
    ToolValue,
    RenderedSurface,
    CodeFailure,
    RetryResetPartial,
}

impl DevProviderScenario {
    pub(crate) fn from_environment() -> Result<Option<Self>> {
        let Ok(value) = std::env::var(DEV_PROVIDER_SCENARIO_ENV) else {
            return Ok(None);
        };
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        let scenario = match value {
            "auth-failure-once" => Self::AuthFailureOnce,
            "rate-limit-once" => Self::RateLimitOnce,
            "partial-output-failure" => Self::PartialOutputFailure,
            "failed-process" => Self::FailedProcess,
            "exec-blocked" => Self::ExecBlocked,
            "tool-value" => Self::ToolValue,
            "rendered-surface" => Self::RenderedSurface,
            "code-failure" => Self::CodeFailure,
            "retry-reset-partial" => Self::RetryResetPartial,
            other => bail!(
                "invalid {DEV_PROVIDER_SCENARIO_ENV} `{other}`; expected one of: \
                 auth-failure-once, rate-limit-once, partial-output-failure, failed-process, \
                 exec-blocked, tool-value, rendered-surface, code-failure, retry-reset-partial"
            ),
        };
        Ok(Some(scenario))
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AuthFailureOnce => "auth-failure-once",
            Self::RateLimitOnce => "rate-limit-once",
            Self::PartialOutputFailure => "partial-output-failure",
            Self::FailedProcess => "failed-process",
            Self::ExecBlocked => "exec-blocked",
            Self::ToolValue => "tool-value",
            Self::RenderedSurface => "rendered-surface",
            Self::CodeFailure => "code-failure",
            Self::RetryResetPartial => "retry-reset-partial",
        }
    }

    pub(crate) fn provider(self) -> ProviderHandle {
        let retry_delay_ms = if self == Self::RetryResetPartial {
            2_000
        } else {
            0
        };
        ProviderHandle::new(ProviderComponents::new(Box::new(DevFailureProvider {
            scenario: self,
            calls: Arc::new(AtomicUsize::new(0)),
            options: ProviderOptions {
                reliability: ProviderReliability::default()
                    .max_attempts(2)
                    .base_delay_ms(retry_delay_ms)
                    .max_delay_ms(retry_delay_ms),
                ..ProviderOptions::default()
            },
        })))
    }

    pub(crate) fn tool_provider(self) -> Option<Arc<dyn lash::tools::ToolProvider>> {
        (self == Self::ToolValue).then(|| {
            use lash::tools::ToolDefinitionLashlangExt as _;

            let definition = lash::tools::ToolDefinition::raw(
                "tool:workbench_tool_value",
                "workbench_tool_value",
                "Finish the deterministic workbench scenario with a typed tool value.",
                serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                serde_json::json!({ "type": "object" }),
            )
            .with_lashlang_binding(lash::tools::LashlangToolBinding::new(
                ["workbench_surface"],
                "terminal",
            ));
            Arc::new(lash::tools::StaticToolProvider::new(
                vec![definition],
                DevToolValue,
            )) as Arc<dyn lash::tools::ToolProvider>
        })
    }
}

#[derive(Clone, Copy)]
struct DevToolValue;

#[async_trait]
impl lash::tools::StaticToolExecute for DevToolValue {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolResult {
        debug_assert_eq!(call.name, "workbench_tool_value");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        lash::tools::ToolResult::from_output(
            lash::tools::ToolCallOutput::success(serde_json::json!({ "accepted": true }))
                .with_control(lash::tools::ToolControl::Finish {
                    value: lash::tools::ToolValue::from(serde_json::json!({
                        "event_class": "tool_value",
                        "marker": "FIG-1350 deterministic tool value"
                    })),
                }),
        )
    }
}

#[derive(Clone, Debug)]
struct DevFailureProvider {
    scenario: DevProviderScenario,
    calls: Arc<AtomicUsize>,
    options: ProviderOptions,
}

#[async_trait]
impl Provider for DevFailureProvider {
    fn kind(&self) -> &'static str {
        "workbench-dev-failure"
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::json!({ "scenario": self.scenario.as_str() })
    }

    fn requires_streaming(&self) -> bool {
        true
    }

    fn generation_retry_guarantee(&self, _request: &LlmRequest) -> GenerationRetryGuarantee {
        if self.scenario == DevProviderScenario::RetryResetPartial {
            GenerationRetryGuarantee::Idempotent
        } else {
            GenerationRetryGuarantee::None
        }
    }

    async fn complete(
        &mut self,
        request: LlmRequest,
    ) -> std::result::Result<LlmResponse, LlmTransportError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        match self.scenario {
            DevProviderScenario::AuthFailureOnce if call == 0 => {
                send_delta(&request, "provider authentication check started");
                Err(
                    LlmTransportError::new("development provider rejected credentials mid-turn")
                        .with_status(401)
                        .with_code("dev_auth_rejected"),
                )
            }
            DevProviderScenario::AuthFailureOnce => Ok(streamed_response(
                &request,
                "<lashlang>\nfinish \"session recovered after provider auth failure\"\n</lashlang>",
            )),
            DevProviderScenario::RateLimitOnce if call == 0 => Err(LlmTransportError::new(
                "development provider rate limit; retry is safe",
            )
            .with_status(429)
            .with_retry_after(std::time::Duration::ZERO)
            .with_code("dev_rate_limited")),
            DevProviderScenario::RateLimitOnce => Ok(streamed_response(
                &request,
                "retry observer single-copy marker\n<lashlang>\nfinish \"provider retry succeeded\"\n</lashlang>",
            )),
            DevProviderScenario::PartialOutputFailure if call == 0 => {
                let partial = "paid partial output marker";
                send_delta(&request, partial);
                Err(
                    LlmTransportError::new("development provider interrupted after paid output")
                        .with_kind(ProviderFailureKind::Stream)
                        .with_code("dev_paid_output_interrupted")
                        .retryable(true)
                        .with_output_started(true)
                        .with_partial_response(LlmResponse {
                            full_text: partial.to_string(),
                            parts: vec![LlmOutputPart::Text {
                                text: partial.to_string(),
                                response_meta: None,
                            }],
                            usage: LlmUsage {
                                output_tokens: 4,
                                ..LlmUsage::default()
                            },
                            provider_usage: Some(serde_json::json!({ "output_tokens": 4 })),
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        }),
                )
            }
            DevProviderScenario::PartialOutputFailure => Ok(streamed_response(
                &request,
                "<lashlang>\nfinish \"UNSAFE second generation was purchased\"\n</lashlang>",
            )),
            DevProviderScenario::FailedProcess => Ok(streamed_response(
                &request,
                r#"<lashlang>
process FIG425_deterministic_failure() {
  fail "deterministic durable process failure"
}
start FIG425_deterministic_failure()
finish "started deterministic failing process"
</lashlang>"#,
            )),
            DevProviderScenario::ExecBlocked if call == 0 => Ok(streamed_response(
                &request,
                r#"<lashlang>
sleep for "10m"
finish "exec block unexpectedly returned"
</lashlang>"#,
            )),
            DevProviderScenario::ExecBlocked => Ok(streamed_response(
                &request,
                "<lashlang>\nfinish \"session recovered after break glass\"\n</lashlang>",
            )),
            DevProviderScenario::ToolValue => Ok(streamed_response(
                &request,
                "<lashlang>\nawait workbench_surface.terminal({})?\n</lashlang>",
            )),
            DevProviderScenario::RenderedSurface => {
                send_reasoning(&request, "FIG-1350 deterministic reasoning");
                Ok(streamed_response(
                    &request,
                    "<lashlang>\nfinish { event_class: \"final_value\", marker: \"FIG-1350 deterministic final value\" }\n</lashlang>",
                ))
            }
            DevProviderScenario::CodeFailure => Ok(streamed_response(
                &request,
                "<lashlang>\nfail \"FIG-1350 deterministic code failure\"\n</lashlang>",
            )),
            DevProviderScenario::RetryResetPartial if call == 0 => {
                let partial = "FIG-1350 superseded partial text";
                send_delta(&request, partial);
                Err(
                    LlmTransportError::new("FIG-1350 deterministic retry boundary")
                        .with_kind(ProviderFailureKind::Stream)
                        .with_code("fig1350_retry_reset")
                        .retryable(true)
                        .with_output_started(true)
                        .with_partial_response(LlmResponse {
                            full_text: partial.to_string(),
                            parts: vec![LlmOutputPart::Text {
                                text: partial.to_string(),
                                response_meta: None,
                            }],
                            ..LlmResponse::default()
                        }),
                )
            }
            DevProviderScenario::RetryResetPartial => Ok(streamed_response(
                &request,
                "<lashlang>\nfinish \"FIG-1350 retry replacement\"\n</lashlang>",
            )),
        }
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

fn streamed_response(request: &LlmRequest, text: &str) -> LlmResponse {
    send_delta(request, text);
    LlmResponse {
        full_text: text.to_string(),
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn send_delta(request: &LlmRequest, text: &str) {
    if let Some(events) = request.stream_events.as_ref() {
        events.send(LlmStreamEvent::Delta(text.to_string()));
    }
}

fn send_reasoning(request: &LlmRequest, text: &str) {
    if let Some(events) = request.stream_events.as_ref() {
        events.send(LlmStreamEvent::ReasoningDelta(text.to_string()));
    }
}
