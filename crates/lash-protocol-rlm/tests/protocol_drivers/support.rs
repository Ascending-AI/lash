use lash_kernel_value::identity::SessionId;
use lash_protocol_types::ReportedFailure;
use lash_vocabulary::identity::TurnId;
pub(crate) use std::sync::Arc;

pub(crate) use lash_core::sansio::{self, ChatContextProjector, ProtocolDriverHandle, Response};
pub(crate) use lash_core::testing::behavior_transcript::Transcript;
pub(crate) use lash_core::testing::sansio_transcript::record_effects;
pub(crate) use lash_core::{Effect, TurnMachine, TurnMachineConfig};
pub(crate) use lash_protocol_rlm::RlmDriver;

/// Actor name pinned on every RLM Protocol Scenario transcript. The harness executes
/// one sans-io machine for one session.
pub(crate) const RLM_TRANSCRIPT_ACTOR: &str = "rlm";
use lash_core::CellRecord;
pub(crate) use lash_protocol_types::{
    CheckpointKind, Message, MessageRole, Part, PartKind, SessionStreamEvent,
};

pub(crate) use lash_protocol_types::{
    llm::types::LlmContentBlock, llm::types::LlmOutputPart, llm::types::LlmRequest,
    llm::types::LlmResponse, llm::types::LlmRole,
};

pub(crate) use lash_rlm_types::{RlmCreateExtras, RlmProtocolEvent};

pub(crate) fn recorded_rlm_event(event: &lash_core::ProtocolEvent) -> Option<RlmProtocolEvent> {
    lash_protocol_rlm::decode_rlm_protocol_event(event)
        .unwrap_or_else(|error| panic!("recorded RLM event is corrupt: {error:?}"))
        .map(|event| RlmProtocolEvent::clone(&event))
}

pub(crate) fn test_config() -> TurnMachineConfig {
    test_config_with_termination(lash_core::TerminationMode::default())
}

/// A config whose turns end under `termination`.
pub(crate) fn test_config_with_termination(
    termination: lash_core::TerminationMode,
) -> TurnMachineConfig {
    test_config_with_protocol_turn_options(recorded_namespace(RlmCreateExtras {
        finish_value_schema: None,
        termination: Some(termination),
        render: None,
    }))
}

/// The recorded RLM namespace of a session created stating `options`: what the
/// driver reads a turn's options from. The behaviour is an unbounded
/// RLM one without process lifecycle.
#[expect(
    clippy::expect_used,
    reason = "test support: a recorded RLM namespace is plain data and always encodes"
)]
pub(crate) fn recorded_namespace(options: RlmCreateExtras) -> lash_core::ProtocolTurnOptions {
    lash_core::ProtocolTurnOptions::typed(lash_protocol_rlm::RlmRecordedConfig {
        finish_value_schema: None,
        render: options.render,
        termination: options.termination,

        dialect: None,
        behaviour: lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash_protocol_rlm::InstructionBound::unbounded())
            .memory_limit(lash_protocol_rlm::MemoryBound::unbounded())
            .build()
            .recorded_behaviour(&lash_protocol_rlm::CellDialect::typescript())
            .expect("the standard tool presentation admits"),
    })
    .expect("the recorded namespace encodes")
}

pub(crate) fn test_config_with_protocol_turn_options(
    termination: lash_core::ProtocolTurnOptions,
) -> TurnMachineConfig {
    let protocol_driver: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>> =
        Arc::new(RlmDriver::new(lash_protocol_rlm::CellDialect::typescript()));
    TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver,
        projector: Arc::new(ChatContextProjector::default()),
        model: lash_protocol_types::llm_profile::LlmProfileConfig::new(
            lash_protocol_types::llm_profile::RecordedLlmProfile::mint(
                lash_protocol_types::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_protocol_types::llm_profile::LlmProfileMetadata::new(
                    "test-model".to_string(),
                    std::num::NonZeroUsize::MIN.saturating_add(127_999),
                    lash_protocol_types::llm::capability::CacheRetention::Short,
                )
                .with_capability(lash_core::LlmProfileCapability::default())
                .with_extra_body(Default::default()),
            ),
        )
        .with_reasoning(Default::default()),
        turn_budget: lash_core::TurnBudget::Unbounded,
        no_progress_budget: lash_core::NoProgressBudget::bounded(12),
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        session_id: SessionId::from("test"),
        agent_frame_id: "test-frame".to_string(),
        turn_id: TurnId::from("test-turn"),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination,
    }
}

pub(crate) fn user_message(content: &str) -> Message {
    Message {
        id: "m0".to_string(),
        role: MessageRole::User,
        parts: vec![Part::text("m0.p0".to_string(), content.to_string(), None)].into(),
        origin: None,
        reply_marker: None,
    }
}

pub(crate) fn drain_effects(machine: &mut TurnMachine) -> Vec<Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
        if let Effect::SyncExecutionEnvironment { id, .. } = effect {
            effects.push(effect);
            machine.handle_response(Response::ExecutionEnvironmentSynced {
                id,
                result: Ok(rlm_environment()),
            });
            continue;
        }
        effects.push(effect);
    }
    effects
}

// Completion transfers the records to the consumer and releases the machine's copy.
pub(crate) fn delivered_events(
    effects: &[Effect],
) -> impl Iterator<Item = &lash_core::SessionHistoryRecord> {
    effects.iter().flat_map(|effect| match effect {
        Effect::Progress { event_delta, .. } | Effect::Done { event_delta, .. } => {
            event_delta.as_slice()
        }
        _ => &[],
    })
}

pub(crate) fn find_llm_call(effects: &[Effect]) -> Option<&sansio::EffectId> {
    effects.iter().find_map(|e| match e {
        Effect::LlmCall { id, .. } => Some(id),
        _ => None,
    })
}

pub(crate) fn find_llm_request(effects: &[Effect]) -> Option<LlmRequest> {
    effects
        .iter()
        .find_map(|e| match e {
            Effect::LlmCall { request, .. } => Some(request),
            _ => None,
        })
        .map(|request| request.materialize())
}

pub(crate) fn find_checkpoint(effects: &[Effect]) -> Option<(sansio::EffectId, CheckpointKind)> {
    effects.iter().find_map(|e| match e {
        Effect::Checkpoint { id, checkpoint } => Some((*id, *checkpoint)),
        _ => None,
    })
}

pub(crate) fn find_done(
    effects: &[Effect],
) -> Option<(&lash_protocol_types::MessageSequence, usize)> {
    effects.iter().find_map(|e| match e {
        Effect::Done {
            messages,
            event_delta: _,
            protocol_iteration,
        } => Some((messages, *protocol_iteration)),
        _ => None,
    })
}

#[expect(
    clippy::expect_used,
    reason = "test support: the checkpoint is a crate-owned serde round-trip fixture; serialization cannot fail"
)]
pub(crate) fn roundtrip_turn_checkpoint(
    checkpoint: lash_sansio::SavedTurn<lash_core::HostTurnProtocol>,
) -> lash_sansio::SavedTurn<lash_core::HostTurnProtocol> {
    let encoded = serde_json::to_string(&checkpoint).expect("serialize checkpoint");
    serde_json::from_str(&encoded).expect("deserialize checkpoint")
}

pub(crate) fn machine_trajectory(machine: &TurnMachine) -> Vec<CellRecord> {
    machine
        .events()
        .iter()
        .filter_map(|event| match event {
            lash_core::SessionHistoryRecord::Protocol(event) => match recorded_rlm_event(event) {
                Some(RlmProtocolEvent::RlmTrajectoryEntry(entry)) => {
                    Some(CellRecord::clone(&entry))
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "test support: the assertion one line above pins the count to exactly one diagnostic, so next() is Some and the payload is the object the driver built"
)]
pub(crate) fn single_llm_extraction_payload(machine: &TurnMachine) -> serde_json::Value {
    let payloads: Vec<_> = machine
        .events()
        .iter()
        .filter_map(|event| match event {
            lash_core::SessionHistoryRecord::Protocol(event) => match recorded_rlm_event(event) {
                Some(RlmProtocolEvent::RlmDiagnostic(diagnostic)) => (diagnostic.phase
                    == lash_rlm_types::RlmDiagnosticPhase::NativeExtraction)
                    .then_some(diagnostic.payload),
                _ => None,
            },
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

#[expect(
    clippy::expect_used,
    reason = "test support: the payload is asserted to be the five-key extraction object this fn names, so the reads below always land"
)]
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
    assistant_messages(machine)
        .into_iter()
        .flat_map(|message| {
            message
                .parts
                .iter()
                .filter(|part| matches!(part.kind(), PartKind::Reasoning))
                .map(|part| part.content().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(crate) fn assistant_visible_texts(machine: &TurnMachine) -> Vec<String> {
    assistant_messages(machine)
        .into_iter()
        .flat_map(|message| {
            message
                .parts
                .iter()
                .filter(|part| matches!(part.kind(), PartKind::Text | PartKind::Prose))
                .map(|part| part.content().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
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

pub(crate) fn typescript_block(code: &str) -> LlmOutputPart {
    lash_protocol_rlm::test_support::execute_code(code)
}
pub(crate) fn typescript_block_with_prose(prose: &str, code: &str) -> Vec<LlmOutputPart> {
    vec![text_part(prose), typescript_block(code)]
}

pub(crate) fn rlm_response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    LlmResponse {
        parts,
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

pub(crate) fn exec_response(
    output: &[&str],
    error: Option<&str>,
    final_output: Option<serde_json::Value>,
) -> lash_protocol_types::ExecResponse {
    lash_protocol_types::ExecResponse {
        read_facts: Default::default(),
        consumption: Vec::new(),
        prints_retained: None,
        prints: output
            .iter()
            .map(|item| lash_protocol_types::CellPrint {
                text: (*item).to_string(),
                value: serde_json::json!(*item),
                projection: Default::default(),
            })
            .collect(),
        calls: Vec::new(),
        // A cell that `control.finish` ended holds that call's settled
        // record: the cell's outcome is the call's control.
        tool_calls: final_output
            .iter()
            .filter(|_| error.is_none())
            .map(|value| lash_protocol_types::ToolCallRecord {
                call_id: finish_call_id(),
                provider_call_id: None,
                tool: lash_protocol_rlm::FINISH_TOOL_NAME.to_string(),
                args: value.clone(),
                output: lash_protocol_types::ToolCallOutput::finish(value.clone()),
            })
            .collect(),
        printed_images: Vec::new(),
        result: match (error, final_output) {
            (Some(message), _) => {
                lash_protocol_types::CellOutcome::Failed(lash_protocol_types::CellFailure::new(
                    lash_protocol_types::CellFailureKind::Program,
                    message,
                ))
            }
            (None, Some(value)) => finished(value),
            (None, None) => lash_protocol_types::CellOutcome::Completed,
        },
        retained_finish_value: None,
        degraded_bindings: Vec::new(),
        bindings: Default::default(),
        suspended: false,
    }
}

/// The environment an RLM session syncs: its control tools, `finish`
/// among them, are always installed.
pub(crate) fn rlm_environment() -> sansio::ExecutionEnvironmentSync {
    sansio::ExecutionEnvironmentSync {
        turn_controls: [(
            lash_protocol_rlm::FINISH_TOOL_NAME.to_string(),
            lash_kernel_value::tool_declaration::TurnControls::finish(
                lash_kernel_value::json_schema::JsonSchema::any(),
            ),
        )]
        .into(),
        ..sansio::ExecutionEnvironmentSync::default()
    }
}

/// The call id of the scripted cells' `control.finish` call.
pub(crate) fn finish_call_id() -> lash_protocol_types::ToolCallId {
    lash_protocol_types::ToolCallId::fixture("finish-call")
}

/// The outcome of a cell `control.finish(value)` ended.
pub(crate) fn finished(value: serde_json::Value) -> lash_protocol_types::CellOutcome {
    lash_protocol_types::CellOutcome::finished_by(
        lash_protocol_rlm::FINISH_TOOL_NAME,
        finish_call_id(),
        value,
    )
}

/// The typed failure a trajectory entry records for a defect in the program.
pub(crate) fn program_failure(message: &str) -> lash_core::CellFailure {
    lash_core::CellFailure::new(lash_core::CellFailureKind::Program, message)
}

pub(crate) fn effects_include_runtime_error(effects: &[Effect], message_fragment: &str) -> bool {
    let has_error = effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::Emit(SessionStreamEvent::Error(ReportedFailure { message, .. }))
                if message.contains(message_fragment)
        )
    });
    let has_runtime_outcome = effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::Emit(SessionStreamEvent::TurnOutcome {
                outcome: lash_protocol_types::TurnOutcome::Stopped(
                    lash_protocol_types::TurnStop::RuntimeError { .. }
                )
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
// These scenarios execute the protocol state machine through declarative LLM,
// exec, and checkpoint steps. Direct white-box tests remain below only when
// they intentionally corrupt turn options or checkpoint driver state.
pub(crate) struct RlmProtocolScenario {
    pub(crate) name: &'static str,
    pub(crate) user_message: &'static str,
    pub(crate) termination: lash_core::TerminationMode,
    pub(crate) max_turns: Option<usize>,
    pub(crate) steps: Vec<RlmProtocolStep>,
    pub(crate) expectations: RlmProtocolExpectations,
}

impl RlmProtocolScenario {
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            name,
            user_message: "perform one step",
            termination: lash_core::TerminationMode::default(),
            max_turns: None,
            steps: Vec::new(),
            expectations: RlmProtocolExpectations::default(),
        }
    }

    pub(crate) fn termination(mut self, termination: lash_core::TerminationMode) -> Self {
        self.termination = termination;
        self
    }

    pub(crate) fn llm_response(mut self, parts: Vec<LlmOutputPart>) -> Self {
        self.steps.push(RlmProtocolStep::LlmResponse {
            text_streamed: false,
            parts,
        });
        self
    }

    pub(crate) fn exec_result(mut self, result: lash_protocol_types::ExecResponse) -> Self {
        self.steps
            .push(RlmProtocolStep::ExecResult(Box::new(result)));
        self
    }

    pub(crate) fn expect(mut self, expectations: RlmProtocolExpectations) -> Self {
        self.expectations = expectations;
        self
    }

    pub(crate) fn run(self) -> RlmProtocolRun {
        let mut config = test_config_with_termination(self.termination);
        config.turn_budget = self
            .max_turns
            .map(lash_core::TurnBudget::bounded)
            .unwrap_or(lash_core::TurnBudget::Unbounded);
        let mut machine = TurnMachine::new(
            config,
            vec![user_message(self.user_message)],
            Default::default(),
            0,
        );
        let mut observed = RlmProtocolRun::default();
        observed
            .transcript
            .pin(RLM_TRANSCRIPT_ACTOR, RLM_TRANSCRIPT_ACTOR);
        let mut effects = drain_effects(&mut machine);
        observed.record(&effects);
        record_effects(&mut observed.transcript, RLM_TRANSCRIPT_ACTOR, &effects);
        observed.initial_request = find_llm_request(&effects);

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
                        result: Ok(result.as_ref().clone()),
                    });
                }
            }

            effects = drain_effects(&mut machine);
            observed.record(&effects);
            record_effects(&mut observed.transcript, RLM_TRANSCRIPT_ACTOR, &effects);
        }

        self.expectations.assert(self.name, &observed, &machine);
        let rendered = observed.transcript.render();
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.contains("checkpoint.request"))
                .count(),
            observed.checkpoints.len()
        );
        observed
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RlmProtocolStep {
    LlmResponse {
        text_streamed: bool,
        parts: Vec<LlmOutputPart>,
    },
    ExecResult(Box<lash_protocol_types::ExecResponse>),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RlmProtocolExpectations {
    pub(crate) initial_request_tools_empty: bool,
    pub(crate) exec_codes: Vec<&'static str>,
    pub(crate) checkpoints: Vec<CheckpointKind>,
    pub(crate) llm_call_count: Option<usize>,
    pub(crate) done: Option<bool>,
    pub(crate) no_exec_code: bool,
    pub(crate) no_tool_call_events: bool,
    pub(crate) tool_call_events: bool,
    pub(crate) no_assistant_conversation_progress: bool,
    pub(crate) trajectory_omits_tool_call_ids: bool,
    pub(crate) system_message_contains: Vec<&'static str>,
    pub(crate) system_message_omits: Vec<&'static str>,
    pub(crate) transcript_system_message_count: Option<usize>,
    pub(crate) assistant_reasoning_texts: Option<Vec<&'static str>>,
    pub(crate) assistant_visible_texts: Option<Vec<&'static str>>,
    pub(crate) assistant_message_count: Option<usize>,
    pub(crate) llm_extraction_payload: Option<serde_json::Value>,
    pub(crate) turn_outcome: Option<lash_protocol_types::TurnOutcome>,
    pub(crate) agent_frame_switch: Option<(&'static str, &'static str)>,
    pub(crate) tool_error_message: Option<(&'static str, &'static str)>,
    pub(crate) trajectory_last: Option<RlmTrajectoryExpectation>,
}

impl RlmProtocolExpectations {
    #[expect(
        clippy::expect_used,
        reason = "test support: frame key material is a fixed non-empty fixture and the trajectory entry is crate-owned; a failure is the expectation under test"
    )]
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
                            .any(|part| part.content().contains(expected))
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
                            .any(|part| part.content().contains(omitted))
                }),
                "{scenario_name} found unexpected system feedback containing `{omitted}`"
            );
        }
        if let Some(expected) = self.transcript_system_message_count {
            assert_eq!(
                machine
                    .messages()
                    .iter()
                    .filter(|message| message.role == MessageRole::System)
                    .count(),
                expected,
                "{scenario_name} transcript system-message count changed"
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
                    lash_protocol_types::TurnOutcome::AgentFrameSwitch {
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
                    lash_protocol_types::TurnOutcome::Stopped(lash_protocol_types::TurnStop::ToolError {
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
                entry
                    .prints
                    .iter()
                    .map(|print| &print.text)
                    .collect::<Vec<_>>(),
                expected.output.iter().collect::<Vec<_>>(),
                "{scenario_name} trajectory output"
            );
            assert_eq!(
                entry.result, expected.outcome,
                "{scenario_name} trajectory outcome"
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
    pub(crate) outcome: lash_core::CellOutcome,
}

#[derive(Default)]
pub(crate) struct RlmProtocolRun {
    /// Behavior transcript built from the machine's own effect stream, in drain
    /// order. See `lash_core::testing::sansio_transcript`.
    pub(crate) transcript: Transcript,
    pub(crate) initial_request: Option<LlmRequest>,
    pub(crate) llm_requests: Vec<LlmRequest>,
    pub(crate) exec_codes: Vec<String>,
    pub(crate) checkpoints: Vec<CheckpointKind>,
    pub(crate) llm_call_count: usize,
    pub(crate) turn_outcomes: Vec<lash_protocol_types::TurnOutcome>,
    pub(crate) tool_call_event: bool,
    pub(crate) assistant_conversation_progress: bool,
}

impl RlmProtocolRun {
    pub(crate) fn record(&mut self, effects: &[Effect]) {
        for effect in effects {
            match effect {
                Effect::LlmCall { request, .. } => {
                    self.llm_call_count += 1;
                    self.llm_requests.push(request.materialize());
                }
                Effect::ExecCode { code, .. } => {
                    self.exec_codes.push(code.clone());
                }
                Effect::Checkpoint { checkpoint, .. } => self.checkpoints.push(*checkpoint),
                Effect::Emit(SessionStreamEvent::TurnOutcome { outcome }) => {
                    self.turn_outcomes.push(outcome.clone());
                }
                Effect::Emit(SessionStreamEvent::ToolCall { .. }) => {
                    self.tool_call_event = true;
                }
                Effect::Progress { event_delta, .. } => {
                    self.assistant_conversation_progress |= event_delta.iter().any(|event| {
                        matches!(
                            event,
                            lash_protocol_types::SessionHistoryRecord::Conversation(record)
                                if record.to_message().role == MessageRole::Assistant
                        )
                    });
                }
                _ => {}
            }
        }
    }
}

pub(crate) fn invalid_code_call() -> LlmOutputPart {
    LlmOutputPart::ToolCall {
        call_id: "invalid".into(),
        tool_name: "execute_code".into(),
        input_json: "{\"code\":".into(),
        replay: None,
    }
}
