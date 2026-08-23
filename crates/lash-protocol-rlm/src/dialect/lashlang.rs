use std::sync::Arc;

use lash_core::SessionError;
use lash_lashlang_runtime::{LashlangArtifactStore, LashlangSurface, SharedDeferredToolResolver};

use super::{CellTags, DialectSession, RlmDialect, RlmDialectSession, SourceDialect};
use crate::executor::RlmLashlangExecutionTraceConfig;
use crate::projection::ProjectionResolver;

pub(crate) const LANGUAGE_ID: &str = "lashlang";

#[derive(Clone)]
pub(crate) struct LashlangDialectServices {
    pub(crate) projection_resolver: Arc<dyn ProjectionResolver>,
    pub(crate) artifact_store: Arc<dyn LashlangArtifactStore>,
    pub(crate) deferred_tool_resolver: Option<SharedDeferredToolResolver>,
    pub(crate) execution_trace_config: RlmLashlangExecutionTraceConfig,
    pub(crate) execution_bounds: crate::plugin::ExecutionBounds,
}

pub(crate) struct LashlangDialect {
    surface: LashlangSurface,
    services: Option<LashlangDialectServices>,
}

impl LashlangDialect {
    pub(crate) fn new(surface: LashlangSurface, services: LashlangDialectServices) -> Self {
        Self {
            surface,
            services: Some(services),
        }
    }

    pub(crate) fn prompt_only(surface: LashlangSurface) -> Self {
        Self {
            surface,
            services: None,
        }
    }
}

pub(crate) const LASHLANG_PROMPT_VOCABULARY: crate::dialect::DialectPromptVocabulary =
    crate::dialect::DialectPromptVocabulary {
        language_name: "lashlang",
        cell_open_tag: "<lashlang>",
        cell_noun: "block",
        print_call: "print",
        print_statement_prefix: "print ",
        print_statement_suffix: "",
        finish_statement: "finish <value>",
        continue_as_call: "control.continue_as(...)",
        continue_as_example: "await control.continue_as({ task: \"continue the audit from the summarized findings\", seed: { problem: input.prompt, findings: findings } })?",
        type_literal_hint: ", or pass a `Type { ... }` literal for nested shapes",
    };

impl RlmDialect for LashlangDialect {
    fn language_id(&self) -> &'static str {
        LANGUAGE_ID
    }

    fn prompt_vocabulary(&self) -> crate::dialect::DialectPromptVocabulary {
        LASHLANG_PROMPT_VOCABULARY
    }

    fn tool_call_path(&self, manifest: &lash_core::ToolManifest) -> Result<String, SessionError> {
        Ok(
            lash_lashlang_runtime::required_tool_lashlang_executable(manifest)
                .map_err(|error| SessionError::Protocol(error.to_string()))?
                .call_path(),
        )
    }

    fn snapshot_engine_id(&self) -> &'static str {
        LANGUAGE_ID
    }

    fn cell_tags(&self) -> CellTags {
        CellTags {
            open: "<lashlang>",
            close: "</lashlang>",
        }
    }

    fn create_session(&self) -> Result<Box<dyn RlmDialectSession>, SessionError> {
        let services = self.services.clone().ok_or_else(|| {
            SessionError::Protocol(
                "prompt-only Lashlang dialect cannot create an execution session".to_string(),
            )
        })?;
        Ok(Box::new(DialectSession::new(
            SourceDialect::Lashlang,
            self.snapshot_engine_id(),
            self.surface.clone(),
            services,
        )))
    }

    fn render_execution_section(
        &self,
        features: crate::protocol::RlmPromptFeatures,
        tool_catalog: &lash_core::ToolCatalog,
    ) -> Result<String, SessionError> {
        let host_environment = self
            .surface
            .host_environment(tool_catalog)
            .map_err(|error| {
                SessionError::Protocol(format!("invalid Lashlang host tool surface: {error}"))
            })?;
        Ok(crate::protocol::rlm_execution_section_for_host_environment(
            features,
            &host_environment,
        ))
    }

    fn finalization_copy(&self, termination: &lash_rlm_types::RlmTermination) -> &'static str {
        match termination {
            lash_rlm_types::RlmTermination::FinishRequired { schema: Some(_) } => {
                "This turn uses finish-required termination. Prose-only does not end the turn. Every non-terminal response must contain a paired `<lashlang>...</lashlang>` block that performs the next step; prose before the block is commentary/status only. Never say you will continue, inspect, patch, wait, monitor, validate, or retry unless the same response also contains the block that does it. The terminal response must be a paired `<lashlang>...</lashlang>` block that calls `finish <value>`, and `<value>` must match the REQUIRED OUTPUT contract."
            }
            lash_rlm_types::RlmTermination::FinishRequired { schema: None } => {
                "This turn uses finish-required termination. Prose-only does not end the turn. Every non-terminal response must contain a paired `<lashlang>...</lashlang>` block that performs the next step; prose before the block is commentary/status only. Never say you will continue, inspect, patch, wait, monitor, validate, or retry unless the same response also contains the block that does it. The terminal response must be a paired `<lashlang>...</lashlang>` block that calls `finish <value>`. Use `finish null` only when null is intentional."
            }
            lash_rlm_types::RlmTermination::Natural => {
                r#"This turn uses natural termination. Each assistant response must choose exactly one of these shapes:

1. Continue working: include a paired `<lashlang>...</lashlang>` block. Brief prose may appear before the block; that prose is commentary/status for the action that follows. A block without `finish` is progress and continues the loop.
2. Finish with prose: write prose with no `<lashlang>` block. A prose-only response immediately ends the turn as the final answer. Use this only when the task is complete and no work remains.
3. Finish with a computed/raw value: call `finish <value>` inside a paired `<lashlang>...</lashlang>` block. This ends the turn with that value.

Every message before the final answer must contain a paired `<lashlang>...</lashlang>` block. Any message may also contain prose; when prose accompanies a Lashlang block, it is commentary/status for the action that follows. Unaccompanied prose is final-answer-only. If any work remains, do not write prose-only. Never say you will continue, inspect, patch, wait, monitor, validate, or retry unless the same response also contains the `<lashlang>` block that does it.

Example multi-step natural turn:

I’ll inspect the current value first.
<lashlang>
preview = slice(to_string(value), 0, 400)
print(preview)
</lashlang>

<lashlang>
result = format("Checked: {}", preview)
print(result)
</lashlang>

Done. I inspected the value and summarized the result."#
            }
        }
    }

    fn cell_error_message(&self, error: crate::protocol::CellExtractionError) -> String {
        match error {
            crate::protocol::CellExtractionError::UnclosedCell => {
                "Model response started a `<lashlang>` block but did not close it. Retry with a complete paired block. A line whose trimmed content is exactly `</lashlang>` closes the cell.".to_string()
            }
        }
    }

    fn turn_limit_final_copy(&self, max_turns: usize) -> String {
        format!(
            "Turn limit reached ({max_turns}). You MUST reply in plain prose now containing:\n\
             1. Summary of what you accomplished\n\
             2. List of remaining tasks not yet completed\n\
             3. Recommended next steps\n\
             Do NOT emit a <lashlang> block, invoke module operations, or call finish/control.continue_as."
        )
    }

    fn finish_required_copy(&self, requires_schema: bool) -> String {
        if requires_schema {
            "Deliver the final answer from a paired `<lashlang>...</lashlang>` block by calling `finish <value>` with a value matching the required output schema. Plain text before the block is recorded only as progress.".to_string()
        } else {
            "Your prose was recorded, but this turn requires an explicit final value. Add a paired `<lashlang>...</lashlang>` block containing `finish <value>`. Use `finish null` only when null is intentional.".to_string()
        }
    }

    fn finish_schema_mismatch_copy(&self) -> String {
        "The `finish` value didn't match the required output schema. Fix the value described in the failed-step observation and call `finish <corrected>` from another paired `<lashlang>...</lashlang>` block.".to_string()
    }

    fn invalid_cell_retry_copy(&self, error_text: &str) -> String {
        format!(
            "{error_text}\n\nReply again using exactly one paired `<lashlang>...</lashlang>` block, with no text after `</lashlang>`."
        )
    }

    fn output_limit_cell_copy(&self, output_token_cap: Option<usize>) -> String {
        let cap = output_token_cap
            .map(|cap| format!(" The request cap was {cap} tokens."))
            .unwrap_or_default();
        format!(
            "Model output limit truncated the `<lashlang>` block before `</lashlang>`.{cap} Retry with a shorter block; do less per block and continue in a later step."
        )
    }

    fn code_stream_kind(&self) -> &'static str {
        "lashlang_code"
    }

    fn execution_diagnostic_name(&self) -> &'static str {
        "execute_lashlang"
    }

    fn stream_cell_start_event_name(&self) -> &'static str {
        "rlm_lashlang_cell_start"
    }

    fn stream_cell_end_event_name(&self) -> &'static str {
        "rlm_lashlang_cell_end"
    }
}
