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
    pub model: crate::llm_profile::LlmProfileConfig,
    pub messages: MessageSequence,
    pub events: crate::AppendVec<crate::SessionHistoryRecord<M::Event>>,
    pub protocol_run_offset: usize,
    pub turn_driver_preamble: Arc<TurnDriverPreamble<M>>,
    pub turn_budget: crate::TurnBudget,
    pub no_progress_budget: crate::NoProgressBudget,
    /// The session's recorded attachment-acceptance rules every request of
    /// the turn carries.
    pub attachment_acceptance: Arc<crate::llm::capability::AttachmentCapabilitySnapshot>,
    pub generation: crate::llm::types::GenerationOptions,
    pub emit_llm_trace: bool,
    pub termination: M::Termination,
}

pub struct PreparedTurnMachine<M: TurnProtocol = UnitTurnProtocol> {
    pub machine: TurnMachine<M>,
    pub turn_driver_preamble: Arc<TurnDriverPreamble<M>>,
}

pub fn build_turn<M: TurnProtocol>(input: SansIoTurnInput<M>) -> PreparedTurnMachine<M> {
    let machine = TurnMachine::new_shared(
        TurnMachineConfig {
            model_tool_calls: input.model_tool_calls,
            protocol_driver: input.turn_driver_preamble.config.protocol.clone(),
            projector: input.turn_driver_preamble.config.projector.clone(),
            model: input.model,
            turn_budget: input.turn_budget,
            no_progress_budget: input.no_progress_budget,
            attachment_acceptance: input.attachment_acceptance,
            generation: input.generation,
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
    );

    PreparedTurnMachine {
        machine,
        turn_driver_preamble: input.turn_driver_preamble,
    }
}
