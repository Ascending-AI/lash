pub(crate) mod typescript;

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext, SessionError};
use lash_lashlang_runtime::{
    LashlangArtifacts, SharedDeferredToolResolver, SharedDeferredTriggerResolver,
};
use lash_rlm_types::RlmGlobalsPatchPluginBody;

pub(crate) use typescript::{TypeScript, TypescriptDialect};

use crate::executor::{
    RlmExecutionState, execute_code_with_channel_and_bounds_with_trigger_resolver,
};
use crate::rlm_support::{BoundVariableRenderCache, render_bound_variables};

/// A dialect's refusal of a program, rendered against the source it refused.
#[derive(Clone, Debug)]
pub(crate) struct DialectDiagnostic {
    /// Whether the dialect refuses a construct
    /// ([`lash_core::CellFailureKind::Policy`]) or reports a wrong program
    /// ([`lash_core::CellFailureKind::Program`]).
    pub(crate) kind: lash_core::CellFailureKind,
    /// What the dialect refused, and nothing else.
    pub(crate) message: String,
    pub(crate) span: Option<lashlang::Span>,
    /// The refusal rendered against the source, with the line the model wrote.
    pub(crate) rendered: String,
}

/// The language a cell is written in, as the protocol layer reaches it.
///
/// Each dialect defines its own semantics and targets the IR: it lowers its
/// source to a Lashlang [`lashlang::Program`], explains its own refusals, and
/// spells the host's schemas in its own syntax. No module in this crate calls a
/// dialect's front end except that dialect's own implementation of this trait.
/// Shared session code also takes its prompt vocabulary and cell tags here.
/// Dialects target one IR and VM with independent semantics and evidence;
/// ADR 0096 records the prompt-adapter and registration work a new one needs.
pub(crate) trait Dialect: Send + Sync {
    /// The id the dialect names itself by.
    fn language_id(&self) -> &'static str;

    fn prompt_vocabulary(&self) -> DialectPromptVocabulary;

    fn cell_tags(&self) -> CellTags;

    /// Lowers `source` with no host: enough to read what a cell references
    /// before the host it links against is assembled.
    fn parse(&self, source: &str) -> Result<lashlang::Program, DialectDiagnostic>;

    /// Lowers one cell against the host it will link against, including the
    /// session globals the cell may read.
    fn parse_cell(
        &self,
        source: &str,
        host: &lashlang::LashlangHostEnvironment,
    ) -> Result<lashlang::Program, DialectDiagnostic>;

    /// A catalog tool's callable signature, in this dialect's syntax.
    fn tool_signature(
        &self,
        call_path: &str,
        input_schema: &serde_json::Value,
        output_schema: &serde_json::Value,
    ) -> String;

    /// Refuses a tool call path that no cell in this dialect can call as a
    /// tool.
    fn ensure_tool_call_path_addressable(&self, call_path: &str) -> Result<(), String>;
}

/// The current host's dialect. TypeScript is the only shipped one today;
/// future registration must select the front end consistently with its prompt
/// adapter and restored session state (ADR 0096).
pub(crate) fn rlm_dialect() -> &'static dyn Dialect {
    &TypeScript
}

/// Everything one execution session needs from the host that opened it.
///
/// These resources are shared by dialects targeting the IR and VM (ADR 0096).
/// Source semantics belong to `Dialect`; artifact storage, resolvers, trace
/// configuration, bounds and transport belong to the execution session.
#[derive(Clone)]
pub(crate) struct RlmDialectServices {
    pub(crate) code_renderer: crate::render::CodeRendererSlot,
    pub(crate) artifact_store: LashlangArtifacts,
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
/// The session runs its dialect over the Lashlang IR and VM, and seeds its
/// state's engine id from that dialect.
pub(crate) struct DialectSession {
    dialect: &'static dyn Dialect,
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
        dialect: &'static dyn Dialect,
        surface: lash_lashlang_runtime::LashlangSurface,
        services: RlmDialectServices,
    ) -> Self {
        let state = RlmExecutionState::for_engine(dialect.language_id());
        Self {
            dialect,
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
            self.dialect,
            &mut self.state,
            ctx,
            request,
            self.services.artifact_store.clone(),
            self.surface.clone(),
            self.services.deferred_tool_resolver.clone(),
            self.services.deferred_trigger_resolver.clone(),
            session_projected_bindings,
            self.services.execution_trace_config.clone(),
            self.services.execution_bounds.into_engine(),
            self.services.channel,
            self.services.code_renderer.clone(),
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
        fleet_format: lash_core::FleetFormat,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        self.state.snapshot_execution_state(fleet_format)
    }

    pub(crate) fn probe_execution_state_capture(
        &mut self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), SessionError> {
        self.state.probe_execution_state_capture(fleet_format)
    }

    pub(crate) fn hydrated_execution_state(
        &self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<lash_core::plugin::HydratedExecutionState, SessionError> {
        self.state.hydrated_execution_state(fleet_format)
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
        outcome: lash_core::plugin::CodeExecutionOutcome,
    ) -> Result<(), SessionError> {
        match outcome {
            lash_core::plugin::CodeExecutionOutcome::Accepted => {
                self.state.accept_code_execution();
            }
            lash_core::plugin::CodeExecutionOutcome::Discarded
            | lash_core::plugin::CodeExecutionOutcome::Cancelled => {
                self.state.cancel_code_execution();
            }
        }
        Ok(())
    }

    pub(crate) fn restore_execution_state(
        &mut self,
        state: &lash_core::plugin::HydratedExecutionState,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), SessionError> {
        self.state
            .restore_execution_state(state, fleet_format)
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
        params: lash_render::RenderParams,
    ) -> Result<BoundVariablesPromptRender, SessionError> {
        let globals = self.state.bound_variable_values(exclude);
        let opaque = self.state.opaque_bound_variables(exclude);
        let cache = Arc::clone(&self.bound_variable_render_cache);
        let renderer = self.services.code_renderer.clone();
        let vocabulary = self.dialect.prompt_vocabulary();
        Ok(BoundVariablesPromptRender::new(move || {
            let mut cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            render_bound_variables(
                &mut cache,
                &globals,
                &opaque,
                vocabulary,
                renderer.0.as_ref(),
                &params,
            )
        }))
    }
}

/// The words and call forms shared prompt fragments take from their dialect.
/// Session code passes the selected vocabulary explicitly. The prompt walker
/// and extension-session tests check that syntax matches the served front end.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DialectPromptVocabulary {
    /// How the prompt names the language in prose.
    pub(crate) language_name: &'static str,
    /// The heading of the prompt template's execution section.
    pub(crate) execution_title: &'static str,
    /// The opening cell tag, quoted in prose that points at cells.
    pub(crate) cell_open_tag: &'static str,
    /// What the prompt calls one unit of code.
    pub(crate) cell_noun: &'static str,
    /// The history collection's type, as the dialect spells it in prompts.
    pub(crate) history_type: &'static str,
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
}

impl Default for DialectPromptVocabulary {
    /// The current host's default. Shared session code reads its selected
    /// dialect's vocabulary explicitly.
    fn default() -> Self {
        crate::dialect::typescript::TYPESCRIPT_PROMPT_VOCABULARY
    }
}

impl DialectPromptVocabulary {
    /// `console.log(x)` for one expression.
    pub(crate) fn print_statement(&self, expression: &str) -> String {
        format!(
            "{}{expression}{}",
            self.print_statement_prefix, self.print_statement_suffix
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: u64 = 0x5_2c03;

    struct ExtensionFixture;

    impl Dialect for ExtensionFixture {
        fn language_id(&self) -> &'static str {
            "extension-fixture"
        }

        fn prompt_vocabulary(&self) -> DialectPromptVocabulary {
            DialectPromptVocabulary {
                language_name: "Extension fixture",
                cell_open_tag: "<fixture>",
                history_type: "FixtureHistory",
                ..typescript::TYPESCRIPT_PROMPT_VOCABULARY
            }
        }

        fn cell_tags(&self) -> CellTags {
            CellTags {
                open: "<fixture>",
                close: "</fixture>",
            }
        }

        fn parse(&self, _source: &str) -> Result<lashlang::Program, DialectDiagnostic> {
            Err(DialectDiagnostic {
                kind: lash_core::CellFailureKind::Program,
                message: "fixture syntax error".to_string(),
                span: None,
                rendered: "fixture syntax error".to_string(),
            })
        }

        fn parse_cell(
            &self,
            source: &str,
            _host: &lashlang::LashlangHostEnvironment,
        ) -> Result<lashlang::Program, DialectDiagnostic> {
            self.parse(source)
        }

        fn tool_signature(
            &self,
            call_path: &str,
            _input_schema: &serde_json::Value,
            _output_schema: &serde_json::Value,
        ) -> String {
            call_path.to_string()
        }

        fn ensure_tool_call_path_addressable(&self, _call_path: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn extension_session_bound_variables_use_its_vocabulary() {
        let mut session = DialectSession::new(
            &ExtensionFixture,
            lash_lashlang_runtime::LashlangSurface::default(),
            test_dialect_services(),
        );
        session
            .patch_globals(
                &RlmGlobalsPatchPluginBody {
                    set_default: [("answer".to_string(), serde_json::json!(42))]
                        .into_iter()
                        .collect(),
                },
                &BTreeSet::new(),
            )
            .expect("bind a value to render through the session");
        let prompt = session
            .prepare_bound_variables_prompt(&BTreeSet::new(), lash_render::RenderParams::default())
            .expect("render the extension session's bindings")
            .render();
        assert!(prompt.contains("bound in Extension fixture"), "{prompt}");
        assert!(prompt.contains("`<fixture>`"), "{prompt}");
        assert!(!prompt.contains("TypeScript"), "{prompt}");
    }

    #[tokio::test]
    async fn extension_session_parse_feedback_uses_its_cell_delimiter() {
        for channel in [
            crate::plugin::RlmChannel::Cell,
            crate::plugin::RlmChannel::NativeTool,
        ] {
            let mut services = test_dialect_services();
            services.channel = channel;
            let mut session = DialectSession::new(
                &ExtensionFixture,
                lash_lashlang_runtime::LashlangSurface::default(),
                services,
            );
            let double =
                crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default())
                    .await;
            let handler = double
                .open_handler(crate::testing::default_cell_scope())
                .await
                .expect("open the extension cell's handler");
            let response = session
                .execute(
                    lash_core::testing::code_execution_context(crate::testing::double_ports(
                        &double, &handler,
                    )),
                    ExecRequest {
                        code: "invalid fixture source".to_string(),
                    },
                    crate::projection::RlmProjectedBindings::default(),
                )
                .await
                .expect("report the extension's parse failure");
            handler
                .close()
                .await
                .expect("close the extension cell's handler");
            let feedback = response
                .error
                .expect("the fixture refuses the source")
                .message;
            assert!(feedback.contains("fixture syntax error"), "{feedback}");
            assert!(!feedback.contains("</typescript>"), "{feedback}");
            match channel {
                crate::plugin::RlmChannel::Cell => assert!(
                    feedback.contains("standalone `</fixture>` line"),
                    "{feedback}"
                ),
                crate::plugin::RlmChannel::NativeTool => assert!(
                    !feedback.contains("standalone delimiter line"),
                    "{feedback}"
                ),
            }
        }
    }

    /// The channel reaches the parse diagnostic through the production session
    /// seam, not just through the formatter's own argument: the session reads
    /// it off the services the plugin factory fills from the session-pinned
    /// config, so a native-channel session never sees cell-delimiter advice.
    #[tokio::test]
    async fn the_session_channel_decides_the_cell_delimiter_hint() {
        async fn parse_failure_feedback(channel: crate::plugin::RlmChannel) -> String {
            let mut services = test_dialect_services();
            services.channel = channel;
            let mut session = DialectSession::new(
                rlm_dialect(),
                lash_lashlang_runtime::LashlangSurface::default(),
                services,
            );
            let double =
                crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default())
                    .await;
            let handler = double
                .open_handler(crate::testing::default_cell_scope())
                .await
                .expect("open the cell's handler");
            let response = session
                .execute(
                    lash_core::testing::code_execution_context(crate::testing::double_ports(
                        &double, &handler,
                    )),
                    ExecRequest {
                        code: "const payload = `".to_string(),
                    },
                    crate::projection::RlmProjectedBindings::default(),
                )
                .await
                .expect("the cell runs and reports its own failure");
            handler.close().await.expect("close the cell's handler");
            response
                .error
                .expect("an unterminated template literal fails to parse")
                .message
        }

        let native = Box::pin(parse_failure_feedback(
            crate::plugin::RlmChannel::NativeTool,
        ))
        .await;
        assert!(!native.contains("</typescript>"), "{native}");

        let cell = Box::pin(parse_failure_feedback(crate::plugin::RlmChannel::Cell)).await;
        assert!(cell.contains("standalone `</typescript>` line"), "{cell}");
    }
}

#[cfg(test)]
pub(crate) fn test_dialect_services() -> RlmDialectServices {
    RlmDialectServices {
        artifact_store: crate::testing::memory_artifact_store_blocking(),
        deferred_tool_resolver: None,
        deferred_trigger_resolver: None,
        execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
        execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
        code_renderer: Default::default(),
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
