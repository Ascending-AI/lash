use crate::SessionId;
use crate::TurnId;
use std::sync::Arc;

use crate::MessageSequence;
use crate::sansio::{TurnMachine, TurnMachineConfig, TurnProtocol, UnitTurnProtocol};
use crate::turn_driver::TurnDriverPreamble;

pub struct SansIoTurnInput<M: TurnProtocol = UnitTurnProtocol> {
    pub session_id: SessionId,
    /// Where the turn's model-issued tool calls are admitted.
    pub model_tool_calls: crate::ModelToolCalls,
    pub agent_frame_id: String,
    pub turn_id: TurnId,
    pub autonomous: bool,
    pub model: String,
    /// Model context-window size in tokens, if known. Threaded into the kernel
    /// so it can reclassify a zero-output `OutputLimit` as `ContextOverflow`.
    pub max_context_tokens: Option<usize>,
    pub messages: MessageSequence,
    pub events: crate::AppendVec<crate::SessionHistoryRecord<M::Event>>,
    pub turn_causes: Vec<crate::TurnCause>,
    pub protocol_run_offset: usize,
    pub turn_driver_preamble: Arc<TurnDriverPreamble<M>>,
    pub turn_budget: crate::TurnBudget,
    pub no_progress_budget: crate::NoProgressBudget,
    pub model_variant: crate::llm::capability::ReasoningSelection,
    pub llm_profile_capability: crate::llm::capability::LlmProfileCapability,
    /// The session's recorded attachment-acceptance rules every request of
    /// the turn carries.
    pub attachment_acceptance: Arc<crate::llm::capability::AttachmentCapabilitySnapshot>,
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    pub request_defaults: crate::llm::capability::LlmProfileRequestDefaults,
    pub generation: crate::llm::types::GenerationOptions,
    pub emit_llm_trace: bool,
    pub termination: M::Termination,
}

pub struct PreparedTurnMachine<M: TurnProtocol = UnitTurnProtocol> {
    pub machine: TurnMachine<M>,
    pub turn_driver_preamble: Arc<TurnDriverPreamble<M>>,
}

pub fn build_turn<M: TurnProtocol>(input: SansIoTurnInput<M>) -> PreparedTurnMachine<M> {
    let machine = TurnMachine::new_shared_with_turn_causes(
        TurnMachineConfig {
            model_tool_calls: input.model_tool_calls,
            protocol_driver: input.turn_driver_preamble.config.protocol.clone(),
            projector: input.turn_driver_preamble.config.projector.clone(),
            model: input.model,
            max_context_tokens: input.max_context_tokens,
            turn_budget: input.turn_budget,
            no_progress_budget: input.no_progress_budget,
            model_variant: input.model_variant,
            llm_profile_capability: input.llm_profile_capability,
            attachment_acceptance: input.attachment_acceptance,
            extra_body: input.extra_body,
            request_defaults: input.request_defaults,
            generation: input.generation,
            autonomous: input.autonomous,
            session_id: input.session_id,
            agent_frame_id: input.agent_frame_id,
            turn_id: input.turn_id,
            writer_formats: Arc::clone(&input.turn_driver_preamble.writer_formats),
            emit_llm_trace: input.emit_llm_trace,
            termination: input.termination,
        },
        input.messages,
        input.events,
        input.protocol_run_offset,
        input.turn_causes,
    );

    PreparedTurnMachine {
        machine,
        turn_driver_preamble: input.turn_driver_preamble,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::ToolDefinition;
    use crate::sansio::{CompletedToolCall, DriverAction, DriverContextView, ProtocolDriverHandle};
    use crate::turn_driver::{TurnDriverConfig, TurnDriverPreamble};

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            format!("Tool {name}"),
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            serde_json::json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
    }

    /// Minimal no-op driver so the turn-machine test can build a
    /// `TurnDriverPreamble` without pulling in a protocol plugin crate (which would
    /// create a cyclic dependency on `lash` from `lash-sansio`).
    struct NoopDriver;

    impl ProtocolDriverHandle for NoopDriver {
        fn prepare_protocol_iteration(&self, _ctx: DriverContextView<'_>) -> Vec<DriverAction> {
            Vec::new()
        }

        fn handle_llm_success(
            &self,
            _ctx: DriverContextView<'_>,
            _request: Arc<crate::llm::types::LlmRequest>,
            _driver_state: Option<serde_json::Value>,
            _llm_response: crate::llm::types::LlmResponse,
            _calls: &crate::ResponseToolCalls,
            _text_streamed: bool,
        ) -> Vec<DriverAction> {
            Vec::new()
        }

        fn handle_tool_results(
            &self,
            _ctx: DriverContextView<'_>,
            _completed: Vec<CompletedToolCall>,
        ) -> Vec<DriverAction> {
            Vec::new()
        }

        fn handle_exec_result(
            &self,
            _ctx: DriverContextView<'_>,
            _driver_state: serde_json::Value,
            _result: Result<crate::ExecResponse, crate::ExecCodeFailure>,
        ) -> Vec<DriverAction> {
            Vec::new()
        }
    }

    #[test]
    fn build_turn_creates_machine_at_the_run_offset() {
        let tool_catalog = Arc::new(crate::ToolCatalog::from_tool_definitions(vec![tool(
            "read_file",
        )]));
        let turn_driver_preamble = Arc::new(TurnDriverPreamble {
            config: TurnDriverConfig::chat(Arc::new(NoopDriver)),
            tool_specs: tool_catalog.model_tool_specs(),
            tool_names: tool_catalog.tool_names(),
            writer_formats: crate::build_newest_writer_formats(),
        });
        let prepared = build_turn(SansIoTurnInput {
            session_id: SessionId::from("session".to_string()),
            model_tool_calls: crate::ModelToolCalls::fixture(),
            agent_frame_id: "frame-test".to_string(),
            turn_id: TurnId::from("turn"),
            autonomous: false,
            model: "gpt-5".to_string(),
            max_context_tokens: None,
            messages: crate::MessageSequence::default(),
            events: crate::AppendVec::new(),
            turn_causes: Vec::new(),
            protocol_run_offset: 2,
            turn_driver_preamble,
            turn_budget: crate::TurnBudget::bounded(3),
            no_progress_budget: crate::NoProgressBudget::default(),
            model_variant: crate::ReasoningSelection::Effort("mini".to_string()),
            llm_profile_capability: crate::llm::capability::LlmProfileCapability::default(),
            attachment_acceptance: Default::default(),
            extra_body: Default::default(),
            request_defaults: Default::default(),
            generation: crate::llm::types::GenerationOptions::default(),
            emit_llm_trace: true,
            termination: (),
        });

        assert_eq!(prepared.machine.protocol_iteration(), 2);
        assert_eq!(prepared.turn_driver_preamble.tool_specs.len(), 1);
    }
}
