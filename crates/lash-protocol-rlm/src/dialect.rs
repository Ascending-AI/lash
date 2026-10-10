pub(crate) mod python;
pub(crate) mod typescript;
mod typescript_types;

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext, SessionError};
use lash_kernel_doc::NumberPolicy;
use lash_rlm_types::RlmGlobalsPatchPluginBody;
use lash_sansio::{SchemaShape, ShapeRow};
use lash_vm_runtime::ResolvedToolBinding;

pub use python::PythonPrompts;
pub use typescript::TypescriptPrompts;

use crate::deferred::SharedDeferredToolResolver;
use crate::executor::{CellServices, CodeModeExecutionState, execute_cell};
use crate::rlm_support::{BoundVariableRenderCache, render_bound_variables};

/// The language a code-mode session's model writes: an installed kernel
/// dialect package, by name, with the adapter that words prompts in it.
///
/// A cell is one kernel program. The package named here lowers its source,
/// in a worker, to a kernel document (`lash-kernel-dialect`); nothing in
/// this crate parses a cell. A session records the name at creation and
/// reads it back from its record on every later run, so it only ever runs
/// its cells in the dialect it was created in.
#[derive(Clone)]
pub struct CellDialect {
    name: Arc<str>,
    numbers: NumberPolicy,
    prompts: Arc<dyn DialectPrompts>,
}

impl CellDialect {
    /// The dialect package installed as `name`, whose documents read
    /// numbers under `numbers`, worded by `prompts`.
    pub fn new(
        name: impl Into<Arc<str>>,
        numbers: NumberPolicy,
        prompts: Arc<dyn DialectPrompts>,
    ) -> Self {
        Self {
            name: name.into(),
            numbers,
            prompts,
        }
    }

    /// TypeScript (`lash-dialect-typescript`): every number is a float.
    pub fn typescript() -> Self {
        Self::new(
            typescript::LANGUAGE_ID,
            NumberPolicy::Float,
            Arc::new(TypescriptPrompts),
        )
    }

    /// Python (`lash-dialect-python`): a number is an integer or a float
    /// by its spelling.
    pub fn python() -> Self {
        Self::new(
            python::LANGUAGE_ID,
            NumberPolicy::BySpelling,
            Arc::new(PythonPrompts),
        )
    }

    /// The name the dialect package is installed under: what a session
    /// records, and what a worker is asked to lower with.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How a document of this dialect reads a number that came from JSON.
    pub fn numbers(&self) -> NumberPolicy {
        self.numbers
    }

    /// The adapter that words prompts and tool paths in this dialect.
    pub fn prompts(&self) -> Arc<dyn DialectPrompts> {
        Arc::clone(&self.prompts)
    }
}

impl std::fmt::Debug for CellDialect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellDialect")
            .field("name", &self.name)
            .field("numbers", &self.numbers)
            .finish_non_exhaustive()
    }
}

/// How one dialect words what a model reads: tool paths, signatures, types
/// and the prompt fragments that speak its syntax. It lowers nothing; the
/// dialect's kernel package does that in a worker.
pub trait DialectPrompts: Send + Sync + 'static {
    /// The call path a cell in this dialect writes to call a bound tool, or
    /// the refusal when no cell in this dialect can address it. The tool is
    /// offered to the cell as the effect of this name.
    fn tool_call_path(&self, binding: &ResolvedToolBinding) -> Result<String, DialectRefusal>;

    /// A catalog tool's callable signature, in this dialect's syntax. The
    /// shapes are the contract layer's reading of the tool's schemas; a
    /// dialect spells them and reads no JSON Schema itself.
    fn tool_signature(&self, call_path: &str, input: &SchemaShape, output: &SchemaShape) -> String;

    /// A schema shape as a type, in this dialect's syntax. Shared code calls
    /// it wherever a prompt shows the type of a field or of a required value.
    fn schema_type(&self, shape: &SchemaShape) -> String;

    /// Defines a named inferred record using the same shape spelling as tools.
    fn schema_definition(&self, name: &str, shape: &SchemaShape) -> String;

    /// A tool's authored example in this dialect's syntax, or `None` when the
    /// dialect cannot spell it; an example it cannot spell is left out of the
    /// prompt.
    fn render_tool_example(&self, authored: &str) -> Option<String>;

    /// The words, tags and notation shared prompt fragments read.
    fn prompt_vocabulary(&self) -> DialectPromptVocabulary;

    /// The execution section of the system prompt, in its two parts.
    fn render_execution_section(&self, request: ExecutionSectionRequest<'_>) -> ExecutionSection;
}

/// A dialect's execution section, split by who authored it (FIG-4588).
///
/// A session's prompt config may leave the built-in prose out; the
/// declarations describe what this session can call and render either way,
/// so a dialect never puts a tool, a host operation or a discovery hint in
/// its prose.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionSection {
    /// The dialect's built-in teaching: how to write a program, the response
    /// shape, the worked example and the host API every session has.
    pub prose: String,
    /// What this session's catalog and host generate: the tool declarations,
    /// grouped under each module's heading, and the host surface. Empty when
    /// the session has neither.
    pub declarations: String,
}

impl ExecutionSection {
    /// Both parts as one section, the prose first.
    pub fn joined(&self) -> String {
        [self.prose.trim(), self.declarations.trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// A dialect's refusal of something the host asked it to spell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialectRefusal {
    pub kind: DialectRefusalKind,
    pub message: String,
}

/// What a [`DialectRefusal`] refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DialectRefusalKind {
    /// No cell in the dialect can call the tool binding's path as a tool.
    UnaddressableToolPath,
}

impl std::fmt::Display for DialectRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DialectRefusal {}

/// The lines that open and close one cell on the cell channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellTags {
    pub open: &'static str,
    pub close: &'static str,
}

/// What shared code hands a dialect to render its execution section. The
/// dialect owns every word of the section.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct ExecutionSectionRequest<'a> {
    /// The transport programs arrive on.
    pub channel: crate::plugin::RlmChannel,
    /// The callable tools' docs, already rendered through this dialect's
    /// call paths, signatures and examples; empty when there are none.
    pub tools: &'a str,
    /// The catalog the docs were rendered from.
    pub tool_catalog: &'a lash_core::ToolCatalog,
    /// The tool that finds the tools not listed, when the host has one.
    pub discovery_operation: Option<&'a str>,
}

/// The words and call forms shared prompt fragments take from their dialect.
#[derive(Clone, Copy, Debug)]
pub struct DialectPromptVocabulary {
    /// How the prompt names the language in prose.
    pub language_name: &'static str,
    /// The heading of the prompt template's execution section.
    pub execution_title: &'static str,
    /// The cell channel's tag lines.
    pub cell_tags: CellTags,
    /// What the prompt calls one unit of code.
    pub cell_noun: &'static str,
    /// The history collection's type, as the dialect spells it in prompts.
    pub history_type: &'static str,
    /// The name `history_type` gives one item. Shared code defines it, through
    /// [`DialectPrompts::schema_definition`], from the shape a history item
    /// serializes as.
    pub history_item_name: &'static str,
    /// The call that shows a value for inspection.
    pub print_call: &'static str,
    /// The inspect form, ready to take a value expression.
    pub print_statement_prefix: &'static str,
    pub print_statement_suffix: &'static str,
    /// The `control.finish` control call, as a model would write it: the
    /// tool's real call path in this dialect.
    pub finish_call: &'static str,
    /// The continue-as control call, as a model would write it.
    pub continue_as_call: &'static str,
    /// A complete continue-as example for the tool doc.
    pub continue_as_example: &'static str,
    /// The rule the bound-variable listing states about field names, in the
    /// terms of what this dialect's runtime does with a missing field.
    pub field_miss_rule: &'static str,
    /// What to do about a binding an earlier cell left that was not carried
    /// because it held a function or a task (`K-SES-003`).
    pub not_carried_repair: &'static str,
    /// What to do about work a cell started and did not await before it
    /// ended (`K-TASK-018`).
    pub unjoined_task_repair: &'static str,
}

impl DialectPromptVocabulary {
    /// The inspect form for one expression.
    pub(crate) fn print_statement(&self, expression: &str) -> String {
        format!(
            "{}{expression}{}",
            self.print_statement_prefix, self.print_statement_suffix
        )
    }
}

/// Everything one execution session needs from the host that opened it:
/// workers, resolvers, bounds and transport. Source semantics belong to the
/// dialect's kernel package.
#[derive(Clone)]
pub(crate) struct CodeModeDialectServices {
    pub(crate) presentation: crate::RlmPresentationConfig,
    pub(crate) workers: lash_vm_client::service::Service,
    pub(crate) code_renderer: crate::render::CodeRendererSlot,
    pub(crate) deferred_tool_resolver: Option<SharedDeferredToolResolver>,
    pub(crate) execution_bounds: crate::plugin::ExecutionBounds,
    /// The session-pinned transport programs arrive on. Carried with the
    /// services because the executor needs it to decide whether cell-delimiter
    /// advice is true of the source the model actually wrote (FIG-2769).
    pub(crate) channel: crate::plugin::RlmChannel,
    /// What a stored kernel definition is carried forward with.
    pub(crate) kernel: crate::executor::KernelCarry,
    /// Which helper release a cell is lowered against; `None` is the
    /// build's own (FIG-5799).
    pub(crate) helpers: Option<Arc<dyn crate::plugin::HelperReleaseGate>>,
}

/// Shared cell transport teaching; native transport replaces this whole section.
pub(crate) fn cell_response_shape(tags: CellTags) -> String {
    format!(
        "### Response shape\n\nPut one program after any commentary, between standalone `{open}` and `{close}` lines. Markdown fences do not execute. A standalone `{close}` line ends the program even inside a multiline string; keep that line out of string contents.\n",
        open = tags.open,
        close = tags.close
    )
}

/// The session's dialect with the host resources it runs against: what
/// every protocol adapter of one session carries.
#[derive(Clone)]
pub(crate) struct SessionDialect {
    dialect: CellDialect,
    services: CodeModeDialectServices,
}

impl SessionDialect {
    pub(crate) fn read_only_variables_prompt(
        &self,
        bindings: &crate::projection::CodeModeProjectedBindings,
    ) -> Option<String> {
        crate::projection::read_only_variables_prompt(bindings, self.language())
    }

    pub(crate) fn new(dialect: CellDialect, services: CodeModeDialectServices) -> Self {
        Self { dialect, services }
    }

    /// A session dialect that can render prompts and diagnostics but is
    /// given no host to execute against. The protocol driver needs one to
    /// answer questions about cells without an execution environment.
    pub(crate) fn prompt_only(dialect: CellDialect) -> Self {
        Self {
            dialect,
            services: CodeModeDialectServices {
                workers: lash_vm_client::service::Service::default(),
                deferred_tool_resolver: None,
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                code_renderer: Default::default(),
                channel: crate::plugin::RlmChannel::Cell,
                presentation: crate::RlmPresentationConfig::standard(),
                kernel: crate::executor::KernelCarry::default(),
                helpers: None,
            },
        }
    }

    /// What a stored kernel definition is carried forward with.
    pub(crate) fn kernel(&self) -> &crate::executor::KernelCarry {
        &self.services.kernel
    }

    pub(crate) fn presentation(&self) -> crate::RlmPresentationConfig {
        self.services.presentation
    }

    pub(crate) fn renderer(&self) -> crate::render::CodeRendererSlot {
        self.services.code_renderer.clone()
    }

    /// The adapter that words this session's prompts.
    pub(crate) fn language(&self) -> &dyn DialectPrompts {
        self.dialect.prompts.as_ref()
    }

    /// The name of the session's dialect package.
    pub(crate) fn language_id(&self) -> &str {
        self.dialect.name()
    }

    pub(crate) fn prompt_vocabulary(&self) -> DialectPromptVocabulary {
        self.language().prompt_vocabulary()
    }

    pub(crate) fn cell_tags(&self) -> CellTags {
        self.prompt_vocabulary().cell_tags
    }

    /// A catalog tool's call path in the selected dialect: the manifest's
    /// neutral binding, spelled by the dialect.
    pub(crate) fn tool_call_path(
        &self,
        manifest: &lash_core::ToolManifest,
    ) -> Result<String, SessionError> {
        let binding = lash_vm_runtime::required_tool_executable(manifest)
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        self.language()
            .tool_call_path(&binding)
            .map_err(|refusal| SessionError::Protocol(refusal.to_string()))
    }

    pub(crate) fn render_tool_example(&self, example: &str) -> Option<String> {
        self.language().render_tool_example(example)
    }

    pub(crate) fn create_session(&self) -> DialectSession {
        DialectSession::new(self.dialect.clone(), self.services.clone())
    }

    /// The execution section as one text, prose then declarations.
    #[cfg(test)]
    pub(crate) fn render_execution_section(
        &self,
        features: crate::protocol::RlmPromptFeatures,
        tool_catalog: &lash_core::ToolCatalog,
        channel: crate::plugin::RlmChannel,
        discovery: Option<&lash_core::ToolDiscovery>,
    ) -> Result<String, SessionError> {
        self.execution_section(features, tool_catalog, channel, discovery)
            .map(|section| section.joined())
    }

    pub(crate) fn execution_section(
        &self,
        features: crate::protocol::RlmPromptFeatures,
        tool_catalog: &lash_core::ToolCatalog,
        channel: crate::plugin::RlmChannel,
        discovery: Option<&lash_core::ToolDiscovery>,
    ) -> Result<ExecutionSection, SessionError> {
        let tools = crate::tool_catalog::rlm_prompt_tool_docs(tool_catalog, self, features);
        Ok(self
            .language()
            .render_execution_section(ExecutionSectionRequest {
                channel,
                tools: &tools,
                tool_catalog,
                discovery_operation: discovery.map(|discovery| discovery.operation.as_str()),
            }))
    }

    /// The definition of one history item, shown where the prompt introduces
    /// the history collection: the dialect's spelling of the shape the item
    /// serializes as, which `lash-rlm-types` owns.
    pub(crate) fn history_item_definition(&self, images: bool) -> String {
        self.language().schema_definition(
            self.prompt_vocabulary().history_item_name,
            &lash_rlm_types::history_item_shape(images),
        )
    }

    /// The fields of `shape` that say more than their type — a description,
    /// a constraint or a default — one prompt row each. Every field's name
    /// and type is already in the signature the rows sit under.
    pub(crate) fn noted_field_rows(&self, shape: &SchemaShape) -> Vec<String> {
        shape
            .rows()
            .iter()
            .filter(|row| row.shape.has_notes())
            .map(|row| self.field_row(row))
            .collect()
    }

    fn field_row(&self, row: &ShapeRow) -> String {
        let mut line = format!(
            "- `{}{}: {}`",
            row.path,
            if row.required { "" } else { "?" },
            self.language().schema_type(&row.shape)
        );
        let mut notes = row.shape.constraints.notes();
        if let Some(default) = &row.shape.default {
            notes.push(format!("default {default}"));
        }
        if !notes.is_empty() {
            line.push_str(&format!(" ({})", notes.join(", ")));
        }
        if let Some(description) = &row.shape.description {
            line.push_str(&format!(" — {description}"));
        }
        line
    }

    /// How a cell calls the finish tool `manifest`: Lash's `control.finish`
    /// as the dialect spells it, or a host's own by its call path. Its input
    /// is what the catalog documents.
    pub(crate) fn finish_call(&self, manifest: &lash_core::ToolManifest) -> String {
        if manifest.id.as_str() == crate::control_tools::FINISH_TOOL_ID {
            return self.prompt_vocabulary().finish_call.to_owned();
        }
        match self.tool_call_path(manifest) {
            Ok(path) => format!("await {path}(…)"),
            Err(_) => manifest.name.clone(),
        }
    }

    /// How a cell calls the finish tool named `name`, when only its name is
    /// known: what a repair message reads from the synced surface.
    pub(crate) fn finish_call_named(&self, name: &str) -> String {
        if name == crate::control_tools::FINISH_TOOL_NAME {
            self.prompt_vocabulary().finish_call.to_owned()
        } else {
            name.to_owned()
        }
    }

    /// How the model calls each finish tool `ctx`'s synced surface offers:
    /// what a finish-required reminder names.
    pub(crate) fn offered_finish_calls(
        &self,
        ctx: &lash_core::DriverContextView<'_>,
    ) -> Vec<String> {
        ctx.finishing_tools()
            .map(|name| self.finish_call_named(name))
            .collect()
    }

    /// The finish calls `finishing` as prose: each in backticks, joined by
    /// "or". None reads as Lash's own.
    fn finish_calls(&self, finishing: &[String]) -> String {
        if finishing.is_empty() {
            return format!("`{}`", self.prompt_vocabulary().finish_call);
        }
        finishing
            .iter()
            .map(|call| format!("`{call}`"))
            .collect::<Vec<_>>()
            .join(" or ")
    }

    /// The finalization copy of a turn under `termination` whose surface
    /// offers the finish calls `finishing`.
    pub(crate) fn finalization_copy(
        &self,
        termination: lash_core::TerminationMode,
        finishing: &[String],
        channel: crate::plugin::RlmChannel,
    ) -> String {
        match termination {
            lash_core::TerminationMode::TerminalRequired => {
                self.finish_required_finalization(finishing, channel)
            }
            lash_core::TerminationMode::Natural => {
                let step = match channel {
                    crate::plugin::RlmChannel::Cell => "in a block",
                    crate::plugin::RlmChannel::NativeTool => "in an `execute_code` call",
                };
                let finish = self.finish_calls(finishing);
                // A model that reads "return a value" as "return what the
                // tool gave back" ends a chat turn with a record nobody can
                // read (FIG-5104), so the copy names prose as the answer and
                // a tool result as the thing the finish call never passes on.
                // A finish tool whose input is typed refuses any other value
                // as an ordinary failed call, which the catalog documents.
                format!(
                    "Natural termination: prose alone ends this turn as the final answer, so write prose only when no work remains; otherwise perform the next step {step}. Prefer prose for the final answer. Call {finish} inside the program only when the answer is a value the program built for this request, never to hand back a tool's raw result."
                )
            }
        }
    }

    pub(crate) fn cell_error_message(&self, error: crate::protocol::CellExtractionError) -> String {
        let tags = self.cell_tags();
        match error {
            crate::protocol::CellExtractionError::UnclosedCell => format!(
                "Model response started a `{open}` block but did not close it. Retry with one complete paired block. A line whose trimmed content is exactly `{close}` closes the cell.",
                open = tags.open,
                close = tags.close,
            ),
        }
    }

    pub(crate) fn finish_required_copy(
        &self,
        finishing: &[String],
        channel: crate::plugin::RlmChannel,
    ) -> String {
        let tags = self.prompt_vocabulary().cell_tags;
        let place = match channel {
            crate::plugin::RlmChannel::Cell => {
                format!("inside a paired `{}...{}` block", tags.open, tags.close)
            }
            crate::plugin::RlmChannel::NativeTool => {
                "inside the `code` argument of an `execute_code` call".to_string()
            }
        };
        let finish = self.finish_calls(finishing);
        format!(
            "Call {finish} {place} when the task is complete. It ends the turn: make it the last thing the program does."
        )
    }

    pub(crate) fn invalid_cell_retry_copy(&self, error_text: &str) -> String {
        let tags = self.cell_tags();
        format!(
            "{error_text}\n\nReply again using exactly one paired `{}...{}` block.",
            tags.open, tags.close
        )
    }

    /// What to tell a model whose reply carried a provider tool call on a
    /// channel that declared no tools.
    ///
    /// The call is malformed provider output, not a protocol violation — the
    /// request showed no tool surface — so the copy names what happened and
    /// sends the work back inside the cell (FIG-2777).
    pub(crate) fn native_tool_call_copy(&self, tool_name: &str) -> String {
        format!(
            "The model response carried a provider tool call `{tool_name}`, but this channel's request declares no tools, so nothing was executed. Express that work inside the program instead."
        )
    }

    pub(crate) fn output_limit_cell_copy(&self, output_token_cap: Option<usize>) -> String {
        let tags = self.cell_tags();
        let cap = output_token_cap
            .map(|cap| format!(" The request cap was {cap} tokens."))
            .unwrap_or_default();
        format!(
            "Model output truncated the `{}` block before `{}`.{cap} Retry with a shorter block.",
            tags.open, tags.close
        )
    }

    /// The diagnostic name of one program execution: derived from the
    /// session's language id, so it names the dialect that ran.
    pub(crate) fn execution_diagnostic_name(&self) -> String {
        format!("execute_{}", self.language_id())
    }

    pub(crate) fn stream_cell_start_event_name(&self) -> String {
        format!("rlm_{}_cell_start", self.language_id())
    }

    pub(crate) fn stream_cell_end_event_name(&self) -> String {
        format!("rlm_{}_cell_end", self.language_id())
    }

    pub(crate) fn render_history_cell(&self, prose: &str, code: &str) -> String {
        crate::cell_scan::render_cell_text(self.cell_tags(), prose, code)
    }

    pub(crate) fn finish_required_finalization(
        &self,
        finishing: &[String],
        channel: crate::plugin::RlmChannel,
    ) -> String {
        let tags = self.prompt_vocabulary().cell_tags;
        let finish = self.finish_calls(finishing);
        match channel {
            crate::plugin::RlmChannel::Cell => format!(
                "Finish-required: prose alone never ends this turn. Every response, including the last, acts inside a paired `{open}...{close}` block. Do not call {finish} until the answer is in hand; the final response's block calls it as its last statement. Never announce an action without the block that performs it.",
                open = tags.open,
                close = tags.close,
            ),
            crate::plugin::RlmChannel::NativeTool => format!(
                "Finish-required: prose alone never ends this turn. Every response, including the last, acts inside the `code` argument of an `execute_code` call. Do not call {finish} until the answer is in hand; the final response's `execute_code` call runs it as its last statement. Never announce an action without the `execute_code` call that performs it."
            ),
        }
    }

    /// What to tell a model that opened a line with the cell tag in a position
    /// the cell grammar refuses.
    ///
    /// The rule itself is the whole content, because the failure this replaces
    /// was a reply that got no rule at all: a misplaced fence was read as prose,
    /// the driver answered "please finish", and the model — correctly seeing
    /// nothing wrong with its own code — re-sent it until the turn's budget died
    /// (FIG-1475).
    ///
    /// It names the *canonical* shape only, and deliberately says nothing about
    /// the one-line shape the scanner also reads. Every prompt fragment teaches
    /// standalone tag lines; a correction that advertised a second accepted
    /// shape would contradict them, and this copy exists to remove a
    /// contradiction rather than add one. A reply already in the one-line shape
    /// never reaches this copy — it executes.
    pub(crate) fn malformed_cell_fence_retry_copy(&self) -> String {
        let vocabulary = self.prompt_vocabulary();
        let tags = vocabulary.cell_tags;
        format!(
            "That reply opened a line with `{open}` in a position the {noun} grammar could not read, so nothing ran and no code was executed. The tag lines are what this depends on: `{open}` must stand alone on its own line with nothing else on it, the source goes on the lines after it, and `{close}` must stand alone on a later line. To mention a tag in prose instead, wrap it in backticks.",
            noun = vocabulary.cell_noun,
            open = tags.open,
            close = tags.close,
        )
    }

    /// What to tell a native-tool-channel model that wrote a cell in its reply
    /// text. That channel runs code only through `execute_code` and its prompt
    /// teaches no cell tags, so a text cell is never executed there — well
    /// formed or not — and is never the answer either (FIG-5302).
    pub(crate) fn native_text_cell_copy(&self) -> String {
        format!(
            "No code executed: a `{open}` block in reply text is not executed in this session. Resend the program as the `code` argument of an `{tool}` call. To mention a tag in prose instead, wrap it in backticks.",
            open = self.cell_tags().open,
            tool = crate::native::NATIVE_EXECUTE_TOOL_NAME,
        )
    }
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

/// One code mode execution session: the session's bindings, and the cells that
/// run from them.
///
/// The state records the session's dialect, and a restore of a state
/// another dialect recorded is refused.
pub(crate) struct DialectSession {
    dialect: CellDialect,
    state: CodeModeExecutionState,
    services: CodeModeDialectServices,
    bound_variable_render_cache: Arc<std::sync::Mutex<BoundVariableRenderCache>>,
}

impl DialectSession {
    pub(crate) fn new(dialect: CellDialect, services: CodeModeDialectServices) -> Self {
        let state = CodeModeExecutionState::new(dialect.name(), dialect.numbers())
            .carrying(services.kernel.clone());
        Self {
            dialect,
            state,
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
        session_projected_bindings: crate::projection::CodeModeProjectedBindings,
    ) -> Result<ExecResponse, SessionError> {
        // The state is borrowed, never moved out: a cell that is cancelled
        // mid-flight leaves the session holding the same state it started
        // with, and every other caller waits behind the session's own lock.
        self.state
            .prepare_runtime_code_execution()
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        let response = execute_cell(
            &mut self.state,
            ctx,
            request,
            &CellServices {
                workers: self.services.workers.clone(),
                deferred_tool_resolver: self.services.deferred_tool_resolver.clone(),
                execution_bounds: self.services.execution_bounds,
                channel: self.services.channel,
                code_renderer: self.services.code_renderer.clone(),
                prompts: self.dialect.prompts(),
                helpers: self.services.helpers.clone(),
            },
            session_projected_bindings,
        )
        .await;
        self.state.mark_code_execution_response_returned();
        Ok(response)
    }

    pub(crate) fn session_names(&self) -> BTreeSet<String> {
        self.state
            .bindings()
            .names()
            .into_iter()
            .map(|name| name.to_string())
            .collect()
    }

    pub(crate) fn execution_state_dirty(&self) -> bool {
        self.state.execution_state_dirty()
    }

    pub(crate) async fn snapshot_execution_state(
        &mut self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<lash_core::plugin::ExecutionStateCapture, SessionError> {
        self.state.snapshot_execution_state(fleet_format).await
    }

    pub(crate) async fn probe_execution_state_capture(
        &mut self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), SessionError> {
        self.state.probe_execution_state_capture(fleet_format).await
    }

    pub(crate) async fn hydrated_execution_state(
        &self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<lash_core::plugin::HydratedExecutionState, SessionError> {
        self.state.hydrated_execution_state(fleet_format).await
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
            lash_core::plugin::CodeExecutionOutcome::Discarded => {
                self.state.rollback_code_execution();
            }
            lash_core::plugin::CodeExecutionOutcome::Cancelled
            | lash_core::plugin::CodeExecutionOutcome::Terminated => {
                self.state.terminate_code_execution();
            }
        }
        Ok(())
    }

    pub(crate) async fn restore_execution_state(
        &mut self,
        state: &lash_core::plugin::HydratedExecutionState,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), SessionError> {
        self.state
            .restore_execution_state(state, fleet_format)
            .await
            .map_err(SessionError::from)
    }

    pub(crate) async fn prune_protected_globals(
        &mut self,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        self.state.prune_protected_globals(protected_names).await?;
        Ok(())
    }

    pub(crate) async fn patch_globals(
        &mut self,
        patch: &RlmGlobalsPatchPluginBody,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        self.state.patch_globals(patch, protected_names).await
    }

    /// Holds the saved functions a session is created with.
    pub(crate) async fn seed_functions(
        &mut self,
        functions: &serde_json::Map<String, serde_json::Value>,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        self.state.seed_functions(functions, protected_names).await
    }

    /// The bound-variable prompt: the session's bindings, each a value a
    /// cell left. A binding that was not carried (`K-SES-003`) is listed by
    /// name with why it is not bound, so the listing names every name an
    /// earlier cell wrote.
    pub(crate) async fn prepare_bound_variables_prompt(
        &self,
        exclude: &BTreeSet<String>,
        params: lash_render::RenderParams,
    ) -> Result<BoundVariablesPromptRender, SessionError> {
        let globals = self.state.bound_variable_values(exclude);
        let noun = self.dialect.prompts.prompt_vocabulary().cell_noun;
        let not_carried = self
            .state
            .bindings()
            .not_carried()
            .iter()
            .filter(|(name, _)| !exclude.contains(name.as_str()))
            .map(|(name, why)| (name.to_string(), format!("not bound: {why}")))
            // A saved function is listed with its signature and the cell
            // whose end froze what it reads.
            .chain(
                self.state
                    .bindings()
                    .held_functions()
                    .iter()
                    .filter(|(name, _)| !exclude.contains(name.as_str()))
                    .map(|(name, held)| {
                        let signature = held
                            .function
                            .written
                            .as_ref()
                            .and_then(|written| written.signature.as_deref())
                            .unwrap_or("(...)");
                        let frozen = match held.cell {
                            Some(cell) => format!("captures frozen at {noun} {cell}"),
                            None => "captures frozen before this session".to_owned(),
                        };
                        (name.to_string(), format!("function {signature}; {frozen}"))
                    }),
            )
            .collect::<Vec<_>>();
        let cache = Arc::clone(&self.bound_variable_render_cache);
        let renderer = self.services.code_renderer.clone();
        let prompts = self.dialect.prompts();
        let max_inline_keys = self.services.presentation.max_inline_keys;
        Ok(BoundVariablesPromptRender::new(move || {
            let mut cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            render_bound_variables(
                &mut cache,
                &globals,
                &not_carried,
                prompts.as_ref(),
                renderer.0.as_ref(),
                &params,
                max_inline_keys,
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ExtensionFixture;

    impl DialectPrompts for ExtensionFixture {
        fn prompt_vocabulary(&self) -> DialectPromptVocabulary {
            DialectPromptVocabulary {
                language_name: "Extension fixture",
                cell_tags: CellTags {
                    open: "<fixture>",
                    close: "</fixture>",
                },
                history_type: "FixtureHistory",
                history_item_name: "FixtureHistoryItem",
                ..TypescriptPrompts.prompt_vocabulary()
            }
        }

        fn tool_call_path(&self, binding: &ResolvedToolBinding) -> Result<String, DialectRefusal> {
            Ok(binding.call_path())
        }

        fn tool_signature(
            &self,
            call_path: &str,
            _input: &SchemaShape,
            _output: &SchemaShape,
        ) -> String {
            call_path.to_string()
        }

        fn schema_type(&self, shape: &SchemaShape) -> String {
            shape.compact_type()
        }

        fn schema_definition(&self, name: &str, shape: &SchemaShape) -> String {
            format!("shape {name} = {}", self.schema_type(shape))
        }

        fn render_tool_example(&self, _authored: &str) -> Option<String> {
            None
        }

        fn render_execution_section(
            &self,
            _request: ExecutionSectionRequest<'_>,
        ) -> ExecutionSection {
            ExecutionSection::default()
        }
    }

    /// TypeScript cells worded by the fixture's prompts.
    fn extension_fixture() -> CellDialect {
        CellDialect::new(
            "typescript",
            NumberPolicy::Float,
            Arc::new(ExtensionFixture),
        )
    }

    #[tokio::test]
    async fn extension_session_bound_variables_use_its_vocabulary() {
        let mut session = DialectSession::new(extension_fixture(), test_dialect_services());
        session
            .patch_globals(
                &RlmGlobalsPatchPluginBody {
                    set_default: [("answer".to_string(), serde_json::json!(42))]
                        .into_iter()
                        .collect(),
                },
                &BTreeSet::new(),
            )
            .await
            .expect("bind a value to render through the session");
        let prompt = session
            .prepare_bound_variables_prompt(&BTreeSet::new(), lash_render::RenderParams::default())
            .await
            .expect("render the extension session's bindings")
            .render();
        assert!(prompt.contains("bound in Extension fixture"), "{prompt}");
        assert!(prompt.contains("`<fixture>`"), "{prompt}");
        assert!(!prompt.contains("TypeScript"), "{prompt}");
    }

    #[test]
    fn inferred_bound_and_read_only_shapes_use_the_dialect_schema_renderer() {
        let value = serde_json::json!({
            "body": "x".repeat(2_000),
            "empty": [],
            "numbers": [1, 1.5, null],
            "rows": [{"ok": true}, {"ok": null}],
        });
        let bound = render_bound_variables(
            &mut BoundVariableRenderCache::default(),
            &[("payload".to_string(), value.clone())],
            &[],
            &ExtensionFixture,
            &crate::render::BuiltinCodeRenderer,
            &lash_render::RenderParams::preview(),
            crate::RlmPresentationConfig::standard().max_inline_keys,
        );
        let read_only = crate::rlm_support::render_read_only_variables(
            vec![crate::rlm_support::ReadOnlyVariableDoc {
                name: "payload".to_string(),
                descriptor_type: "object".to_string(),
                value: Some(value),
            }],
            &ExtensionFixture,
        );
        for prompt in [&*bound, &*read_only] {
            assert!(
                prompt.contains("shape Payload = record{body: str,"),
                "{prompt}"
            );
            assert!(prompt.contains("numbers: list[float | null]"), "{prompt}");
            assert!(prompt.contains("empty: list[any]"), "{prompt}");
            assert!(!prompt.contains("body: string"), "{prompt}");
        }
        let schemas = [&*bound, &*read_only]
            .map(|prompt| {
                let row = prompt
                    .lines()
                    .find(|line| line.starts_with("- `payload`"))
                    .expect("binding row");
                let row = row.split(" ≈ ").next().expect("binding type");
                let schema = prompt
                    .split("Schema:\n")
                    .nth(1)
                    .expect("named schema")
                    .trim();
                format!("{row}\n{schema}")
            })
            .join("\n\n");
        insta::assert_snapshot!(schemas, @r###"
        - `payload`: `Payload`, keys=4 (body, empty, numbers, rows)
        shape RowItem = record{ok: bool | null}

        shape Payload = record{body: str, empty: list[any], numbers: list[float | null], rows: list[RowItem]}

        - `payload`: `Payload`, read-only (descriptor: `record`)
        shape RowItem = record{ok: bool | null}

        shape Payload = record{body: str, empty: list[any], numbers: list[float | null], rows: list[RowItem]}
        "###);
    }

    #[tokio::test]
    async fn extension_session_parse_feedback_uses_its_cell_delimiter() {
        for channel in [
            crate::plugin::RlmChannel::Cell,
            crate::plugin::RlmChannel::NativeTool,
        ] {
            let mut services = test_dialect_services();
            services.channel = channel;
            let mut session = DialectSession::new(extension_fixture(), services);
            let handler =
                crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
            let response = session
                .execute(
                    lash_core::testing::code_execution_context(handler.ports()),
                    ExecRequest {
                        code: "const payload = `".to_string(),
                    },
                    crate::projection::CodeModeProjectedBindings::default(),
                )
                .await
                .expect("report the extension's parse failure");
            let feedback = response
                .error()
                .expect("the fixture refuses the source")
                .message
                .clone();
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
            let mut session =
                DialectSession::new(crate::dialect::CellDialect::typescript(), services);
            let handler =
                crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
            let response = session
                .execute(
                    lash_core::testing::code_execution_context(handler.ports()),
                    ExecRequest {
                        code: "const payload = `".to_string(),
                    },
                    crate::projection::CodeModeProjectedBindings::default(),
                )
                .await
                .expect("the cell runs and reports its own failure");
            response
                .error()
                .expect("an unterminated template literal fails to parse")
                .message
                .clone()
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
pub(crate) fn test_dialect_services() -> CodeModeDialectServices {
    CodeModeDialectServices {
        kernel: crate::executor::KernelCarry::default(),
        presentation: crate::RlmPresentationConfig::standard(),
        workers: lash_vm_client::service::Service::default(),
        deferred_tool_resolver: None,
        execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
        code_renderer: Default::default(),
        channel: crate::plugin::RlmChannel::Cell,
        helpers: None,
    }
}

#[cfg(test)]
pub(crate) fn typescript_test_dialect() -> SessionDialect {
    SessionDialect::new(
        crate::dialect::CellDialect::typescript(),
        test_dialect_services(),
    )
}

#[cfg(test)]
#[path = "dialect/prompt_walker_tests.rs"]
mod prompt_walker_tests;
