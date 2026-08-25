//! Explicitly opt-in, development-only LLM Provider scenarios for failure UX.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use async_trait::async_trait;
use lash::direct::{
    LlmOutputPart, LlmStreamEvent, LlmUsage, ProviderReasoningReplay, ProviderRouteIdentity,
};
use lash::provider::{
    GenerationRetryGuarantee, LlmContentBlock, LlmMessage, LlmRequest, LlmResponse, LlmRole,
    LlmTransportError, Provider, ProviderComponents, ProviderFailureKind, ProviderHandle,
    ProviderOptions, ProviderReliability,
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
    ReplayRouteChange,
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
            "replay-route-change" => Self::ReplayRouteChange,
            other => bail!(
                "invalid {DEV_PROVIDER_SCENARIO_ENV} `{other}`; expected one of: \
                 auth-failure-once, rate-limit-once, partial-output-failure, failed-process, \
                 exec-blocked, tool-value, rendered-surface, code-failure, retry-reset-partial, \
                 replay-route-change"
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
            Self::ReplayRouteChange => "replay-route-change",
        }
    }

    pub(crate) fn initial_model(self) -> &'static str {
        match self {
            Self::ReplayRouteChange => "dev/replay-route-a",
            _ => "dev/failure-paths",
        }
    }

    /// The scripted provider for this scenario, in the dialect the host is
    /// configured to run.
    ///
    /// ADR 0063 is about prompts; this is the same rule one layer out. Every
    /// reply below is a *cell*, and a cell in the wrong dialect cannot execute:
    /// the session refuses it, the turn never reaches a terminal state, and the
    /// scenario hangs rather than failing. Nine of the twenty-one TypeScript
    /// judged rows boot this provider.
    pub(crate) fn provider(self, dialect: lash::rlm::RlmDialect) -> ProviderHandle {
        let retry_delay_ms = if self == Self::RetryResetPartial {
            2_000
        } else {
            0
        };
        ProviderHandle::new(ProviderComponents::new(Box::new(DevFailureProvider {
            scenario: self,
            dialect,
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

    /// The scripted cells, for the fixture that walks every scenario.
    #[cfg(test)]
    pub(crate) fn scripted_cell_for_test(
        self,
        dialect: lash::rlm::RlmDialect,
        call: usize,
    ) -> Option<String> {
        self.scripted_cell(dialect, call)
    }

    /// The cell this scenario scripts for `call`, in the host's dialect.
    ///
    /// One table rather than literals scattered through `complete`, so the
    /// fixture test can walk every scenario in both dialects and link what a
    /// judged row would actually execute. A cell in the wrong dialect does not
    /// fail a scenario — the session cannot execute it, so the turn never
    /// reaches a terminal state and the row hangs.
    fn scripted_cell(self, dialect: lash::rlm::RlmDialect, call: usize) -> Option<String> {
        Some(match (self, call) {
            (Self::AuthFailureOnce, 0)
            | (Self::RateLimitOnce, 0)
            | (Self::PartialOutputFailure, 0)
            | (Self::RetryResetPartial, 0) => return None,
            (Self::AuthFailureOnce, _) => {
                finish_cell(dialect, "\"session recovered after provider auth failure\"")
            }
            (Self::RateLimitOnce, _) => finish_cell(dialect, "\"provider retry succeeded\""),
            (Self::PartialOutputFailure, _) => {
                finish_cell(dialect, "\"UNSAFE second generation was purchased\"")
            }
            (Self::RetryResetPartial, _) => finish_cell(dialect, "\"FIG-1350 retry replacement\""),
            (Self::FailedProcess, _) => cell(
                dialect,
                r#"process FIG425_deterministic_failure() {
  fail "deterministic durable process failure"
}
start FIG425_deterministic_failure()
finish "started deterministic failing process""#,
                // `fail` is Lashlang's process-only failure keyword and has no
                // direct TypeScript twin, so the honest form is an uncaught
                // throw of a supported value.
                r#"const FIG425_deterministic_failure = defineProcess({
  name: "FIG425_deterministic_failure",
  signals: {},
  run: async (request: unknown) => {
    throw "deterministic durable process failure";
  }
});
start(FIG425_deterministic_failure, { request: 1 });
finish("started deterministic failing process");"#,
            ),
            (Self::ExecBlocked, 0) => cell(
                dialect,
                "sleep for \"10m\"\nfinish \"exec block unexpectedly returned\"",
                "await sleep(600000);\nfinish(\"exec block unexpectedly returned\");",
            ),
            (Self::ExecBlocked, _) => {
                finish_cell(dialect, "\"session recovered after break glass\"")
            }
            (Self::ToolValue, _) => cell(
                dialect,
                "await workbench_surface.terminal({})?",
                "await workbench_surface.terminal({});",
            ),
            (Self::RenderedSurface, _) => finish_cell(
                dialect,
                "{ event_class: \"final_value\", marker: \"FIG-1350 deterministic final value\" }",
            ),
            // The failing cell this scenario exists to render. What shipped was
            // `fail "..."` at cell top level in *both* dialects, and `fail` is
            // process-only: the cell never executed, the turn never reached a
            // terminal state, and the unbounded workbench turn budget re-asked
            // the provider forever (FIG-1407 owns the budget). Each dialect now
            // fails the way a model actually fails, and the retry terminates.
            (Self::CodeFailure, 0) => cell(
                dialect,
                "finish format(\"FIG-1350 deterministic code failure: {} {}\", \"one argument\")",
                "throw \"FIG-1350 deterministic code failure\";",
            ),
            (Self::CodeFailure, _) => {
                finish_cell(dialect, "\"session recovered after code failure\"")
            }
            // Turn-numbered, because the replay-route scenario asserts a
            // distinct answer per turn; `call` is that turn.
            (Self::ReplayRouteChange, _) => finish_cell(
                dialect,
                &format!("\"FIG-1374 replay-route response {call}\""),
            ),
        })
    }

    pub(crate) fn tool_provider(self) -> Option<Arc<dyn lash::tools::ToolProvider>> {
        (self == Self::ToolValue).then(|| {
            use lash::tools::ToolDefinitionBindingExt as _;

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
            .with_tool_binding(lash::tools::ToolBinding::new(
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
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolOutcome {
        debug_assert_eq!(call.name, "workbench_tool_value");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        lash::tools::ToolOutcome::from_output(
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
    dialect: lash::rlm::RlmDialect,
    calls: Arc<AtomicUsize>,
    options: ProviderOptions,
}

#[async_trait]
impl Provider for DevFailureProvider {
    fn kind(&self) -> &'static str {
        "workbench-dev-failure"
    }

    fn route_identity(&self, model: &str) -> lash::direct::ProviderRouteIdentity {
        lash::direct::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::json!({
            "scenario": self.scenario.as_str(),
            "dialect": self.dialect.language_id()
        })
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
            DevProviderScenario::AuthFailureOnce => {
                Ok(streamed_response(&request, &self.cell(call)))
            }
            DevProviderScenario::RateLimitOnce if call == 0 => Err(LlmTransportError::new(
                "development provider rate limit; retry is safe",
            )
            .with_status(429)
            .with_retry_after(std::time::Duration::ZERO)
            .with_code("dev_rate_limited")),
            DevProviderScenario::RateLimitOnce => Ok(streamed_response(
                &request,
                &format!("retry observer single-copy marker\n{}", self.cell(call)),
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
            DevProviderScenario::PartialOutputFailure => {
                Ok(streamed_response(&request, &self.cell(call)))
            }
            DevProviderScenario::FailedProcess
            | DevProviderScenario::ExecBlocked
            | DevProviderScenario::ToolValue
            | DevProviderScenario::CodeFailure => Ok(streamed_response(&request, &self.cell(call))),
            DevProviderScenario::RenderedSurface => {
                send_reasoning(&request, "FIG-1350 deterministic reasoning");
                Ok(streamed_response(&request, &self.cell(call)))
            }
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
            DevProviderScenario::RetryResetPartial => {
                Ok(streamed_response(&request, &self.cell(call)))
            }
            DevProviderScenario::ReplayRouteChange => {
                Ok(replay_route_response(&request, self.dialect))
            }
        }
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

impl DevFailureProvider {
    fn cell(&self, call: usize) -> String {
        self.scenario
            .scripted_cell(self.dialect, call)
            .expect("every response-producing branch scripts a cell")
    }
}

/// One scripted cell, in the dialect the host is running.
fn cell(dialect: lash::rlm::RlmDialect, lashlang: &str, typescript: &str) -> String {
    let body = match dialect {
        lash::rlm::RlmDialect::Lashlang => lashlang,
        lash::rlm::RlmDialect::Typescript => typescript,
    };
    let tag = dialect.language_id();
    format!("<{tag}>\n{body}\n</{tag}>")
}

/// The common shape: one cell that finishes with `value`, spelled per dialect.
fn finish_cell(dialect: lash::rlm::RlmDialect, value: &str) -> String {
    cell(
        dialect,
        &format!("finish {value}"),
        &format!("finish({value});"),
    )
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

fn replay_route_response(request: &LlmRequest, dialect: lash::rlm::RlmDialect) -> LlmResponse {
    let turn = next_replay_route_turn(&request.messages);
    // The row's own dialect, like every other scripted reply here: a Lashlang
    // cell served to a TypeScript session is a wrong-dialect cell, not a
    // dialect-independent one. The table is the single source, so the dialect
    // walk covers this scenario too.
    let text = DevProviderScenario::ReplayRouteChange
        .scripted_cell(dialect, turn)
        .expect("the replay-route scenario scripts every call");
    let reasoning = format!("FIG-1374 portable reasoning {turn}");
    send_reasoning(request, &reasoning);
    send_delta(request, &text);
    LlmResponse {
        full_text: text.clone(),
        parts: vec![
            LlmOutputPart::Reasoning {
                text: reasoning,
                replay: Some(ProviderReasoningReplay {
                    signature: Some(format!("FIG1374-OPAQUE-REPLAY-{turn}")),
                    origin: Some(ProviderRouteIdentity::new(
                        "workbench-dev-failure",
                        "workbench-dev-failure",
                        request.model.clone(),
                    )),
                    ..ProviderReasoningReplay::default()
                }),
            },
            LlmOutputPart::Text {
                text,
                response_meta: None,
            },
        ],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn next_replay_route_turn(messages: &[LlmMessage]) -> usize {
    messages
        .iter()
        .filter(|message| message.role == LlmRole::Assistant)
        .filter(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    LlmContentBlock::Text { text, .. }
                        if text.contains("FIG-1374 replay-route response ")
                )
            })
        })
        .count()
        + 1
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

#[cfg(test)]
mod tests {
    use lash::provider::{LlmMessage, LlmRole};

    use super::{DevProviderScenario, next_replay_route_turn};

    #[test]
    fn replay_route_change_starts_on_route_a() {
        assert_eq!(
            DevProviderScenario::ReplayRouteChange.initial_model(),
            "dev/replay-route-a"
        );
        assert_eq!(
            DevProviderScenario::PartialOutputFailure.initial_model(),
            "dev/failure-paths"
        );
    }

    #[test]
    fn replay_route_turn_counts_completed_scenario_responses_not_rlm_control_messages() {
        let first_request = vec![
            LlmMessage::text(LlmRole::User, "FIG425-RESUME-ONE"),
            LlmMessage::text(LlmRole::User, "=== CURRENT ITERATION: 1 ==="),
        ];
        assert_eq!(next_replay_route_turn(&first_request), 1);

        let second_request = vec![
            LlmMessage::text(LlmRole::User, "FIG425-RESUME-ONE"),
            LlmMessage::text(LlmRole::Assistant, "FIG-1374 replay-route response 1"),
            LlmMessage::text(LlmRole::User, "FIG425-RESUME-TWO"),
            LlmMessage::text(LlmRole::User, "=== CURRENT ITERATION: 1 ==="),
        ];
        assert_eq!(next_replay_route_turn(&second_request), 2);
    }
}
