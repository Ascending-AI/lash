use std::sync::Arc;

use crate::MessageSequence;
use crate::prompt::PreparedPrompt;
use crate::sansio::{TurnMachine, TurnMachineConfig, TurnProtocol, UnitTurnProtocol};
use crate::turn_driver::TurnDriverPreamble;

pub struct SansIoTurnInput<M: TurnProtocol = UnitTurnProtocol> {
    pub session_id: String,
    pub turn_id: String,
    pub autonomous: bool,
    pub model: String,
    /// Model context-window size in tokens, if known. Threaded into the kernel
    /// so it can reclassify a zero-output `OutputLimit` as `ContextOverflow`.
    pub max_context_tokens: Option<usize>,
    pub messages: MessageSequence,
    pub events: Arc<Vec<crate::SessionHistoryRecord<M::Event>>>,
    pub turn_causes: Vec<crate::TurnCause>,
    pub protocol_run_offset: usize,
    pub turn_driver_preamble: Arc<TurnDriverPreamble<M>>,
    pub prepared_prompt: PreparedPrompt,
    pub turn_budget: crate::TurnBudget,
    pub no_progress_budget: crate::NoProgressBudget,
    pub model_variant: crate::llm::capability::ReasoningSelection,
    pub model_capability: crate::llm::capability::ModelCapability,
    pub generation: crate::llm::types::GenerationOptions,
    pub emit_llm_trace: bool,
    pub termination: M::Termination,
}

pub struct PreparedTurnMachine<M: TurnProtocol = UnitTurnProtocol> {
    pub machine: TurnMachine<M>,
    pub prepared_prompt: PreparedPrompt,
    pub turn_driver_preamble: Arc<TurnDriverPreamble<M>>,
}

pub fn build_turn<M: TurnProtocol>(input: SansIoTurnInput<M>) -> PreparedTurnMachine<M> {
    let machine = TurnMachine::new_shared_with_turn_causes(
        TurnMachineConfig {
            protocol_driver: input.turn_driver_preamble.config.protocol.clone(),
            projector: input.turn_driver_preamble.config.projector.clone(),
            sync_execution_environment: input
                .turn_driver_preamble
                .config
                .sync_execution_environment,
            model: input.model,
            max_context_tokens: input.max_context_tokens,
            turn_budget: input.turn_budget,
            no_progress_budget: input.no_progress_budget,
            model_variant: input.model_variant,
            model_capability: input.model_capability,
            generation: input.generation,
            autonomous: input.autonomous,
            tool_specs: input.turn_driver_preamble.tool_specs.clone(),
            system_prompt: Arc::clone(&input.prepared_prompt.system_prompt),
            session_id: input.session_id,
            turn_id: input.turn_id,
            emit_llm_trace: input.emit_llm_trace,
            termination: input.termination,
            turn_limit_final_message: input
                .turn_driver_preamble
                .config
                .turn_limit_final_message
                .clone(),
        },
        input.messages,
        input.events,
        input.protocol_run_offset,
        input.turn_causes,
    );

    PreparedTurnMachine {
        machine,
        prepared_prompt: input.prepared_prompt,
        turn_driver_preamble: input.turn_driver_preamble,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::sansio::{
        CompletedToolCall, DriverAction, DriverContextView, ProtocolDriverHandle, WaitingExecState,
        WaitingLlmState,
    };
    use crate::turn_driver::{TurnDriverConfig, TurnDriverPreamble};
    use crate::{
        PromptBuildInput, PromptContribution, PromptContributionSet, ToolDefinition, build_prompt,
        default_prompt_template, prompt_template_fingerprint, prompt_text_fingerprint,
    };

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
            _waiting: WaitingLlmState,
            _llm_response: crate::llm::types::LlmResponse,
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
            _waiting: WaitingExecState,
            _result: Result<crate::ExecResponse, String>,
        ) -> Vec<DriverAction> {
            Vec::new()
        }
    }

    #[test]
    fn build_turn_creates_machine_with_rendered_system_prompt() {
        let tool_catalog = Arc::new(crate::ToolCatalog::from_tool_definitions(vec![tool(
            "read_file",
        )]));
        let turn_driver_preamble = Arc::new(TurnDriverPreamble {
            config: TurnDriverConfig::chat(
                Arc::new(NoopDriver),
                false,
                Arc::new(test_turn_limit_final_message),
            ),
            tool_specs: tool_catalog.model_tool_specs(),
            tool_names: tool_catalog.tool_names(),
            tool_names_fingerprint: tool_catalog.tool_names_fingerprint(),
            execution_prompt: Arc::from("test prompt"),
            prompt_contributions: Vec::new(),
        });
        let template = default_prompt_template();
        let prompt_contributions =
            PromptContributionSet::new(vec![PromptContribution::guidance("Guide", "Be precise.")]);
        let prepared_prompt = build_prompt(PromptBuildInput {
            template_fingerprint: prompt_template_fingerprint(&template),
            template,
            execution_prompt_fingerprint: prompt_text_fingerprint(
                &turn_driver_preamble.execution_prompt,
            ),
            execution_prompt: Arc::clone(&turn_driver_preamble.execution_prompt),
            tool_names_fingerprint: turn_driver_preamble.tool_names_fingerprint,
            tool_names: Arc::clone(&turn_driver_preamble.tool_names),
            contributions: prompt_contributions,
        });
        let prepared = build_turn(SansIoTurnInput {
            session_id: "session".to_string(),
            turn_id: "turn".to_string(),
            autonomous: false,
            model: "gpt-5".to_string(),
            max_context_tokens: None,
            messages: crate::MessageSequence::default(),
            events: Arc::new(Vec::new()),
            turn_causes: Vec::new(),
            protocol_run_offset: 2,
            turn_driver_preamble,
            prepared_prompt,
            turn_budget: crate::TurnBudget::bounded(3),
            no_progress_budget: crate::NoProgressBudget::default(),
            model_variant: crate::ReasoningSelection::Effort("mini".to_string()),
            model_capability: crate::llm::capability::ModelCapability::default(),
            generation: crate::llm::types::GenerationOptions::default(),
            emit_llm_trace: true,
            termination: (),
        });

        assert_eq!(prepared.machine.protocol_iteration(), 2);
        assert!(
            prepared
                .prepared_prompt
                .system_prompt
                .contains("Be precise.")
        );
        assert_eq!(prepared.turn_driver_preamble.tool_specs.len(), 1);
    }

    fn test_turn_limit_final_message(message_id: String, max_turns: usize) -> crate::Message {
        crate::Message {
            id: message_id.clone(),
            role: crate::MessageRole::System,
            parts: crate::shared_parts(vec![crate::Part::error(
                format!("{message_id}.p0"),
                format!("Turn limit reached ({max_turns}) before a final test response."),
            )]),
            origin: None,
        }
    }
}
