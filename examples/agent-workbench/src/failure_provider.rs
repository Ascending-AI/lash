//! Explicitly opt-in, development-only LLM Provider scenarios for failure UX.

use lash::{StreamBlockEvent, StreamBlockKind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use async_trait::async_trait;
use lash::direct::{LlmOutputPart, LlmStreamEvent, ProviderReasoningReplay, ProviderRouteIdentity};
use lash::provider::{
    FailureCode, GenerationRetryGuarantee, LlmContentBlock, LlmMessage, LlmRequest, LlmResponse,
    LlmRole, LlmTransportError, Provider, ProviderComponents, ProviderFailureKind, ProviderHandle,
    ProviderOptions, ProviderReliability, RecordedRequestTemplate, TransportRetryVerdict,
};
use lash::usage::LlmUsage;

pub(crate) const DEV_PROVIDER_SCENARIO_ENV: &str = "AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DevProviderScenario {
    McpFixture,
    AuthFailureOnce,
    RateLimitOnce,
    PartialOutputFailure,
    FailedProcess,
    ExecBlocked,
    CrashResume,
    MigrationSweep,
    ToolValue,
    RenderedSurface,
    TranscriptProjection,
    CodeFailure,
    RetryResetPartial,
    ReplayRouteChange,
    #[cfg(feature = "provider-wire-fixtures")]
    ValidEmptyCompletion,
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
            "mcp-fixture" => Self::McpFixture,
            "auth-failure-once" => Self::AuthFailureOnce,
            "rate-limit-once" => Self::RateLimitOnce,
            "partial-output-failure" => Self::PartialOutputFailure,
            "failed-process" => Self::FailedProcess,
            "exec-blocked" => Self::ExecBlocked,
            "crash-resume" => Self::CrashResume,
            "migration-sweep" => Self::MigrationSweep,
            "tool-value" => Self::ToolValue,
            "rendered-surface" => Self::RenderedSurface,
            "transcript-projection" => Self::TranscriptProjection,
            "code-failure" => Self::CodeFailure,
            "retry-reset-partial" => Self::RetryResetPartial,
            "replay-route-change" => Self::ReplayRouteChange,
            "valid-empty-completion" => {
                #[cfg(feature = "provider-wire-fixtures")]
                {
                    Self::ValidEmptyCompletion
                }
                #[cfg(not(feature = "provider-wire-fixtures"))]
                bail!(
                    "{DEV_PROVIDER_SCENARIO_ENV}=valid-empty-completion requires the agent-workbench provider-wire-fixtures feature"
                )
            }
            other => bail!(
                "invalid {DEV_PROVIDER_SCENARIO_ENV} `{other}`; expected one of: \
                 mcp-fixture, auth-failure-once, rate-limit-once, partial-output-failure, failed-process, \
                 exec-blocked, crash-resume, migration-sweep, tool-value, rendered-surface, transcript-projection, code-failure, retry-reset-partial, \
                 replay-route-change, valid-empty-completion"
            ),
        };
        Ok(Some(scenario))
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::McpFixture => "mcp-fixture",
            Self::AuthFailureOnce => "auth-failure-once",
            Self::RateLimitOnce => "rate-limit-once",
            Self::PartialOutputFailure => "partial-output-failure",
            Self::FailedProcess => "failed-process",
            Self::ExecBlocked => "exec-blocked",
            Self::CrashResume => "crash-resume",
            Self::MigrationSweep => "migration-sweep",
            Self::ToolValue => "tool-value",
            Self::RenderedSurface => "rendered-surface",
            Self::TranscriptProjection => "transcript-projection",
            Self::CodeFailure => "code-failure",
            Self::RetryResetPartial => "retry-reset-partial",
            Self::ReplayRouteChange => "replay-route-change",
            #[cfg(feature = "provider-wire-fixtures")]
            Self::ValidEmptyCompletion => "valid-empty-completion",
        }
    }

    pub(crate) fn initial_profile(self) -> &'static str {
        match self {
            Self::ReplayRouteChange => "dev/replay-route-a",
            #[cfg(feature = "provider-wire-fixtures")]
            Self::ValidEmptyCompletion => "openai/gpt-5.4",
            _ => "dev/failure-paths",
        }
    }

    /// The scripted provider for this scenario.
    ///
    /// ADR 0063 is about prompts; this is the same rule one layer out. Every
    /// reply below is a *cell*, and a cell the session cannot execute leaves
    /// the turn short of a terminal state, so the scenario hangs rather than
    /// failing. TypeScript is the sole RLM language (ADR 0096).
    pub(crate) fn provider(self) -> ProviderHandle {
        #[cfg(feature = "provider-wire-fixtures")]
        if self == Self::ValidEmptyCompletion {
            return crate::valid_empty_completion::provider().unwrap_or_else(|err| {
                panic!("the checked-in valid-empty Provider Wire Script must parse: {err:?}")
            });
        }
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
                    .max_attempts(Some(2))
                    .base_delay_ms(retry_delay_ms)
                    .max_delay_ms(retry_delay_ms),
                ..ProviderOptions::default()
            },
        })))
    }

    /// The cell this scenario scripts for `call`.
    ///
    /// One table rather than literals scattered through `complete`. A cell
    /// the session cannot execute does not fail a scenario: the turn never
    /// reaches a terminal state and the row hangs.
    fn scripted_cell(self, call: usize) -> Option<String> {
        Some(match (self, call) {
            (Self::McpFixture, _) => return None,
            (Self::AuthFailureOnce, 0)
            | (Self::RateLimitOnce, 0)
            | (Self::PartialOutputFailure, 0)
            | (Self::RetryResetPartial, 0) => return None,
            (Self::AuthFailureOnce, _) => {
                finish_cell("\"session recovered after provider auth failure\"")
            }
            (Self::RateLimitOnce, _) => finish_cell("\"provider retry succeeded\""),
            (Self::PartialOutputFailure, _) => {
                finish_cell("\"UNSAFE second generation was purchased\"")
            }
            (Self::RetryResetPartial, _) => finish_cell("\"FIG-1350 retry replacement\""),
            (Self::FailedProcess, _) => (r#"const FIG425_deterministic_failure = async (request: unknown) => {
  throw "deterministic durable process failure";
};
await processes.start({ definition: FIG425_deterministic_failure, args: { request: 1 } });
await chat.reply("started deterministic failing process");"#).to_owned(),
            (Self::CrashResume, _) => crate::crash_fixture::CELL.to_owned(),
            (Self::MigrationSweep, _) => crate::crash_fixture::MIGRATION_SEED.to_owned(),
            (Self::ExecBlocked, 0) => "await sleep(600000);\nawait chat.reply(\"exec block unexpectedly returned\");".to_owned(),
            (Self::ExecBlocked, _) => finish_cell("\"session recovered after break glass\""),
            (Self::ToolValue, _) => "await workbench_surface.terminal({});".to_owned(),
            (Self::TranscriptProjection, 0) => "await control.continueAs({task: \"canonical follow-frame task\", seed: {marker: \"hidden-seed\"}});".to_owned(),
            (Self::TranscriptProjection, _) => finish_cell("\"canonical follow-frame reply\""),
            (Self::RenderedSurface, _) => finish_cell(
                "{ event_class: \"final_value\", marker: \"FIG-1350 deterministic final value\" }",
            ),
            // The failing cell this scenario exists to render. It fails the way
            // a model actually fails, so the retry terminates instead of the
            // unbounded workbench turn budget re-asking the provider forever
            // (FIG-1407 owns the budget).
            (Self::CodeFailure, 0) => "throw \"FIG-1350 deterministic code failure\";".to_owned(),
            (Self::CodeFailure, _) => finish_cell("\"session recovered after code failure\""),
            // Turn-numbered, because the replay-route scenario asserts a
            // distinct answer per turn; `call` is that turn.
            (Self::ReplayRouteChange, _) => {
                finish_cell(&format!("\"FIG-1374 replay-route response {call}\""))
            }
            #[cfg(feature = "provider-wire-fixtures")]
            (Self::ValidEmptyCompletion, _) => {
                unreachable!("valid-empty-completion is served by the OpenAI-compatible adapter")
            }
        })
    }

    #[expect(
        clippy::expect_used,
        reason = "this module declares the tool or payload schema and admission checks its invariant"
    )]
    pub(crate) fn tool_provider(self) -> Option<Arc<dyn lash::tools::ToolProvider>> {
        (self == Self::ToolValue).then(|| {
            let definition = lash::tools::ToolDefinition::control(
                "tool:workbench_tool_value",
                "workbench_surface.terminal",
                "Finish the deterministic workbench scenario with a typed tool value.",
                serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                lash::tools::TurnControls::finish(lash::schema::JsonSchema::any()),
            )
            .expect("valid declared tool schemas")
            .with_execution(std::time::Duration::from_secs(120));
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
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        (async {
            debug_assert_eq!(call.name(), "workbench_surface.terminal");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            lash::tools::ToolOutcome::from_output(lash::tools::ToolCallOutput::finish(
                lash::tools::ToolValue::untrusted_json(serde_json::json!({
                    "event_class": "tool_value",
                    "marker": "FIG-1350 deterministic tool value"
                })),
            ))
        })
        .await
        .into()
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
            "dialect": "typescript"
        })
    }

    fn requires_streaming(&self) -> bool {
        true
    }

    fn generation_retry_guarantee(
        &self,
        _context: &lash::provider::ResponseContext,
        _body: &RecordedRequestTemplate,
    ) -> GenerationRetryGuarantee {
        if self.scenario == DevProviderScenario::RetryResetPartial {
            GenerationRetryGuarantee::Idempotent
        } else {
            GenerationRetryGuarantee::None
        }
    }

    async fn send(
        &mut self,
        body: &lash::provider::LiveRequestBody,
        context: lash::provider::ResponseContext,
    ) -> std::result::Result<LlmResponse, LlmTransportError> {
        // The request the call's body says, reporting through this send.
        let request = lash::provider::canonical_request(body, context)?;
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        match self.scenario {
            DevProviderScenario::McpFixture => Ok(mcp_fixture_response(&request)),
            DevProviderScenario::MigrationSweep => {
                let seeded = request.messages.iter().any(|message| {
                    message.role == LlmRole::Assistant && message.blocks.iter().any(|block| {
                        matches!(block, LlmContentBlock::ToolCall { input_json: text, .. } if text.contains("migration-sweep seeded"))
                    })
                });
                let text = (if seeded {
                    crate::crash_fixture::MIGRATION_READ
                } else {
                    crate::crash_fixture::MIGRATION_SEED
                })
                .to_owned();
                Ok(streamed_response(&request, &text))
            }
            DevProviderScenario::AuthFailureOnce if call == 0 => {
                send_delta(&request, "provider authentication check started");
                Err(
                    LlmTransportError::new("development provider rejected credentials mid-turn")
                        .with_http_status(401)
                        .with_code(FailureCode::provider("dev_auth_rejected")),
                )
            }
            DevProviderScenario::AuthFailureOnce => {
                Ok(streamed_response(&request, &self.program(call)))
            }
            DevProviderScenario::RateLimitOnce if call == 0 => {
                let retry_verdict = TransportRetryVerdict::RetryableThrottle {
                    retry_after: Some(std::time::Duration::ZERO),
                };
                debug_assert!(retry_verdict.is_retryable());
                debug_assert_eq!(retry_verdict.retry_after(), Some(std::time::Duration::ZERO));
                Err(
                    LlmTransportError::new("development provider rate limit; retry is safe")
                        .with_http_status(429)
                        .with_retry_verdict(retry_verdict)
                        .with_code(FailureCode::provider("dev_rate_limited")),
                )
            }
            DevProviderScenario::RateLimitOnce => {
                let mut response = streamed_response(&request, &self.program(call));
                let prose = "retry observer single-copy marker\n";
                send_delta(&request, prose);
                response.parts.insert(
                    0,
                    LlmOutputPart::Text {
                        text: prose.to_owned(),
                        response_meta: None,
                    },
                );
                Ok(response)
            }
            DevProviderScenario::PartialOutputFailure if call == 0 => {
                let partial = "paid partial output marker";
                send_delta(&request, partial);
                Err(
                    LlmTransportError::new("development provider interrupted after paid output")
                        .with_kind(ProviderFailureKind::Stream)
                        .with_code(FailureCode::provider("dev_paid_output_interrupted"))
                        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
                        .with_output_started(true)
                        .with_partial_response(LlmResponse {
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
                Ok(streamed_response(&request, &self.program(call)))
            }
            DevProviderScenario::FailedProcess
            | DevProviderScenario::ExecBlocked
            | DevProviderScenario::CrashResume
            | DevProviderScenario::ToolValue
            | DevProviderScenario::CodeFailure => {
                Ok(streamed_response(&request, &self.program(call)))
            }
            DevProviderScenario::RenderedSurface | DevProviderScenario::TranscriptProjection => {
                send_reasoning(&request, "FIG-1350 deterministic reasoning");
                Ok(streamed_response(&request, &self.program(call)))
            }
            DevProviderScenario::RetryResetPartial if call == 0 => {
                let partial = "FIG-1350 superseded partial text";
                send_delta(&request, partial);
                Err(
                    LlmTransportError::new("FIG-1350 deterministic retry boundary")
                        .with_kind(ProviderFailureKind::Stream)
                        .with_code(FailureCode::provider("fig1350_retry_reset"))
                        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
                        .with_output_started(true)
                        .with_partial_response(LlmResponse {
                            parts: vec![LlmOutputPart::Text {
                                text: partial.to_string(),
                                response_meta: None,
                            }],
                            ..LlmResponse::default()
                        }),
                )
            }
            DevProviderScenario::RetryResetPartial => {
                Ok(streamed_response(&request, &self.program(call)))
            }
            DevProviderScenario::ReplayRouteChange => Ok(replay_route_response(&request)),
            #[cfg(feature = "provider-wire-fixtures")]
            DevProviderScenario::ValidEmptyCompletion => {
                unreachable!("valid-empty-completion uses the OpenAI-compatible adapter")
            }
        }
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

impl DevFailureProvider {
    #[expect(
        clippy::expect_used,
        reason = "each response-producing scenario scripts a program"
    )]
    fn program(&self, call: usize) -> String {
        self.scenario
            .scripted_cell(call)
            .expect("every response-producing branch scripts a program")
    }
}

/// The common shape: one cell that finishes with `value`.
fn finish_cell(value: &str) -> String {
    format!("await chat.reply({value});")
}

fn streamed_response(request: &LlmRequest, text: &str) -> LlmResponse {
    let part = native_program(text);
    if let Some(events) = &request.stream_events {
        events.send(LlmStreamEvent::Part(part.clone()));
    }
    LlmResponse {
        parts: vec![part],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

#[expect(
    clippy::expect_used,
    reason = "the ReplayRouteChange scenario scripts a cell for every replay-route turn index"
)]
fn replay_route_response(request: &LlmRequest) -> LlmResponse {
    let turn = next_replay_route_turn(&request.messages);
    // The table is the single source, so the scripted-cell walk covers this
    // scenario too.
    let text = DevProviderScenario::ReplayRouteChange
        .scripted_cell(turn)
        .expect("the replay-route scenario scripts every call");
    let reasoning = format!("FIG-1374 portable reasoning {turn}");
    send_reasoning(request, &reasoning);
    LlmResponse {
        parts: vec![
            LlmOutputPart::Reasoning {
                text: reasoning,
                replay: Some(ProviderReasoningReplay {
                    signature: Some(format!("FIG1374-OPAQUE-REPLAY-{turn}")),
                    origin: Some(ProviderRouteIdentity::new(
                        "workbench-dev-failure",
                        "workbench-dev-failure",
                        request.model.wire_model().to_string(),
                    )),
                    ..ProviderReasoningReplay::default()
                }),
            },
            native_program(&text),
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
        events.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            block: lash::direct::StreamBlockIdentity::new("text:0", 0),
            text: text.to_string(),
        }));
    }
}

fn send_reasoning(request: &LlmRequest, text: &str) {
    if let Some(events) = request.stream_events.as_ref() {
        events.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
            kind: StreamBlockKind::Reasoning,
            block: lash::direct::StreamBlockIdentity::new("reasoning:0", 0),
            text: text.to_string(),
        }));
    }
}

fn mcp_fixture_response(request: &LlmRequest) -> LlmResponse {
    if let Ok(path) = std::env::var("AGENT_WORKBENCH_MCP_PROVIDER_LOG") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            && let Ok(value) = serde_json::to_string(request)
        {
            let _ = writeln!(file, "{value}");
        }
    }
    // Sampling is a real nested provider request under its own host profile.
    if request.model.key().as_str() == "mcp-sampling" {
        return LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Host-generated summary.".to_owned(),
                response_meta: None,
            }],
            ..Default::default()
        };
    }
    let prompt_index = request.messages.iter().rposition(|message| message.role == LlmRole::User
        && message.blocks.iter().any(|block| matches!(block, LlmContentBlock::Text { text, .. } if text.contains("MCP-"))));
    let prompt = prompt_index
        .map(|index| {
            request.messages[index]
                .blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<Vec<&str>>()
                .join("\n")
        })
        .unwrap_or_default();
    if prompt_index.is_some_and(|index| {
        request.messages[index + 1..]
            .iter()
            .any(|message| message.role == LlmRole::Assistant)
    }) {
        return streamed_response(request, &finish_cell("\"peer interruption settled\""));
    }
    let body = if prompt.contains("MCP-DEPTH") {
        r#"const summary = await workspace_stdio.sampleSummary({text: "workbench"});
const form = await workspace_stdio.elicitConfirmation({});
const url = await workspace_stdio.elicitViaUrl({});
const roots = await workspace_stdio.listHostRoots({});
await chat.reply(summary.summary);"#
    } else if prompt.contains("MCP-DETACHED") {
        "await chat.reply(\"badge tool is detached\");"
    } else {
        "const badge = await workspace_http.workspaceBadge({}); await chat.reply(\"workspace badge came back\");"
    };
    streamed_response(request, body)
}

fn native_program(source: &str) -> LlmOutputPart {
    LlmOutputPart::ToolCall {
        call_id: "execute-code".to_owned(),
        tool_name: "execute_code".to_owned(),
        input_json: serde_json::json!({"code":source}).to_string(),
        replay: None,
    }
}

#[cfg(test)]
mod tests {
    use lash::provider::{LlmMessage, LlmRole};

    use super::next_replay_route_turn;

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
