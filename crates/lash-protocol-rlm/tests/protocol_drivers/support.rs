pub(crate) use std::sync::Arc;

pub(crate) use lash_core::facade_support::PluginHost;
pub(crate) use lash_core::plugin::{
    AssistantStreamFinishReason, AssistantStreamTransform, PluginFactory, PluginSession,
};
pub(crate) use lash_core::sansio::{self, ChatContextProjector, ProtocolDriverHandle, Response};
pub(crate) use lash_core::testing::behavior_transcript::{Actor, Entry, Kind, Transcript};
pub(crate) use lash_core::testing::sansio_transcript::record_effects;
pub(crate) use lash_core::{Effect, TurnMachine, TurnMachineConfig};
pub(crate) use lash_protocol_rlm::{RlmDriver, RlmProtocolPluginConfig, RlmProtocolPluginFactory};

/// Actor name pinned on every RLM Protocol Scenario transcript. The harness drives
/// one sans-io machine for one session.
pub(crate) const RLM_TRANSCRIPT_ACTOR: &str = "rlm";
pub(crate) use lash_rlm_types::{
    RlmCreateExtras, RlmProtocolEvent, RlmTermination, RlmTrajectoryEntry,
};
pub(crate) use lash_sansio::llm::types::{
    LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse, LlmRole,
};
pub(crate) use lash_sansio::{
    CheckpointKind, Message, MessageRole, Part, PartKind, SessionStreamEvent,
};

pub(crate) fn test_config() -> TurnMachineConfig {
    test_config_with_termination(RlmTermination::default())
}

pub(crate) fn test_config_with_termination(rlm_termination: RlmTermination) -> TurnMachineConfig {
    test_config_with_protocol_turn_options(
        lash_core::ProtocolTurnOptions::typed(RlmCreateExtras {
            dialect: None,
            termination: Some(rlm_termination),
            final_answer_format: None,
        })
        .expect("valid rlm turn options"),
    )
}

/// A config whose protocol driver runs `language`, for assertions that must
/// hold in a session that is not the default dialect.
pub(crate) fn test_config_with_dialect(language: &str) -> TurnMachineConfig {
    let protocol_driver: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>> =
        Arc::new(RlmDriver::for_language(language));
    TurnMachineConfig {
        protocol_driver,
        ..test_config()
    }
}

pub(crate) fn test_config_with_protocol_turn_options(
    termination: lash_core::ProtocolTurnOptions,
) -> TurnMachineConfig {
    let protocol_driver: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>> =
        Arc::new(RlmDriver::default());
    TurnMachineConfig {
        protocol_driver,
        projector: Arc::new(ChatContextProjector),
        sync_execution_environment: true,
        model: "test-model".to_string(),
        max_context_tokens: None,
        turn_budget: lash_core::TurnBudget::Unbounded,
        no_progress_budget: Default::default(),
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        autonomous: false,
        tool_specs: Vec::new().into(),
        system_prompt: std::sync::Arc::from(""),
        session_id: "test".to_string(),
        turn_id: "test-turn".to_string(),
        emit_llm_trace: false,
        termination,
        turn_limit_final_message: Arc::new(test_turn_limit_final_message),
    }
}

pub(crate) fn test_turn_limit_final_message(message_id: String, max_turns: usize) -> Message {
    Message {
        id: message_id.clone(),
        role: MessageRole::System,
        parts: lash_sansio::shared_parts(vec![Part::error(
            format!("{message_id}.p0"),
            format!("Turn limit reached ({max_turns}) before a final test response."),
        )]),
        origin: None,
    }
}

pub(crate) fn user_message(content: &str) -> Message {
    Message {
        id: "m0".to_string(),
        role: MessageRole::User,
        parts: vec![Part::text("m0.p0".to_string(), content.to_string(), None)].into(),
        origin: None,
    }
}

pub(crate) fn drain_effects(machine: &mut TurnMachine) -> Vec<Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
        if let Effect::SyncExecutionEnvironment { id, .. } = effect {
            effects.push(effect);
            machine.handle_response(Response::ExecutionEnvironmentSynced {
                id,
                result: Ok(Some(sansio::ExecutionEnvironmentSync {
                    system_prompt: std::sync::Arc::from(""),
                    tool_specs: Arc::new(Vec::new()),
                })),
            });
            continue;
        }
        effects.push(effect);
    }
    effects
}

pub(crate) fn find_llm_call(effects: &[Effect]) -> Option<&sansio::EffectId> {
    effects.iter().find_map(|e| match e {
        Effect::LlmCall { id, .. } => Some(id),
        _ => None,
    })
}

pub(crate) fn find_llm_request(effects: &[Effect]) -> Option<&LlmRequest> {
    effects
        .iter()
        .find_map(|e| match e {
            Effect::LlmCall { request, .. } => Some(request),
            _ => None,
        })
        .map(|request| request.as_ref())
}

pub(crate) fn find_checkpoint(effects: &[Effect]) -> Option<(sansio::EffectId, CheckpointKind)> {
    effects.iter().find_map(|e| match e {
        Effect::Checkpoint { id, checkpoint } => Some((*id, *checkpoint)),
        _ => None,
    })
}

pub(crate) fn find_done(effects: &[Effect]) -> Option<(&lash_sansio::MessageSequence, usize)> {
    effects.iter().find_map(|e| match e {
        Effect::Done {
            messages,
            event_delta: _,
            protocol_iteration,
        } => Some((messages, *protocol_iteration)),
        _ => None,
    })
}

pub(crate) fn roundtrip_turn_checkpoint(
    checkpoint: lash_sansio::TurnCheckpoint<lash_core::HostTurnProtocol>,
) -> lash_sansio::TurnCheckpoint<lash_core::HostTurnProtocol> {
    let encoded = serde_json::to_string(&checkpoint).expect("serialize checkpoint");
    serde_json::from_str(&encoded).expect("deserialize checkpoint")
}

pub(crate) fn machine_trajectory(machine: &TurnMachine) -> Vec<RlmTrajectoryEntry> {
    machine
        .events()
        .iter()
        .filter_map(|event| match event {
            lash_core::SessionHistoryRecord::Protocol(event) => {
                match lash_protocol_rlm::decode_rlm_protocol_event(event) {
                    Some(RlmProtocolEvent::RlmTrajectoryEntry(entry)) => Some(entry),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

pub(crate) fn single_llm_extraction_payload(machine: &TurnMachine) -> serde_json::Value {
    let payloads: Vec<_> = machine
        .events()
        .iter()
        .filter_map(|event| match event {
            lash_core::SessionHistoryRecord::Protocol(event) => {
                match lash_protocol_rlm::decode_rlm_protocol_event(event) {
                    Some(RlmProtocolEvent::RlmDiagnostic(diagnostic)) => {
                        (diagnostic.phase == "llm_extraction").then_some(diagnostic.payload)
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .collect();
    assert_eq!(payloads.len(), 1, "expected one llm_extraction diagnostic");
    let mut payload = payloads.into_iter().next().expect("payload");
    assert_no_legacy_llm_extraction_keys(&payload);
    // The reply fingerprint is a digest of the model's own reply. Asserted by
    // shape above and dropped here: pinning a hash in every scenario fixture
    // would cost the readability of the reply text without adding a check.
    payload
        .as_object_mut()
        .expect("diagnostic payload object")
        .remove("reply_fingerprint");
    payload
}

pub(crate) fn assert_no_legacy_llm_extraction_keys(payload: &serde_json::Value) {
    let object = payload.as_object().expect("diagnostic payload object");
    assert_eq!(
        object.len(),
        5,
        "llm_extraction payload should only contain turn_id, reply_fingerprint, decision, termination, and counts"
    );
    // The reply fingerprint is host-visible evidence of a repeated reply, with
    // no runtime behavior attached, so every attempt must carry one for a host
    // to be able to compare attempts at all.
    let fingerprint = object
        .get("reply_fingerprint")
        .and_then(serde_json::Value::as_str)
        .expect("every extraction diagnostic fingerprints its reply");
    assert_eq!(fingerprint.len(), 16, "fingerprint: {fingerprint}");
    assert!(
        fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
        "fingerprint: {fingerprint}"
    );
    // The turn id is what scopes the no-progress count across a session whose
    // active path carries every earlier turn's diagnostics.
    assert_eq!(
        object.get("turn_id").and_then(serde_json::Value::as_str),
        Some("test-turn"),
        "every extraction diagnostic must name its own turn"
    );
    for key in [
        "assistant_text_chars",
        "prose_only_ends_turn",
        "finalization_reason",
    ] {
        assert!(
            object.get(key).is_none(),
            "legacy llm_extraction key `{key}` should not be emitted"
        );
    }
}

pub(crate) fn assistant_messages(machine: &TurnMachine) -> Vec<Message> {
    machine
        .events()
        .iter()
        .filter_map(|event| match event {
            lash_core::SessionHistoryRecord::Conversation(record) => {
                let message = record.to_message();
                (message.role == MessageRole::Assistant).then_some(message)
            }
            _ => None,
        })
        .collect()
}

pub(crate) fn assistant_reasoning_texts(machine: &TurnMachine) -> Vec<String> {
    let mut texts = assistant_messages(machine)
        .into_iter()
        .flat_map(|message| {
            message
                .parts
                .iter()
                .filter(|part| matches!(part.kind, PartKind::Reasoning))
                .map(|part| part.content.clone())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    texts.extend(machine.events().iter().filter_map(|event| {
        let lash_core::SessionHistoryRecord::Protocol(event) = event else {
            return None;
        };
        match lash_protocol_rlm::decode_rlm_protocol_event(event) {
            Some(RlmProtocolEvent::RlmAssistantContent(content))
                if !content.reasoning.is_empty() =>
            {
                Some(content.reasoning)
            }
            _ => None,
        }
    }));
    texts
}

pub(crate) fn assistant_visible_texts(machine: &TurnMachine) -> Vec<String> {
    let mut texts = assistant_messages(machine)
        .into_iter()
        .flat_map(|message| {
            message
                .parts
                .iter()
                .filter(|part| matches!(part.kind, PartKind::Text | PartKind::Prose))
                .map(|part| part.content.clone())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    texts.extend(machine.events().iter().filter_map(|event| {
        let lash_core::SessionHistoryRecord::Protocol(event) = event else {
            return None;
        };
        match lash_protocol_rlm::decode_rlm_protocol_event(event) {
            Some(RlmProtocolEvent::RlmAssistantContent(content)) if !content.prose.is_empty() => {
                Some(content.prose)
            }
            _ => None,
        }
    }));
    texts
}

pub(crate) fn text_part(text: &str) -> LlmOutputPart {
    LlmOutputPart::Text {
        text: text.to_string(),
        response_meta: None,
    }
}

pub(crate) fn reasoning_part(text: &str) -> LlmOutputPart {
    LlmOutputPart::Reasoning {
        text: text.to_string(),
        replay: None,
    }
}

pub(crate) fn lashlang_block(code: &str) -> String {
    format!("<lashlang>\n{code}\n</lashlang>")
}

pub(crate) fn lashlang_block_with_prose(prose: &str, code: &str) -> String {
    format!("{prose}\n<lashlang>\n{code}\n</lashlang>")
}

pub(crate) fn rlm_response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    let full_text = parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    LlmResponse {
        full_text,
        parts,
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

pub(crate) fn exec_response(
    output: &[&str],
    error: Option<&str>,
    final_output: Option<serde_json::Value>,
) -> lash_sansio::ExecResponse {
    lash_sansio::ExecResponse {
        observations: output.iter().map(|item| (*item).to_string()).collect(),
        observation_truncation: Vec::new(),
        tool_calls: Vec::new(),
        executed_calls: Vec::new(),
        printed_images: Vec::new(),
        error: error.map(str::to_string),
        duration_ms: 1,
        terminal_finish: final_output,
    }
}

pub(crate) fn effects_include_runtime_error(effects: &[Effect], message_fragment: &str) -> bool {
    let has_error = effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::Emit(SessionStreamEvent::Error { message, .. })
                if message.contains(message_fragment)
        )
    });
    let has_runtime_outcome = effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::Emit(SessionStreamEvent::TurnOutcome {
                outcome: lash_sansio::TurnOutcome::Stopped(lash_sansio::TurnStop::RuntimeError)
            })
        )
    });
    has_error && has_runtime_outcome
}

pub(crate) fn rewrite_first_rlm_driver_state_owner(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("plugin_id").and_then(serde_json::Value::as_str)
                == Some(lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID)
            {
                map.insert(
                    "plugin_id".to_string(),
                    serde_json::Value::String("other_protocol".to_string()),
                );
                return true;
            }
            map.values_mut().any(rewrite_first_rlm_driver_state_owner)
        }
        serde_json::Value::Array(values) => {
            values.iter_mut().any(rewrite_first_rlm_driver_state_owner)
        }
        _ => false,
    }
}

// === RLM Protocol Scenario Harness ===
//
// These scenarios drive the protocol state machine through declarative LLM,
// exec, and checkpoint steps. Direct white-box tests remain below only when
// they intentionally corrupt turn options or checkpoint driver state.
pub(crate) struct RlmProtocolScenario {
    pub(crate) name: &'static str,
    pub(crate) user_message: &'static str,
    pub(crate) termination: RlmTermination,
    pub(crate) protocol_turn_options: Option<lash_core::ProtocolTurnOptions>,
    pub(crate) max_turns: Option<usize>,
    pub(crate) plugin_factories: Vec<Arc<dyn PluginFactory>>,
    pub(crate) steps: Vec<RlmProtocolStep>,
    pub(crate) expectations: RlmProtocolExpectations,
}

impl RlmProtocolScenario {
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            name,
            user_message: "perform one step",
            termination: RlmTermination::default(),
            protocol_turn_options: None,
            max_turns: None,
            plugin_factories: Vec::new(),
            steps: Vec::new(),
            expectations: RlmProtocolExpectations::default(),
        }
    }

    pub(crate) fn user_message(mut self, user_message: &'static str) -> Self {
        self.user_message = user_message;
        self
    }

    pub(crate) fn termination(mut self, termination: RlmTermination) -> Self {
        self.termination = termination;
        self
    }

    pub(crate) fn protocol_turn_options(mut self, options: lash_core::ProtocolTurnOptions) -> Self {
        self.protocol_turn_options = Some(options);
        self
    }

    pub(crate) fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    pub(crate) fn plugin_factory(mut self, factory: Arc<dyn PluginFactory>) -> Self {
        self.plugin_factories.push(factory);
        self
    }

    pub(crate) fn llm_response(mut self, parts: Vec<LlmOutputPart>) -> Self {
        self.steps.push(RlmProtocolStep::LlmResponse {
            text_streamed: false,
            parts,
        });
        self
    }

    pub(crate) fn streamed_llm_response(mut self, parts: Vec<LlmOutputPart>) -> Self {
        self.steps.push(RlmProtocolStep::LlmResponse {
            text_streamed: true,
            parts,
        });
        self
    }

    pub(crate) fn plugin_streamed_llm_response(
        mut self,
        chunks: Vec<&'static str>,
        parts: Vec<LlmOutputPart>,
    ) -> Self {
        self.steps.push(RlmProtocolStep::PluginStreamedLlmResponse {
            chunks: chunks.into_iter().map(str::to_string).collect(),
            parts,
        });
        self
    }

    pub(crate) fn exec_result(mut self, result: lash_sansio::ExecResponse) -> Self {
        self.steps.push(RlmProtocolStep::ExecResult(result));
        self
    }

    pub(crate) fn checkpoint(mut self) -> Self {
        self.steps.push(RlmProtocolStep::Checkpoint);
        self
    }

    /// Cross a real durable cell boundary: serialize the machine's
    /// `TurnCheckpoint`, deserialize it, and continue on the restored machine.
    ///
    /// This is the only step that replaces the machine under test, so anything
    /// the driver failed to carry in its checkpointed state is lost for real
    /// rather than by simulation.
    pub(crate) fn checkpoint_round_trip(mut self) -> Self {
        self.steps.push(RlmProtocolStep::CheckpointRoundTrip);
        self
    }

    pub(crate) fn expect(mut self, expectations: RlmProtocolExpectations) -> Self {
        self.expectations = expectations;
        self
    }

    pub(crate) fn run(self) -> RlmProtocolRun {
        let build_config = || {
            let mut config = if let Some(options) = self.protocol_turn_options.clone() {
                test_config_with_protocol_turn_options(options)
            } else {
                test_config_with_termination(self.termination.clone())
            };
            config.turn_budget = self
                .max_turns
                .map(lash_core::TurnBudget::bounded)
                .unwrap_or(lash_core::TurnBudget::Unbounded);
            config
        };
        let config = build_config();
        let plugin_session = if self.plugin_factories.is_empty() {
            None
        } else {
            Some(
                PluginHost::new(self.plugin_factories.clone())
                    .build_session("rlm-protocol-scenario-hooks", None)
                    .unwrap_or_else(|err| {
                        panic!("{} failed to register plugin hooks: {err}", self.name)
                    }),
            )
        };
        let mut machine = TurnMachine::new(
            config,
            vec![user_message(self.user_message)],
            Arc::new(Vec::new()),
            0,
        );
        let mut observed = RlmProtocolRun::default();
        observed
            .transcript
            .pin(RLM_TRANSCRIPT_ACTOR, RLM_TRANSCRIPT_ACTOR);
        let mut effects = drain_effects(&mut machine);
        observed.record(&effects);
        record_effects(&mut observed.transcript, RLM_TRANSCRIPT_ACTOR, &effects);
        observed.initial_request = find_llm_request(&effects).cloned();

        for step in &self.steps {
            match step {
                RlmProtocolStep::LlmResponse {
                    text_streamed,
                    parts,
                } => {
                    let llm_id = *find_llm_call(&effects)
                        .unwrap_or_else(|| panic!("{} expected pending LLM call", self.name));
                    machine.handle_response(Response::LlmComplete {
                        id: llm_id,
                        text_streamed: *text_streamed,
                        result: Ok(rlm_response(parts.clone())),
                    });
                }
                RlmProtocolStep::PluginStreamedLlmResponse { chunks, parts } => {
                    let llm_id = *find_llm_call(&effects)
                        .unwrap_or_else(|| panic!("{} expected pending LLM call", self.name));
                    let plugins = plugin_session.as_ref().unwrap_or_else(|| {
                        panic!(
                            "{} declared a plugin-streamed response without a plugin factory",
                            self.name
                        )
                    });
                    let transformed =
                        drive_plugin_stream(plugins, chunks, rlm_response(parts.clone()));
                    observed
                        .plugin_stream_visible_texts
                        .push(transformed.visible_text);
                    observed
                        .plugin_spliced_response_texts
                        .push(transformed.response.full_text.clone());
                    observed
                        .plugin_stream_abort_requests
                        .push(transformed.abort_requested);
                    machine.handle_response(Response::LlmComplete {
                        id: llm_id,
                        text_streamed: true,
                        result: Ok(transformed.response),
                    });
                }
                RlmProtocolStep::ExecResult(result) => {
                    let exec_id = effects
                        .iter()
                        .find_map(|effect| match effect {
                            Effect::ExecCode { id, .. } => Some(*id),
                            _ => None,
                        })
                        .unwrap_or_else(|| panic!("{} expected pending exec code", self.name));
                    machine.handle_response(Response::ExecResult {
                        id: exec_id,
                        result: Ok(result.clone()),
                    });
                }
                RlmProtocolStep::Checkpoint => {
                    let (checkpoint_id, _) = find_checkpoint(&effects)
                        .unwrap_or_else(|| panic!("{} expected checkpoint", self.name));
                    machine.handle_response(Response::Checkpoint {
                        id: checkpoint_id,
                        delivery: sansio::CheckpointDelivery::default(),
                    });
                }
                RlmProtocolStep::CheckpointRoundTrip => {
                    observed.transcript.record(Entry::new(
                        Kind::Park,
                        Actor::session(RLM_TRANSCRIPT_ACTOR),
                        "cell.checkpoint",
                    ));
                    let checkpoint = roundtrip_turn_checkpoint(machine.checkpoint());
                    machine = TurnMachine::restore_from_checkpoint(build_config(), checkpoint);
                    observed.round_trips += 1;
                    observed.transcript.record(Entry::new(
                        Kind::Resume,
                        Actor::session(RLM_TRANSCRIPT_ACTOR),
                        "cell.restore",
                    ));
                }
            }

            effects = drain_effects(&mut machine);
            observed.record(&effects);
            record_effects(&mut observed.transcript, RLM_TRANSCRIPT_ACTOR, &effects);
        }

        self.expectations.assert(self.name, &observed, &machine);
        observed
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RlmProtocolStep {
    LlmResponse {
        text_streamed: bool,
        parts: Vec<LlmOutputPart>,
    },
    PluginStreamedLlmResponse {
        chunks: Vec<String>,
        parts: Vec<LlmOutputPart>,
    },
    ExecResult(lash_sansio::ExecResponse),
    Checkpoint,
    CheckpointRoundTrip,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RlmProtocolExpectations {
    pub(crate) initial_request_tools_empty: bool,
    pub(crate) exec_codes: Vec<&'static str>,
    pub(crate) checkpoints: Vec<CheckpointKind>,
    pub(crate) llm_call_count: Option<usize>,
    pub(crate) done: Option<bool>,
    pub(crate) no_exec_code: bool,
    pub(crate) no_final_message_event: bool,
    pub(crate) no_tool_call_events: bool,
    pub(crate) tool_call_events: bool,
    pub(crate) no_assistant_conversation_progress: bool,
    pub(crate) trajectory_omits_tool_call_ids: bool,
    pub(crate) system_message_contains: Vec<&'static str>,
    pub(crate) system_message_omits: Vec<&'static str>,
    pub(crate) assistant_reasoning_texts: Option<Vec<&'static str>>,
    pub(crate) assistant_visible_texts: Option<Vec<&'static str>>,
    pub(crate) assistant_message_count: Option<usize>,
    pub(crate) plugin_stream_visible_texts: Option<Vec<&'static str>>,
    pub(crate) plugin_spliced_response_texts: Option<Vec<&'static str>>,
    pub(crate) plugin_stream_abort_requests: Option<Vec<bool>>,
    pub(crate) llm_extraction_payload: Option<serde_json::Value>,
    pub(crate) turn_outcome: Option<lash_sansio::TurnOutcome>,
    pub(crate) agent_frame_switch: Option<(&'static str, &'static str)>,
    pub(crate) tool_error_message: Option<(&'static str, &'static str)>,
    pub(crate) trajectory_last: Option<RlmTrajectoryExpectation>,
}

impl RlmProtocolExpectations {
    pub(crate) fn assert(&self, scenario_name: &str, run: &RlmProtocolRun, machine: &TurnMachine) {
        if self.initial_request_tools_empty {
            let request = run
                .initial_request
                .as_ref()
                .unwrap_or_else(|| panic!("{scenario_name} did not project an LLM request"));
            assert!(
                request.tools.is_empty(),
                "{scenario_name} RLM projection should not advertise native tools"
            );
        }
        assert_eq!(
            run.exec_codes,
            self.exec_codes
                .iter()
                .map(|code| (*code).to_string())
                .collect::<Vec<_>>(),
            "{scenario_name} exec-code sequence changed"
        );
        assert_eq!(
            run.checkpoints, self.checkpoints,
            "{scenario_name} checkpoint sequence changed"
        );
        if let Some(llm_call_count) = self.llm_call_count {
            assert_eq!(
                run.llm_call_count, llm_call_count,
                "{scenario_name} LLM call count changed"
            );
        }
        if let Some(done) = self.done {
            assert_eq!(
                machine.is_done(),
                done,
                "{scenario_name} done state changed"
            );
        }
        if self.no_exec_code {
            assert!(
                run.exec_codes.is_empty(),
                "{scenario_name} unexpectedly executed code: {:?}",
                run.exec_codes
            );
        }
        if self.no_final_message_event {
            assert!(
                !run.final_message_event,
                "{scenario_name} emitted duplicate protocol final message"
            );
        }
        if self.no_tool_call_events {
            assert!(
                !run.tool_call_event,
                "{scenario_name} emitted host tool-call events for protocol-internal exec results"
            );
        }
        if self.tool_call_events {
            assert!(
                run.tool_call_event,
                "{scenario_name} did not emit tool-call accounting events for exec results"
            );
        }
        if self.no_assistant_conversation_progress {
            assert!(
                !run.assistant_conversation_progress,
                "{scenario_name} wrote protocol-owned assistant conversation progress"
            );
        }
        for expected in &self.system_message_contains {
            assert!(
                machine.messages().iter().any(|message| {
                    message.role == MessageRole::System
                        && message
                            .parts
                            .iter()
                            .any(|part| part.content.contains(expected))
                }) || run
                    .llm_requests
                    .iter()
                    .flat_map(|request| request.messages.iter())
                    .filter(|message| message.role == LlmRole::System)
                    .flat_map(|message| message.blocks.iter())
                    .any(|block| {
                        matches!(block, LlmContentBlock::Text { text, .. } if text.contains(expected))
                    }),
                "{scenario_name} missing system repair feedback containing `{expected}`"
            );
        }
        for omitted in &self.system_message_omits {
            assert!(
                !machine.messages().iter().any(|message| {
                    message.role == MessageRole::System
                        && message
                            .parts
                            .iter()
                            .any(|part| part.content.contains(omitted))
                }),
                "{scenario_name} found unexpected system feedback containing `{omitted}`"
            );
        }
        if let Some(expected) = &self.assistant_reasoning_texts {
            assert_eq!(
                assistant_reasoning_texts(machine),
                expected
                    .iter()
                    .map(|text| (*text).to_string())
                    .collect::<Vec<_>>(),
                "{scenario_name} assistant reasoning texts changed"
            );
        }
        if let Some(expected) = &self.assistant_visible_texts {
            assert_eq!(
                assistant_visible_texts(machine),
                expected
                    .iter()
                    .map(|text| (*text).to_string())
                    .collect::<Vec<_>>(),
                "{scenario_name} assistant visible texts changed"
            );
        }
        if let Some(expected) = self.assistant_message_count {
            assert_eq!(
                assistant_messages(machine).len(),
                expected,
                "{scenario_name} assistant message count changed"
            );
        }
        if let Some(expected) = &self.plugin_stream_visible_texts {
            assert_eq!(
                run.plugin_stream_visible_texts,
                expected
                    .iter()
                    .map(|text| (*text).to_string())
                    .collect::<Vec<_>>(),
                "{scenario_name} plugin-visible stream changed"
            );
        }
        if let Some(expected) = &self.plugin_spliced_response_texts {
            assert_eq!(
                run.plugin_spliced_response_texts,
                expected
                    .iter()
                    .map(|text| (*text).to_string())
                    .collect::<Vec<_>>(),
                "{scenario_name} plugin-spliced response changed"
            );
        }
        if let Some(expected) = &self.plugin_stream_abort_requests {
            assert_eq!(
                run.plugin_stream_abort_requests, *expected,
                "{scenario_name} plugin stream-abort decisions changed"
            );
        }
        if let Some(expected) = &self.llm_extraction_payload {
            assert_eq!(
                single_llm_extraction_payload(machine),
                *expected,
                "{scenario_name} llm_extraction diagnostic changed"
            );
        }
        if let Some(expected) = &self.turn_outcome {
            assert!(
                run.turn_outcomes.iter().any(|outcome| outcome == expected),
                "{scenario_name} missing turn outcome {expected:?}: {:?}",
                run.turn_outcomes
            );
        }
        if let Some((frame_key_material, task)) = self.agent_frame_switch {
            let expected_frame_key = lash_core::FrameKey::from_caller_material(frame_key_material)
                .expect("non-empty frame key material");
            assert!(
                run.turn_outcomes.iter().any(|outcome| matches!(
                    outcome,
                    lash_sansio::TurnOutcome::AgentFrameSwitch {
                        frame_key: actual_frame_key,
                        task: actual_task,
                        ..
                    } if actual_frame_key == &expected_frame_key && actual_task == task
                )),
                "{scenario_name} missing agent-frame switch outcome for {frame_key_material}: {:?}",
                run.turn_outcomes
            );
        }
        if let Some((tool_name, message)) = self.tool_error_message {
            assert!(
                run.turn_outcomes.iter().any(|outcome| matches!(
                    outcome,
                    lash_sansio::TurnOutcome::Stopped(lash_sansio::TurnStop::ToolError {
                        tool_name: actual_tool_name,
                        value,
                    }) if actual_tool_name == tool_name
                        && value.get("message") == Some(&serde_json::json!(message))
                )),
                "{scenario_name} missing tool-error outcome for {tool_name}: {:?}",
                run.turn_outcomes
            );
        }
        if let Some(expected) = &self.trajectory_last {
            let trajectory = machine_trajectory(machine);
            let entry = trajectory
                .last()
                .unwrap_or_else(|| panic!("{scenario_name} missing RLM trajectory entry"));
            assert_eq!(entry.code, expected.code, "{scenario_name} trajectory code");
            assert_eq!(
                entry.output, expected.output,
                "{scenario_name} trajectory output"
            );
            assert_eq!(
                entry.final_output, expected.final_output,
                "{scenario_name} trajectory final value"
            );
            assert_eq!(
                entry.error, expected.error,
                "{scenario_name} trajectory error"
            );
        }
        if self.trajectory_omits_tool_call_ids {
            let trajectory = machine_trajectory(machine);
            let entry = trajectory
                .last()
                .unwrap_or_else(|| panic!("{scenario_name} missing RLM trajectory entry"));
            assert!(
                serde_json::to_value(entry)
                    .expect("trajectory entry serializes")
                    .get("tool_call_ids")
                    .is_none(),
                "{scenario_name} leaked protocol-internal tool call ids into the RLM trajectory"
            );
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RlmTrajectoryExpectation {
    pub(crate) code: &'static str,
    pub(crate) output: Vec<String>,
    pub(crate) error: Option<String>,
    pub(crate) final_output: Option<serde_json::Value>,
}

#[derive(Default)]
pub(crate) struct RlmProtocolRun {
    /// Behavior transcript built from the machine's own effect stream, in drain
    /// order. See `lash_core::testing::sansio_transcript`.
    pub(crate) transcript: Transcript,
    /// Real `TurnCheckpoint` serialize/deserialize/restore round trips performed.
    pub(crate) round_trips: usize,
    pub(crate) initial_request: Option<LlmRequest>,
    pub(crate) llm_requests: Vec<LlmRequest>,
    pub(crate) exec_codes: Vec<String>,
    pub(crate) checkpoints: Vec<CheckpointKind>,
    pub(crate) llm_call_count: usize,
    pub(crate) turn_outcomes: Vec<lash_sansio::TurnOutcome>,
    pub(crate) final_message_event: bool,
    pub(crate) tool_call_event: bool,
    pub(crate) assistant_conversation_progress: bool,
    pub(crate) plugin_stream_visible_texts: Vec<String>,
    pub(crate) plugin_spliced_response_texts: Vec<String>,
    pub(crate) plugin_stream_abort_requests: Vec<bool>,
}

impl RlmProtocolRun {
    pub(crate) fn record(&mut self, effects: &[Effect]) {
        for effect in effects {
            match effect {
                Effect::LlmCall { request, .. } => {
                    self.llm_call_count += 1;
                    self.llm_requests.push(request.as_ref().clone());
                }
                Effect::ExecCode { code, .. } => {
                    self.exec_codes.push(code.clone());
                }
                Effect::Checkpoint { checkpoint, .. } => self.checkpoints.push(*checkpoint),
                Effect::Emit(SessionStreamEvent::TurnOutcome { outcome }) => {
                    self.turn_outcomes.push(outcome.clone());
                }
                Effect::Emit(SessionStreamEvent::Message { kind, .. }) if kind == "final" => {
                    self.final_message_event = true;
                }
                Effect::Emit(SessionStreamEvent::ToolCall { .. }) => {
                    self.tool_call_event = true;
                }
                Effect::Progress { event_delta, .. } => {
                    self.assistant_conversation_progress |= event_delta.iter().any(|event| {
                        matches!(
                            event,
                            lash_sansio::SessionHistoryRecord::Conversation(record)
                                if record.to_message().role == MessageRole::Assistant
                        )
                    });
                }
                _ => {}
            }
        }
    }
}

pub(crate) fn rlm_protocol_plugin_factory() -> Arc<dyn PluginFactory> {
    Arc::new(
        RlmProtocolPluginFactory::new(
            RlmProtocolPluginConfig::new(
                lashlang::ExecutionBound::Unbounded,
                lashlang::ExecutionBound::Unbounded,
                lashlang::ExecutionBound::instructions(64 * 1024 * 1024),
            ),
            lashlang::global_in_memory_lashlang_artifact_store(),
        )
        .with_process_lifecycle(false),
    )
}

struct PluginStreamRun {
    visible_text: String,
    response: LlmResponse,
    abort_requested: bool,
}

fn drive_plugin_stream(
    plugins: &PluginSession,
    chunks: &[String],
    response: LlmResponse,
) -> PluginStreamRun {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build protocol-scenario plugin-hook runtime")
        .block_on(async {
            let mut visible_text = String::new();
            let mut abort_requested = false;
            for chunk in chunks {
                let transforms = plugins
                    .transform_assistant_stream("rlm-protocol-scenario-hooks", chunk.clone())
                    .await
                    .expect("protocol-scenario stream hook succeeds");
                let transformed = transforms
                    .last()
                    .map(|owned| owned.value.clone())
                    .unwrap_or_else(|| AssistantStreamTransform {
                        chunk: chunk.clone(),
                        reasoning_deltas: Vec::new(),
                        events: Vec::new(),
                        abort_stream: false,
                    });
                visible_text.push_str(&transformed.chunk);
                abort_requested |= transformed.abort_stream;
                if abort_requested {
                    break;
                }
            }

            let transforms = plugins
                .transform_assistant_response("rlm-protocol-scenario-hooks", response.clone())
                .await
                .expect("protocol-scenario response hook succeeds");
            let response = transforms
                .last()
                .map(|owned| owned.value.response.clone())
                .unwrap_or(response);
            plugins
                .finish_assistant_stream(
                    "rlm-protocol-scenario-hooks",
                    if abort_requested {
                        AssistantStreamFinishReason::Aborted
                    } else {
                        AssistantStreamFinishReason::Complete
                    },
                )
                .await
                .expect("protocol-scenario stream-finished hook succeeds");

            PluginStreamRun {
                visible_text,
                response,
                abort_requested,
            }
        })
}
