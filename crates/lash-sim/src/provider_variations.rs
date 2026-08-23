//! Reusable Provider Wire Script variations shared by provider and runtime laws.
//!
//! The first matrix dimension records the response-boundary behavior that
//! originally escaped RLM coverage: a provider either consumes an emitted stop
//! sequence or returns the literal delimiter because no wire stop was sent.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash_core::ProviderFailureKind;
use lash_core::facade_support::LlmTransportError;
use lash_llm_transport::{LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_sansio::sync::MutexExt;
use serde_json::Value;

use crate::provider::{ProviderWireScript, ScriptedLlmHttpTransport};

pub const LASHLANG_CLOSE_DELIMITER: &str = "</lashlang>";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderStopDialect {
    OpenAiCompatibleChat,
    AnthropicMessages,
    GoogleGenerateContent,
}

impl ProviderStopDialect {
    pub const ALL: [Self; 3] = [
        Self::OpenAiCompatibleChat,
        Self::AnthropicMessages,
        Self::GoogleGenerateContent,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::OpenAiCompatibleChat => "openai-compatible.chat",
            Self::AnthropicMessages => "anthropic.messages",
            Self::GoogleGenerateContent => "google.generate-content",
        }
    }

    pub const fn provider_kind(self) -> &'static str {
        match self {
            Self::OpenAiCompatibleChat => crate::runtime_providers::OPENAI_COMPATIBLE,
            Self::AnthropicMessages => crate::runtime_providers::ANTHROPIC,
            Self::GoogleGenerateContent => crate::runtime_providers::GOOGLE_OAUTH,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderStopVariation {
    StopConsumed,
    LiteralPresent,
}

impl ProviderStopVariation {
    pub const ALL: [Self; 2] = [Self::StopConsumed, Self::LiteralPresent];

    pub const fn name(self) -> &'static str {
        match self {
            Self::StopConsumed => "stop-consumed",
            Self::LiteralPresent => "literal-present",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderNativeFinishReason {
    OpenAiStop,
    OpenAiStopSequence,
    AnthropicEndTurn,
    AnthropicStopSequence,
    GoogleStop,
}

impl ProviderNativeFinishReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiStop => "stop",
            Self::OpenAiStopSequence => "stop_sequence",
            Self::AnthropicEndTurn => "end_turn",
            Self::AnthropicStopSequence => "stop_sequence",
            Self::GoogleStop => "STOP",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ProviderStopFixture {
    pub dialect: ProviderStopDialect,
    pub variation: ProviderStopVariation,
    pub native_finish_reason: ProviderNativeFinishReason,
    pub path: &'static str,
    content: &'static str,
}

impl ProviderStopFixture {
    pub fn script(self) -> Result<ProviderWireScript, LlmTransportError> {
        ProviderWireScript::from_json_str(self.content)
    }
}

const OPENAI_CHAT_STOP_CONSUMED: &str =
    include_str!("../provider-scripts/variations/openai-compatible.chat-stop-consumed.json");
const OPENAI_CHAT_LITERAL_PRESENT: &str =
    include_str!("../provider-scripts/variations/openai-compatible.chat-literal-present.json");
const ANTHROPIC_STOP_CONSUMED: &str =
    include_str!("../provider-scripts/variations/anthropic.messages-stop-consumed.json");
const ANTHROPIC_LITERAL_PRESENT: &str =
    include_str!("../provider-scripts/variations/anthropic.messages-literal-present.json");
const GOOGLE_STOP_CONSUMED: &str =
    include_str!("../provider-scripts/variations/google.generate-content-stop-consumed.json");
const GOOGLE_LITERAL_PRESENT: &str =
    include_str!("../provider-scripts/variations/google.generate-content-literal-present.json");

pub const PROVIDER_STOP_VARIATION_MATRIX: &[ProviderStopFixture] = &[
    ProviderStopFixture {
        dialect: ProviderStopDialect::OpenAiCompatibleChat,
        variation: ProviderStopVariation::StopConsumed,
        native_finish_reason: ProviderNativeFinishReason::OpenAiStopSequence,
        path: "provider-scripts/variations/openai-compatible.chat-stop-consumed.json",
        content: OPENAI_CHAT_STOP_CONSUMED,
    },
    ProviderStopFixture {
        dialect: ProviderStopDialect::OpenAiCompatibleChat,
        variation: ProviderStopVariation::LiteralPresent,
        native_finish_reason: ProviderNativeFinishReason::OpenAiStop,
        path: "provider-scripts/variations/openai-compatible.chat-literal-present.json",
        content: OPENAI_CHAT_LITERAL_PRESENT,
    },
    ProviderStopFixture {
        dialect: ProviderStopDialect::AnthropicMessages,
        variation: ProviderStopVariation::StopConsumed,
        native_finish_reason: ProviderNativeFinishReason::AnthropicStopSequence,
        path: "provider-scripts/variations/anthropic.messages-stop-consumed.json",
        content: ANTHROPIC_STOP_CONSUMED,
    },
    ProviderStopFixture {
        dialect: ProviderStopDialect::AnthropicMessages,
        variation: ProviderStopVariation::LiteralPresent,
        native_finish_reason: ProviderNativeFinishReason::AnthropicEndTurn,
        path: "provider-scripts/variations/anthropic.messages-literal-present.json",
        content: ANTHROPIC_LITERAL_PRESENT,
    },
    ProviderStopFixture {
        dialect: ProviderStopDialect::GoogleGenerateContent,
        variation: ProviderStopVariation::StopConsumed,
        native_finish_reason: ProviderNativeFinishReason::GoogleStop,
        path: "provider-scripts/variations/google.generate-content-stop-consumed.json",
        content: GOOGLE_STOP_CONSUMED,
    },
    ProviderStopFixture {
        dialect: ProviderStopDialect::GoogleGenerateContent,
        variation: ProviderStopVariation::LiteralPresent,
        native_finish_reason: ProviderNativeFinishReason::GoogleStop,
        path: "provider-scripts/variations/google.generate-content-literal-present.json",
        content: GOOGLE_LITERAL_PRESENT,
    },
];

/// Migrated runtime providers that intentionally have no stop-variation rows.
///
/// OpenAI Responses does not serialize caller stop sequences, so it cannot
/// exhibit the provider-consumed delimiter behavior modeled by this corpus.
#[cfg(test)]
const PROVIDER_STOP_VARIATION_EXCLUSIONS: &[(&str, &str)] = &[(
    crate::runtime_providers::OPENAI,
    "OpenAI Responses emits no provider wire stop field",
)];

pub fn provider_stop_fixture(
    dialect: ProviderStopDialect,
    variation: ProviderStopVariation,
) -> ProviderStopFixture {
    *PROVIDER_STOP_VARIATION_MATRIX
        .iter()
        .find(|fixture| fixture.dialect == dialect && fixture.variation == variation)
        .expect("every shipped stop-capable dialect has every stop variation")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderStopSelection {
    pub call_index: usize,
    pub variation: ProviderStopVariation,
    pub carries_unclosed_cell_retry_prompt: bool,
}

/// Reusable stop-boundary transport which selects a fresh fixture from the
/// actual provider request on every call.
///
/// Unlike a queue of request-matched scripts, this transport can model an
/// unbounded protocol retry: every request with `</lashlang>` receives the
/// stop-consumed/no-literal response, while every request without a stop gets
/// the literal-present response.
#[derive(Clone, Debug)]
pub struct PairedProviderStopTransport {
    dialect: ProviderStopDialect,
    selections: Arc<Mutex<Vec<ProviderStopSelection>>>,
}

impl PairedProviderStopTransport {
    pub fn new(dialect: ProviderStopDialect) -> Self {
        Self {
            dialect,
            selections: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn call_count(&self) -> usize {
        self.selections.lock_recover().len()
    }

    pub fn selections(&self) -> Vec<ProviderStopSelection> {
        self.selections.lock_recover().clone()
    }

    fn variation_for_request(
        &self,
        request: &LlmHttpRequest,
    ) -> Result<ProviderStopVariation, LlmTransportError> {
        let body: Value = serde_json::from_slice(&request.body).map_err(|error| {
            LlmTransportError::new(format!(
                "paired provider-stop transport received invalid JSON: {error}"
            ))
            .with_kind(ProviderFailureKind::Validation)
        })?;
        let stop = match self.dialect {
            ProviderStopDialect::OpenAiCompatibleChat => body.get("stop"),
            ProviderStopDialect::AnthropicMessages => body.get("stop_sequences"),
            ProviderStopDialect::GoogleGenerateContent => body
                .get("request")
                .and_then(|request| request.get("generationConfig"))
                .and_then(|generation| generation.get("stopSequences")),
        };
        match stop {
            None => Ok(ProviderStopVariation::LiteralPresent),
            Some(Value::Array(stops))
                if stops.len() == 1
                    && stops.first().and_then(Value::as_str) == Some(LASHLANG_CLOSE_DELIMITER) =>
            {
                Ok(ProviderStopVariation::StopConsumed)
            }
            Some(other) => Err(LlmTransportError::new(format!(
                "paired provider-stop transport expected no stop or exactly [\"{LASHLANG_CLOSE_DELIMITER}\"], got {other}"
            ))
            .with_kind(ProviderFailureKind::Validation)),
        }
    }
}

#[async_trait]
impl LlmHttpTransport for PairedProviderStopTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        timeout: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        let variation = self.variation_for_request(&request)?;
        let body_text = String::from_utf8_lossy(&request.body);
        {
            let mut selections = self.selections.lock_recover();
            let call_index = selections.len();
            selections.push(ProviderStopSelection {
                call_index,
                variation,
                carries_unclosed_cell_retry_prompt: body_text.contains(
                    "Reply again using exactly one paired `<lashlang>...</lashlang>` block",
                ),
            });
        }

        let fixture = provider_stop_fixture(self.dialect, variation);
        ScriptedLlmHttpTransport::new(fixture.script()?)?
            .send(request, timeout)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use lash_core::facade_support::TurnFinish;
    use lash_core::llm::types::{
        LlmEventSender, LlmMessage, LlmProviderTraceEvent, LlmProviderTraceSender, LlmRequest,
        LlmRole, LlmTerminalReason, LlmToolChoice,
    };
    use lash_core::{GenerationOptionOutcome, GenerationOptions};
    use serde_json::json;

    use super::*;
    use crate::runtime_providers::runtime_provider_components;

    #[test]
    fn provider_stop_variation_matrix_has_both_rows_for_every_stop_capable_dialect() {
        assert_eq!(
            PROVIDER_STOP_VARIATION_MATRIX.len(),
            ProviderStopDialect::ALL.len() * ProviderStopVariation::ALL.len()
        );
        let rows = PROVIDER_STOP_VARIATION_MATRIX
            .iter()
            .map(|fixture| (fixture.dialect.name(), fixture.variation.name()))
            .collect::<BTreeSet<_>>();
        for dialect in ProviderStopDialect::ALL {
            for variation in ProviderStopVariation::ALL {
                assert!(
                    rows.contains(&(dialect.name(), variation.name())),
                    "missing {} {} fixture",
                    dialect.name(),
                    variation.name()
                );
            }
        }
    }

    #[test]
    fn every_migrated_runtime_provider_is_covered_or_explicitly_excluded() {
        let covered = ProviderStopDialect::ALL
            .into_iter()
            .map(ProviderStopDialect::provider_kind)
            .collect::<BTreeSet<_>>();
        let excluded = PROVIDER_STOP_VARIATION_EXCLUSIONS
            .iter()
            .map(|(provider_kind, reason)| {
                assert!(!reason.is_empty(), "provider exclusion requires a reason");
                *provider_kind
            })
            .collect::<BTreeSet<_>>();
        assert!(covered.is_disjoint(&excluded));
        assert_eq!(
            covered.union(&excluded).copied().collect::<BTreeSet<_>>(),
            crate::runtime_providers::MIGRATED_RUNTIME_PROVIDER_KINDS
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn provider_stop_variation_files_on_disk_exactly_match_the_public_registry() {
        let fixture_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("provider-scripts/variations");
        let files_on_disk = std::fs::read_dir(&fixture_dir)
            .expect("provider stop-variation fixture directory")
            .map(|entry| {
                entry
                    .expect("provider stop-variation directory entry")
                    .file_name()
            })
            .filter(|name| {
                std::path::Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension == "json")
            })
            .map(|name| {
                name.into_string()
                    .expect("provider stop-variation fixture filename must be UTF-8")
            })
            .collect::<BTreeSet<_>>();
        let listed_files = PROVIDER_STOP_VARIATION_MATRIX
            .iter()
            .map(|fixture| {
                std::path::Path::new(fixture.path)
                    .file_name()
                    .expect("registered fixture path has a filename")
                    .to_str()
                    .expect("registered fixture filename must be UTF-8")
                    .to_string()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            files_on_disk, listed_files,
            "provider stop-variation fixtures on disk must exactly match PROVIDER_STOP_VARIATION_MATRIX"
        );
    }

    #[tokio::test]
    async fn provider_stop_variation_matrix_uses_real_request_and_response_dialects() {
        for dialect in ProviderStopDialect::ALL {
            for variation in ProviderStopVariation::ALL {
                let fixture = provider_stop_fixture(dialect, variation);
                let transport = Arc::new(PairedProviderStopTransport::new(dialect));
                let (mut provider, model, _) =
                    runtime_provider_components(dialect.provider_kind(), &transport)
                        .expect("runtime provider components");
                let trace_events = Arc::new(Mutex::new(Vec::<LlmProviderTraceEvent>::new()));
                let trace_sink = Arc::clone(&trace_events);
                let response = provider
                    .complete(provider_request(
                        &model.id,
                        variation,
                        LlmProviderTraceSender::new(move |event| {
                            trace_sink.lock_recover().push(event);
                        }),
                    ))
                    .await
                    .unwrap_or_else(|error| {
                        panic!(
                            "{} {} fixture failed: {error}",
                            dialect.name(),
                            variation.name()
                        )
                    });

                assert_eq!(response.terminal_reason, LlmTerminalReason::Stop);
                let parsed_native_finish_reason = response
                    .execution_evidence
                    .as_ref()
                    .and_then(|evidence| evidence.provider_finish_reason.as_deref())
                    .map(str::to_string)
                    .or_else(|| {
                        (dialect == ProviderStopDialect::GoogleGenerateContent).then(|| {
                            google_native_finish_reason_from_adapter_trace(
                                &trace_events.lock_recover(),
                            )
                            .expect("Google adapter trace carries its parsed terminal event")
                        })
                    });
                assert_eq!(
                    parsed_native_finish_reason.as_deref(),
                    Some(fixture.native_finish_reason.as_str()),
                    "{} {} parsed the wrong provider-native finish reason",
                    dialect.name(),
                    variation.name()
                );
                assert_eq!(
                    response.full_text.contains(LASHLANG_CLOSE_DELIMITER),
                    matches!(variation, ProviderStopVariation::LiteralPresent),
                    "{} {} returned the wrong delimiter shape",
                    dialect.name(),
                    variation.name()
                );
                assert_eq!(
                    transport.selections(),
                    vec![ProviderStopSelection {
                        call_index: 0,
                        variation,
                        carries_unclosed_cell_retry_prompt: false,
                    }]
                );
            }
        }
    }

    #[tokio::test]
    async fn rlm_stop_honoring_fixture_settles_once_without_wire_stop_or_unclosed_retry() {
        let fixture = provider_stop_fixture(
            ProviderStopDialect::AnthropicMessages,
            ProviderStopVariation::LiteralPresent,
        );
        let transport = Arc::new(PairedProviderStopTransport::new(fixture.dialect));
        let (provider, model, _) =
            runtime_provider_components(fixture.dialect.provider_kind(), &transport)
                .expect("Anthropic provider components");
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
        );
        let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
            .generation(GenerationOptions {
                stop_sequences: vec![LASHLANG_CLOSE_DELIMITER.to_string()],
                ..GenerationOptions::default()
            })
            .effect_host(Arc::new(
                lash::durability::InlineEffectHost::default()
                    .allow_process_lifetime_completion_keys(),
            ))
            .lease_timings(crate::lease::sim_runtime_lease_timings())
            .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .process_env_store(Arc::new(
                lash::persistence::InMemoryProcessExecutionEnvStore::new(),
            ))
            .store_factory(Arc::new(
                lash::persistence::InMemorySessionStoreFactory::new(),
            ))
            .process_registry(Arc::new(lash_core::TestLocalProcessRegistry::default())
                as Arc<dyn lash_core::ProcessRegistry>)
            .provider(provider)
            .model(model)
            .build(crate::sim_process_owner())
            .expect("RLM core");
        let session = core
            .session("rlm-stop-honoring-boundary")
            .open()
            .await
            .expect("RLM session");
        let run = tokio::time::timeout(
            Duration::from_secs(2),
            session
                .turn(lash::TurnInput::text("finish with the scripted value"))
                .run(),
        )
        .await;
        let turn = match run {
            Ok(turn) => turn.expect("RLM turn"),
            Err(_) => {
                let selections = transport.selections();
                let stop_consumed_calls = selections
                    .iter()
                    .filter(|selection| selection.variation == ProviderStopVariation::StopConsumed)
                    .count();
                let literal_present_calls = selections.len() - stop_consumed_calls;
                let retry_prompt_calls = selections
                    .iter()
                    .filter(|selection| selection.carries_unclosed_cell_retry_prompt)
                    .count();
                panic!(
                    "RLM turn exceeded 2s: calls={}, stop_consumed_calls={stop_consumed_calls}, literal_present_calls={literal_present_calls}, retry_unclosed_cell_prompt_calls={retry_prompt_calls}",
                    selections.len()
                );
            }
        };

        assert_eq!(
            turn.result.outcome,
            lash::TurnOutcome::Finished(TurnFinish::FinalValue {
                value: json!("settled")
            })
        );
        assert_eq!(turn.result.state.turn_index, 1);
        assert_eq!(transport.call_count(), 1);
        assert_eq!(
            transport.selections(),
            vec![ProviderStopSelection {
                call_index: 0,
                variation: ProviderStopVariation::LiteralPresent,
                carries_unclosed_cell_retry_prompt: false,
            }]
        );

        let attempt = turn
            .result
            .llm_calls
            .first()
            .and_then(|call| call.attempts.first())
            .expect("one provider attempt");
        assert_eq!(turn.result.llm_calls.len(), 1);
        assert_eq!(turn.result.llm_calls[0].attempts.len(), 1);
        assert_eq!(
            attempt
                .generation_disposition
                .expect("generation disposition")
                .stop_sequences,
            GenerationOptionOutcome::SuppressedProtocolOwned
        );

        let read_view = turn.result.state.read_view();
        assert_eq!(
            read_view.messages().len(),
            1,
            "a final-value turn settles exactly one user transcript message"
        );
        let extraction_decisions = read_view
            .active_events()
            .iter()
            .filter_map(|event| match event {
                lash_core::SessionHistoryRecord::Protocol(event) => {
                    match lash_protocol_rlm::decode_rlm_protocol_event(event) {
                        Some(lash_rlm_types::RlmProtocolEvent::RlmDiagnostic(diagnostic))
                            if diagnostic.phase == "llm_extraction" =>
                        {
                            diagnostic
                                .payload
                                .get("decision")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                        }
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(extraction_decisions, vec!["execute_lashlang"]);
        assert_eq!(
            extraction_decisions
                .iter()
                .filter(|decision| decision.as_str() == "retry_unclosed_cell")
                .count(),
            0
        );
    }

    fn google_native_finish_reason_from_adapter_trace(
        events: &[LlmProviderTraceEvent],
    ) -> Option<String> {
        events
            .iter()
            .filter(|event| event.provider == "google" && event.request_endpoint().is_none())
            .filter_map(|event| serde_json::from_str::<Value>(&event.raw).ok())
            .filter_map(|event| {
                event
                    .get("response")
                    .and_then(|response| response.get("candidates"))
                    .and_then(Value::as_array)
                    .and_then(|candidates| candidates.first())
                    .and_then(|candidate| candidate.get("finishReason"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .next_back()
    }

    fn provider_request(
        model: &str,
        variation: ProviderStopVariation,
        provider_trace: LlmProviderTraceSender,
    ) -> LlmRequest {
        LlmRequest {
            model: model.to_string(),
            messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
            attachments: Vec::new(),
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::new()),
            tool_choice: LlmToolChoice::None,
            model_variant: Default::default(),
            model_capability: Default::default(),
            generation: GenerationOptions {
                stop_sequences: (variation == ProviderStopVariation::StopConsumed)
                    .then(|| LASHLANG_CLOSE_DELIMITER.to_string())
                    .into_iter()
                    .collect(),
                ..GenerationOptions::default()
            },
            scope: lash_core::LlmRequestScope::new(
                "session-1",
                "session-1:frame:provider-stop-matrix",
                "session-1:request:provider-stop-matrix",
            ),
            output_spec: None,
            stream_events: Some(LlmEventSender::new(|_event| {})),
            provider_trace: Some(provider_trace),
        }
    }
}
