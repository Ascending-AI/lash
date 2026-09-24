pub(crate) mod typescript;

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext, SessionError};
use lash_lashlang_runtime::{
    LashlangArtifactStore, SharedDeferredToolResolver, SharedDeferredTriggerResolver,
};
use lash_rlm_types::RlmGlobalsPatchPluginBody;

pub(crate) use typescript::TypescriptDialect;

use crate::executor::{
    RlmExecutionState, execute_code_with_channel_and_bounds_with_trigger_resolver,
};
use crate::projection::ProjectionResolver;
use crate::rlm_support::{BoundVariableRenderCache, render_bound_variables};

/// Everything one execution session needs from the host that opened it.
///
/// The RLM protocol serves one language (ADR 0096), so these are the session's
/// services rather than a dialect's: what varies between two sessions is the
/// artifact store, the resolvers, the trace configuration and the transport,
/// never the language.
#[derive(Clone)]
pub(crate) struct RlmDialectServices {
    pub(crate) projection_resolver: Arc<dyn ProjectionResolver>,
    pub(crate) artifact_store: Arc<dyn LashlangArtifactStore>,
    pub(crate) deferred_tool_resolver: Option<SharedDeferredToolResolver>,
    pub(crate) deferred_trigger_resolver: Option<SharedDeferredTriggerResolver>,
    pub(crate) execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig,
    pub(crate) execution_bounds: crate::plugin::ExecutionBounds,
    /// The session-pinned transport programs arrive on. Carried with the
    /// services because the executor needs it to decide whether cell-delimiter
    /// advice is true of the source the model actually wrote (FIG-2769).
    pub(crate) channel: crate::plugin::RlmChannel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CellTags {
    pub(crate) open: &'static str,
    pub(crate) close: &'static str,
}

/// Shared cell transport teaching; native transport replaces this whole section.
pub(crate) fn cell_response_shape(tags: CellTags) -> String {
    format!(
        "### Response shape\n\nPut one program after any commentary, between standalone `{open}` and `{close}` lines. Markdown fences do not execute. A standalone `{close}` line ends the program even inside a multiline string; keep that line out of string contents.\n",
        open = tags.open,
        close = tags.close
    )
}

pub(crate) struct BoundVariablesPromptRender {
    render: Box<dyn FnOnce() -> Arc<str> + Send>,
}

impl BoundVariablesPromptRender {
    pub(crate) fn new(render: impl FnOnce() -> Arc<str> + Send + 'static) -> Self {
        Self {
            render: Box::new(render),
        }
    }

    pub(crate) fn render(self) -> Arc<str> {
        (self.render)()
    }
}

/// One RLM execution session.
///
/// The session runs the TypeScript surface over the Lashlang IR and VM: the
/// engine id it seeds its state from, the vocabulary its bound-variable prompt
/// is written in and the lowering prefix that prompt hides are all facts of
/// that one language, so none of them is a parameter any more.
pub(crate) struct DialectSession {
    state: RlmExecutionState,
    surface: lash_lashlang_runtime::LashlangSurface,
    services: RlmDialectServices,
    bound_variable_render_cache: Arc<std::sync::Mutex<BoundVariableRenderCache>>,
}

impl DialectSession {
    /// The id is durability identity: it is written into persisted state that a
    /// later process reads back, which is why it stays a named constant rather
    /// than a spelling each call site repeats.
    pub(crate) fn new(
        surface: lash_lashlang_runtime::LashlangSurface,
        services: RlmDialectServices,
    ) -> Self {
        let state = RlmExecutionState::for_engine(typescript::LANGUAGE_ID);
        Self {
            state,
            surface,
            services,
            bound_variable_render_cache: Arc::new(std::sync::Mutex::new(
                BoundVariableRenderCache::default(),
            )),
        }
    }

    pub(crate) async fn execute(
        &mut self,
        ctx: RuntimeExecutionContext<'_>,
        request: ExecRequest,
        session_projected_bindings: crate::projection::RlmProjectedBindings,
    ) -> Result<ExecResponse, SessionError> {
        // The state is borrowed, never moved out: a cell that is cancelled
        // mid-flight leaves the session holding the same state it started
        // with, and every other caller waits behind the session's own lock.
        self.state
            .prepare_runtime_code_execution()
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        let response = execute_code_with_channel_and_bounds_with_trigger_resolver(
            &mut self.state,
            ctx,
            request,
            Arc::clone(&self.services.artifact_store),
            self.surface.clone(),
            self.services.deferred_tool_resolver.clone(),
            self.services.deferred_trigger_resolver.clone(),
            session_projected_bindings,
            Arc::clone(&self.services.projection_resolver),
            self.services.execution_trace_config.clone(),
            self.services.execution_bounds.into_engine(),
            self.services.channel,
        )
        .await;
        self.state.mark_code_execution_response_returned();
        Ok(response)
    }

    pub(crate) fn execution_state_dirty(&self) -> bool {
        self.state.execution_state_dirty()
    }

    pub(crate) fn snapshot_execution_state(
        &mut self,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        self.state.snapshot_execution_state()
    }

    pub(crate) fn probe_execution_state_capture(&mut self) -> Result<(), SessionError> {
        self.state.probe_execution_state_capture()
    }

    pub(crate) fn hydrated_execution_state(
        &self,
    ) -> Result<lash_core::plugin::HydratedExecutionState, SessionError> {
        self.state.hydrated_execution_state()
    }

    pub(crate) fn acknowledge_execution_state_capture(&mut self) -> Result<(), SessionError> {
        self.state.acknowledge_execution_state_capture();
        Ok(())
    }

    pub(crate) fn abort_execution_state_capture(&mut self) -> Result<(), SessionError> {
        self.state.abort_execution_state_capture();
        Ok(())
    }

    pub(crate) fn settle_code_execution(
        &mut self,
        disposition: lash_core::plugin::CodeExecutionDisposition,
    ) -> Result<(), SessionError> {
        match disposition {
            lash_core::plugin::CodeExecutionDisposition::Accepted => {
                self.state.accept_code_execution();
            }
            lash_core::plugin::CodeExecutionDisposition::Discarded
            | lash_core::plugin::CodeExecutionDisposition::Cancelled => {
                self.state.cancel_code_execution();
            }
        }
        Ok(())
    }

    pub(crate) fn restore_execution_state(
        &mut self,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError> {
        self.state
            .restore_execution_state(state)
            .map_err(|error| SessionError::Protocol(error.to_string()))
    }

    pub(crate) fn prune_protected_globals(
        &mut self,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        self.state.prune_protected_globals(protected_names);
        Ok(())
    }

    pub(crate) fn patch_globals(
        &mut self,
        patch: &RlmGlobalsPatchPluginBody,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        self.state.patch_globals(patch, protected_names)
    }

    /// The bound-variable prompt: the session's globals, including the ones
    /// with no host view, by summary. A front end's private
    /// slots never reach them (the VM drops every private binding at the end
    /// of its cell), so every global is a binding the model wrote.
    pub(crate) fn prepare_bound_variables_prompt(
        &self,
        exclude: &BTreeSet<String>,
    ) -> Result<BoundVariablesPromptRender, SessionError> {
        let globals = self.state.bound_variable_values(exclude);
        let opaque = self.state.opaque_bound_variables(exclude);
        let cache = Arc::clone(&self.bound_variable_render_cache);
        Ok(BoundVariablesPromptRender::new(move || {
            let mut cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            render_bound_variables(
                &mut cache,
                &globals,
                &opaque,
                DialectPromptVocabulary::default(),
            )
        }))
    }
}

/// The words and call forms every shared prompt fragment needs.
///
/// Prompt copy used to be language-aware only where it was obviously a *cell* —
/// the execution section, the retry copy, the finalization copy. Everything
/// else assembled around those (bound variables, read-only variables, tool
/// docs, budget escalation, the final-answer instruction) hardcoded the retired
/// surface's syntax, so a TypeScript session was told, in the same prompt, to
/// write `<typescript>` cells and that its variables were "bound in lashlang".
/// A model cannot follow both; the judged battery caught one spending reasoning
/// tokens reconciling the contradiction.
///
/// One struct rather than a dozen scattered literals, so a new fragment has an
/// obvious place to read its words from and
/// `no_cross_dialect_text_in_the_assembled_prompt` has one source of truth to
/// check against.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DialectPromptVocabulary {
    /// How the prompt names the language in prose.
    pub(crate) language_name: &'static str,
    /// The opening cell tag, quoted in prose that points at cells.
    pub(crate) cell_open_tag: &'static str,
    /// What the prompt calls one unit of code.
    pub(crate) cell_noun: &'static str,
    /// The call that prints a value for inspection.
    pub(crate) print_call: &'static str,
    /// `console.log(x)`, ready to take a value expression.
    pub(crate) print_statement_prefix: &'static str,
    pub(crate) print_statement_suffix: &'static str,
    /// The finish form as the prompt spells it in prose.
    pub(crate) finish_statement: &'static str,
    /// The finish form for an intentional null result.
    pub(crate) finish_null_statement: &'static str,
    /// The continue-as control call, as a model would write it.
    pub(crate) continue_as_call: &'static str,
    /// A complete continue-as example for the tool doc.
    pub(crate) continue_as_example: &'static str,
    /// How the language tells a model to describe a *nested* typed shape, as a
    /// clause that continues a sentence about flat string descriptors — empty
    /// when it has no way to write one.
    pub(crate) type_literal_hint: &'static str,
}

impl Default for DialectPromptVocabulary {
    /// TypeScript's words, because they are the only ones a session can be
    /// served (ADR 0096).
    fn default() -> Self {
        crate::dialect::typescript::TYPESCRIPT_PROMPT_VOCABULARY
    }
}

/// One authored token and the vocabulary field that answers it.
pub(crate) type ToolProseToken = (&'static str, fn(DialectPromptVocabulary) -> &'static str);

/// The tokens a host or plugin may write in model-facing tool prose so the
/// prompt's own vocabulary spells the language-specific part.
///
/// Tool descriptions and JSON-Schema `description` strings are authored once,
/// in the crate that owns the tool. A language word written literally there is
/// a leak no renderer can undo — which is how three `lashlang` strings reached
/// TypeScript sessions through `agents.spawn` and `processes.list`. Anything
/// the language owns is spelled by the vocabulary: prose that needs such a word
/// writes the token, [`rlm_prompt_tool_docs`](crate::tool_catalog) resolves it,
/// and [`crate::tool_catalog::validate_dialect_neutral_tool_prose`] refuses
/// registration for the literal spelling.
///
/// One table, read by both the renderer and the guard, so a token can neither
/// be resolved without being accepted nor accepted without being resolved.
pub(crate) const TOOL_PROSE_TOKENS: &[ToolProseToken] =
    &[("{{type_literal_hint}}", |vocabulary| {
        vocabulary.type_literal_hint
    })];

impl DialectPromptVocabulary {
    pub(crate) fn render_tool_prose(&self, text: &str) -> String {
        let mut text = text.to_string();
        for (token, resolve) in TOOL_PROSE_TOKENS {
            if text.contains(token) {
                text = text.replace(token, resolve(*self));
            }
        }
        text
    }

    /// `console.log(x)` for one expression.
    pub(crate) fn print_statement(&self, expression: &str) -> String {
        format!(
            "{}{expression}{}",
            self.print_statement_prefix, self.print_statement_suffix
        )
    }
}

/// The words that identify the RLM language wherever they appear, lowercased.
///
/// Read from the dialect itself rather than listed, so a rename extends the
/// tool-prose guard by construction. Deliberately narrow: the language's own
/// name, its cell tags and its finish form are unmistakable, while `print_call`
/// ("console.log") would fire on any tool that talks about logging. A word this
/// list omits is a leak the guard cannot see, not a leak it permits.
pub(crate) fn dialect_identity_markers(dialect: &TypescriptDialect) -> Vec<String> {
    let vocabulary = dialect.prompt_vocabulary();
    let tags = dialect.cell_tags();
    let mut markers = vec![
        dialect.language_id().to_lowercase(),
        vocabulary.language_name.to_lowercase(),
        tags.open.to_lowercase(),
        tags.close.to_lowercase(),
        vocabulary.finish_statement.to_lowercase(),
    ];
    markers.sort();
    markers.dedup();
    markers
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The channel reaches the parse diagnostic through the production session
    /// seam, not just through the formatter's own argument: the session reads
    /// it off the services the plugin factory fills from the session-pinned
    /// config, so a native-channel session never sees cell-delimiter advice.
    #[tokio::test]
    async fn the_session_channel_decides_the_cell_delimiter_hint() {
        async fn parse_failure_feedback(channel: crate::plugin::RlmChannel) -> String {
            let mut services = test_dialect_services();
            services.channel = channel;
            let mut session =
                DialectSession::new(lash_lashlang_runtime::LashlangSurface::default(), services);
            let response = session
                .execute(
                    lash_core::testing::code_execution_context(
                        crate::testing::memory_backend_ports().await,
                    ),
                    ExecRequest {
                        language: "typescript".to_string(),
                        code: "const payload = `".to_string(),
                    },
                    crate::projection::RlmProjectedBindings::default(),
                )
                .await
                .expect("the cell runs and reports its own failure");
            response
                .error
                .expect("an unterminated template literal fails to parse")
                .message
        }

        let native = parse_failure_feedback(crate::plugin::RlmChannel::NativeTool).await;
        assert!(!native.contains("</typescript>"), "{native}");

        let cell = parse_failure_feedback(crate::plugin::RlmChannel::Cell).await;
        assert!(cell.contains("standalone `</typescript>` line"), "{cell}");
    }
}

#[cfg(test)]
pub(crate) fn test_dialect_services() -> RlmDialectServices {
    RlmDialectServices {
        projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
        artifact_store: ::lashlang::global_in_memory_lashlang_artifact_store(),
        deferred_tool_resolver: None,
        deferred_trigger_resolver: None,
        execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
        execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
        channel: crate::plugin::RlmChannel::Cell,
    }
}

#[cfg(test)]
pub(crate) fn typescript_test_dialect() -> TypescriptDialect {
    TypescriptDialect::new(
        lash_lashlang_runtime::LashlangSurface::default(),
        test_dialect_services(),
    )
}

#[cfg(test)]
#[path = "dialect/prompt_walker_tests.rs"]
mod prompt_walker_tests;
