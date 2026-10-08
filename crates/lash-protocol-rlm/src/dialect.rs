pub(crate) mod typescript;

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext, SessionError};
use lash_lashlang_runtime::{
    LashlangArtifacts, LashlangHostEnvironment, LashlangSurface, ResolvedToolBinding,
    SharedDeferredToolResolver,
};
use lash_rlm_types::RlmGlobalsPatchPluginBody;
use lash_sansio::{SchemaShape, ShapeRow};

pub use typescript::TypescriptDialect;

use crate::executor::{RlmExecutionState, execute_code_with_channel_and_bounds};
use crate::rlm_support::{BoundVariableRenderCache, render_bound_variables};

/// The language a code-mode session's model writes: one front end over the
/// shared IR and VM, and every prompt adapter that speaks its syntax.
///
/// A host selects exactly one dialect per RLM protocol by passing it to
/// [`crate::RlmProtocolPluginFactory::new`] (or, for prompt-only use, to
/// [`crate::RlmDriver::new`] or [`crate::RlmProjectorConfig::new`]). There is no
/// default and no registry: the source frontend, tool-path spelling and
/// prompt fragments a session produces comes from that one value, and the session records its
/// [`Dialect::language_id`] so it only ever resumes under the same dialect
/// (ADR 0096).
///
/// A dialect selects the worker that lowers its source to the shared IR;
/// its host adapter spells prompts and tool paths. Shared code does
/// language-neutral work only.
pub trait Dialect: Send + Sync + 'static {
    /// The stable id the dialect names itself by. A session records it and
    /// refuses to resume under a dialect with another id.
    fn language_id(&self) -> &'static str;

    /// The worker entry and bounds for this dialect's source frontend.
    /// The selected frontend is compiled into that entry and parses only there.
    fn worker_service(&self) -> lash_vm_client::service::Service;

    /// Renders a typed source refusal returned by that worker.
    fn render_parse_diagnostic(&self, diagnostic: &lashlang::ModuleCompileError) -> String {
        diagnostic.to_string()
    }

    /// The call path a cell in this dialect writes to call a bound tool, or
    /// the refusal when no cell in this dialect can address it.
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
    /// The host a cell links against.
    pub host_environment: &'a LashlangHostEnvironment,
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
    /// [`Dialect::schema_definition`], from the shape a history item
    /// serializes as.
    pub history_item_name: &'static str,
    /// The call that shows a value for inspection.
    pub print_call: &'static str,
    /// The inspect form, ready to take a value expression.
    pub print_statement_prefix: &'static str,
    pub print_statement_suffix: &'static str,
    /// The name of the form that ends the turn with a value.
    pub finish_name: &'static str,
    /// The finish form as the prompt spells it in prose.
    pub finish_statement: &'static str,
    /// The finish form for an intentional null result.
    pub finish_null_statement: &'static str,
    /// The continue-as control call, as a model would write it.
    pub continue_as_call: &'static str,
    /// A complete continue-as example for the tool doc.
    pub continue_as_example: &'static str,
    /// The rule the bound-variable listing states about field names, in the
    /// terms of what this dialect's runtime does with a missing field.
    pub field_miss_rule: &'static str,
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

/// Everything one execution session needs from the host that opened it.
///
/// These resources are shared by dialects targeting the IR and VM (ADR 0096).
/// Source semantics belong to `Dialect`; artifact storage, resolvers, trace
/// configuration, bounds and transport belong to the execution session.
#[derive(Clone)]
pub(crate) struct RlmDialectServices {
    pub(crate) presentation: crate::RlmPresentationConfig,
    pub(crate) workers: lash_vm_client::service::Service,
    pub(crate) code_renderer: crate::render::CodeRendererSlot,
    pub(crate) artifact_store: LashlangArtifacts,
    pub(crate) deferred_tool_resolver: Option<SharedDeferredToolResolver>,
    pub(crate) execution_bounds: crate::plugin::ExecutionBounds,
    /// The session-pinned transport programs arrive on. Carried with the
    /// services because the executor needs it to decide whether cell-delimiter
    /// advice is true of the source the model actually wrote (FIG-2769).
    pub(crate) channel: crate::plugin::RlmChannel,
}

/// Shared cell transport teaching; native transport replaces this whole section.
pub(crate) fn cell_response_shape(tags: CellTags) -> String {
    format!(
        "### Response shape\n\nPut one program after any commentary, between standalone `{open}` and `{close}` lines. Markdown fences do not execute. A standalone `{close}` line ends the program even inside a multiline string; keep that line out of string contents.\n",
        open = tags.open,
        close = tags.close
    )
}

/// Whether a finish schema asks for text: the user-facing answer a chat turn
/// finishes with, rather than a structured value.
pub(crate) fn schema_is_text(schema: &lash_sansio::JsonSchema) -> bool {
    schema.as_value().get("type") == Some(&serde_json::Value::from("string"))
}

/// The session's selected dialect with the host resources it runs against:
/// what every protocol adapter of one session carries. It holds the host's
/// one selection; nothing here names a language.
#[derive(Clone)]
pub(crate) struct SessionDialect {
    dialect: Arc<dyn Dialect>,
    surface: LashlangSurface,
    services: RlmDialectServices,
}

impl SessionDialect {
    pub(crate) fn read_only_variables_prompt(
        &self,
        bindings: &crate::projection::RlmProjectedBindings,
    ) -> Option<String> {
        crate::projection::read_only_variables_prompt(bindings, self.dialect.as_ref())
    }

    pub(crate) fn new(
        dialect: Arc<dyn Dialect>,
        surface: LashlangSurface,
        services: RlmDialectServices,
    ) -> Self {
        Self {
            dialect,
            surface,
            services,
        }
    }

    /// A session dialect that can render prompts and diagnostics but cannot
    /// execute. The protocol driver needs one to answer questions about cells
    /// without an execution environment behind it.
    pub(crate) fn prompt_only(dialect: Arc<dyn Dialect>, surface: LashlangSurface) -> Self {
        let workers = dialect.worker_service();
        Self {
            dialect,
            surface,
            services: RlmDialectServices {
                workers,
                artifact_store: lashlang::LashlangArtifacts::new(Arc::new(PromptOnlyArtifactStore)),
                deferred_tool_resolver: None,

                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                code_renderer: Default::default(),
                channel: crate::plugin::RlmChannel::Cell,
                presentation: crate::RlmPresentationConfig::standard(),
            },
        }
    }

    pub(crate) fn presentation(&self) -> crate::RlmPresentationConfig {
        self.services.presentation
    }

    pub(crate) fn renderer(&self) -> crate::render::CodeRendererSlot {
        self.services.code_renderer.clone()
    }

    /// The lashlang host surface a cell of this session links against.
    pub(crate) fn surface(&self) -> LashlangSurface {
        self.surface.clone()
    }

    /// The selected front end.
    pub(crate) fn language(&self) -> &dyn Dialect {
        self.dialect.as_ref()
    }

    pub(crate) fn language_id(&self) -> &'static str {
        self.dialect.language_id()
    }

    pub(crate) fn prompt_vocabulary(&self) -> DialectPromptVocabulary {
        self.dialect.prompt_vocabulary()
    }

    pub(crate) fn cell_tags(&self) -> CellTags {
        self.prompt_vocabulary().cell_tags
    }

    pub(crate) fn worker_service(&self) -> lash_vm_client::service::Service {
        self.services.workers.clone()
    }

    /// A catalog tool's call path in the selected dialect: the manifest's
    /// neutral binding, spelled by the dialect.
    pub(crate) fn tool_call_path(
        &self,
        manifest: &lash_core::ToolManifest,
    ) -> Result<String, SessionError> {
        let binding = lash_lashlang_runtime::required_tool_executable(manifest)
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        self.dialect
            .tool_call_path(&binding)
            .map_err(|refusal| SessionError::Protocol(refusal.to_string()))
    }

    pub(crate) fn render_tool_example(&self, example: &str) -> Option<String> {
        self.dialect.render_tool_example(example)
    }

    pub(crate) fn create_session(&self) -> DialectSession {
        DialectSession::new(
            Arc::clone(&self.dialect),
            self.surface.clone(),
            self.services.clone(),
        )
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
        let host_environment = self
            .surface
            .host_environment(tool_catalog)
            .map_err(|error| {
                SessionError::Protocol(format!("invalid host tool surface: {error}"))
            })?;
        Ok(self
            .dialect
            .render_execution_section(ExecutionSectionRequest {
                channel,
                tools: &tools,
                tool_catalog,
                host_environment: &host_environment,
                discovery_operation: discovery.map(|discovery| discovery.operation.as_str()),
            }))
    }

    /// The definition of one history item, shown where the prompt introduces
    /// the history collection: the dialect's spelling of the shape the item
    /// serializes as, which `lash-rlm-types` owns.
    pub(crate) fn history_item_definition(&self, images: bool) -> String {
        self.dialect.schema_definition(
            self.dialect.prompt_vocabulary().history_item_name,
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
            self.dialect.schema_type(&row.shape)
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

    /// The value a turn's `finish` must carry, as its type and the rows of the
    /// fields that carry notes.
    pub(crate) fn required_output_contract(&self, schema: &serde_json::Value) -> String {
        let shape = SchemaShape::from_json_schema_with_depth(
            schema,
            self.services.presentation.tools.schema_depth,
        );
        let head = self.dialect.schema_type(&shape);
        let rows = self.noted_field_rows(&shape);
        if rows.is_empty() {
            head
        } else {
            format!("{head}\nFields:\n{}", rows.join("\n"))
        }
    }

    pub(crate) fn finalization_copy(
        &self,
        termination: &lash_rlm_types::RlmTermination,
        channel: crate::plugin::RlmChannel,
    ) -> String {
        match termination {
            lash_rlm_types::RlmTermination::FinishRequired { schema } => {
                self.finish_required_finalization(schema.is_some(), channel)
            }
            lash_rlm_types::RlmTermination::Natural { schema } => {
                let step = match channel {
                    crate::plugin::RlmChannel::Cell => "in a block",
                    crate::plugin::RlmChannel::NativeTool => "in an `execute_code` call",
                };
                let finish = self.prompt_vocabulary().finish_statement;
                // A model that reads "return a value" as "return what the
                // tool gave back" ends a chat turn with a record nobody can
                // read (FIG-5104), so every variant names prose as the answer
                // and a tool result as the thing `finish` never passes on.
                let finish_rule = match schema {
                    None => format!(
                        "Prefer prose for the final answer. Call `{finish}` inside the program only when the answer is a value the program built for this request, never to hand back a tool's raw result."
                    ),
                    Some(schema) if schema_is_text(schema) => format!(
                        "Prefer prose for the final answer. `{finish}` takes only the user-facing answer text as a string, never a raw tool result; any other value is refused and you must finish again."
                    ),
                    Some(_) => format!(
                        "Prefer prose for the final answer. `{finish}` takes only a value matching the REQUIRED OUTPUT contract, never a raw tool result; any other value is refused and you must finish again."
                    ),
                };
                format!(
                    "Natural termination: prose alone ends this turn as the final answer, so write prose only when no work remains; otherwise perform the next step {step}. {finish_rule}"
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
        requires_schema: bool,
        channel: crate::plugin::RlmChannel,
    ) -> String {
        let vocabulary = self.prompt_vocabulary();
        let tags = vocabulary.cell_tags;
        let place = match channel {
            crate::plugin::RlmChannel::Cell => {
                format!("inside a paired `{}...{}` block", tags.open, tags.close)
            }
            crate::plugin::RlmChannel::NativeTool => {
                "inside the `code` argument of an `execute_code` call".to_string()
            }
        };
        let finish = vocabulary.finish_statement;
        if requires_schema {
            format!(
                "Call `{finish}` {place} when the task is complete, with a value matching the required output schema."
            )
        } else {
            format!(
                "Call `{finish}` {place} when the task is complete. Use `{}` only when null is intentional.",
                vocabulary.finish_null_statement
            )
        }
    }

    pub(crate) fn finish_schema_mismatch_copy(&self) -> String {
        let vocabulary = self.prompt_vocabulary();
        format!(
            "The `{}` value did not match the required output schema. Correct it and call `{}` again.",
            vocabulary.finish_name, vocabulary.finish_statement
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
        requires_schema: bool,
        channel: crate::plugin::RlmChannel,
    ) -> String {
        let vocabulary = self.prompt_vocabulary();
        let tags = vocabulary.cell_tags;
        let mut text = match channel {
            crate::plugin::RlmChannel::Cell => format!(
                "Finish-required: prose alone never ends this turn. Every response, including the last, acts inside a paired `{open}...{close}` block. Do not call `{finish}` until the answer is in hand; the final response's block calls `{finish}` (`{finish_null}` only when null is the answer). Never announce an action without the block that performs it.",
                open = tags.open,
                close = tags.close,
                finish = vocabulary.finish_statement,
                finish_null = vocabulary.finish_null_statement,
            ),
            crate::plugin::RlmChannel::NativeTool => format!(
                "Finish-required: prose alone never ends this turn. Every response, including the last, acts inside the `code` argument of an `execute_code` call. Do not call `{finish}` until the answer is in hand; the final response's `execute_code` call runs `{finish}` (`{finish_null}` only when null is the answer). Never announce an action without the `execute_code` call that performs it.",
                finish = vocabulary.finish_statement,
                finish_null = vocabulary.finish_null_statement,
            ),
        };
        if requires_schema {
            text.push_str(" The value must match the REQUIRED OUTPUT contract.");
        }
        text
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

/// The artifact port of a [`SessionDialect::prompt_only`] session: it runs no
/// cell, so it has no backend, and every artifact operation is refused rather
/// than answered from a store no session reopens.
struct PromptOnlyArtifactStore;

impl PromptOnlyArtifactStore {
    fn refusal() -> lash_core::ArtifactStoreError {
        lash_core::ArtifactStoreError::Backend(
            "a prompt-only RLM dialect executes no cell and stores no Lashlang artifact"
                .to_string(),
        )
    }
}

#[async_trait::async_trait]
impl lash_core::ModuleArtifactStore for PromptOnlyArtifactStore {
    async fn publish_module_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _module_ref: &str,
        _bytes: &[u8],
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Err(Self::refusal())
    }

    async fn acquire_module_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _module_ref: &str,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Err(Self::refusal())
    }

    async fn end_module_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Err(Self::refusal())
    }

    async fn get_module_artifact(
        &self,
        _module_ref: &str,
    ) -> Result<Option<Vec<u8>>, lash_core::ArtifactStoreError> {
        Err(Self::refusal())
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

/// One RLM execution session.
///
/// The session runs its dialect over the Lashlang IR and VM, and seeds its
/// state's engine id from that dialect: the snapshot records the id, and a
/// restore under another dialect is refused.
pub(crate) struct DialectSession {
    dialect: Arc<dyn Dialect>,
    state: RlmExecutionState,
    surface: lash_lashlang_runtime::LashlangSurface,
    services: RlmDialectServices,
    bound_variable_render_cache: Arc<std::sync::Mutex<BoundVariableRenderCache>>,
}

impl DialectSession {
    pub(crate) fn new(
        dialect: Arc<dyn Dialect>,
        surface: lash_lashlang_runtime::LashlangSurface,
        services: RlmDialectServices,
    ) -> Self {
        let state = RlmExecutionState::for_engine_with_workers(
            dialect.language_id(),
            services.workers.clone(),
        );
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
        let response = execute_code_with_channel_and_bounds(
            self.dialect.as_ref(),
            &mut self.state,
            ctx,
            request,
            self.services.artifact_store.clone(),
            self.surface.clone(),
            self.services.deferred_tool_resolver.clone(),
            session_projected_bindings,
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

    /// The bound-variable prompt: the session's globals, including the ones
    /// with no host view, by summary. A front end's private
    /// slots never reach them (the VM drops every private binding at the end
    /// of its cell), so every global is a binding the model wrote.
    pub(crate) async fn prepare_bound_variables_prompt(
        &self,
        exclude: &BTreeSet<String>,
        params: lash_render::RenderParams,
    ) -> Result<BoundVariablesPromptRender, SessionError> {
        let globals = self.state.bound_variable_values(exclude);
        let opaque = self
            .state
            .opaque_bound_variables(exclude, &self.services.presentation.binding_summary)
            .await?;
        let cache = Arc::clone(&self.bound_variable_render_cache);
        let renderer = self.services.code_renderer.clone();
        let dialect = Arc::clone(&self.dialect);
        let max_inline_keys = self.services.presentation.max_inline_keys;
        Ok(BoundVariablesPromptRender::new(move || {
            let mut cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            render_bound_variables(
                &mut cache,
                &globals,
                &opaque,
                dialect.as_ref(),
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

    impl Dialect for ExtensionFixture {
        fn worker_service(&self) -> lash_vm_client::service::Service {
            TypescriptDialect.worker_service()
        }

        fn language_id(&self) -> &'static str {
            "extension-fixture"
        }

        fn prompt_vocabulary(&self) -> DialectPromptVocabulary {
            DialectPromptVocabulary {
                language_name: "Extension fixture",
                cell_tags: CellTags {
                    open: "<fixture>",
                    close: "</fixture>",
                },
                history_type: "FixtureHistory",
                history_item_name: "FixtureHistoryItem",
                ..TypescriptDialect.prompt_vocabulary()
            }
        }

        fn render_parse_diagnostic(&self, _diagnostic: &lashlang::ModuleCompileError) -> String {
            "fixture syntax error".to_string()
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

    #[tokio::test]
    async fn extension_session_bound_variables_use_its_vocabulary() {
        let mut session = DialectSession::new(
            Arc::new(ExtensionFixture),
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
            &[("payload".to_string(), lashlang::from_json(value.clone()))],
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
            let mut session = DialectSession::new(
                Arc::new(ExtensionFixture),
                lash_lashlang_runtime::LashlangSurface::default(),
                services,
            );
            let handler =
                crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
            let response = session
                .execute(
                    lash_core::testing::code_execution_context(handler.ports()),
                    ExecRequest {
                        code: "invalid fixture source".to_string(),
                    },
                    crate::projection::RlmProjectedBindings::default(),
                )
                .await
                .expect("report the extension's parse failure");
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
                Arc::new(TypescriptDialect),
                lash_lashlang_runtime::LashlangSurface::default(),
                services,
            );
            let handler =
                crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
            let response = session
                .execute(
                    lash_core::testing::code_execution_context(handler.ports()),
                    ExecRequest {
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
        presentation: crate::RlmPresentationConfig::standard(),
        workers: lash_vm_client::service::Service::default(),
        artifact_store: crate::testing::sqlite_memory_artifact_store_blocking(),
        deferred_tool_resolver: None,

        execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
        code_renderer: Default::default(),
        channel: crate::plugin::RlmChannel::Cell,
    }
}

#[cfg(test)]
pub(crate) fn typescript_test_dialect() -> SessionDialect {
    SessionDialect::new(
        Arc::new(TypescriptDialect),
        lash_lashlang_runtime::LashlangSurface::default(),
        test_dialect_services(),
    )
}

#[cfg(test)]
#[path = "dialect/prompt_walker_tests.rs"]
mod prompt_walker_tests;
