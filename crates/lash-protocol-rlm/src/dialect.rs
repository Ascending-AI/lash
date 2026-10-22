pub(crate) mod python;
pub(crate) mod typescript;

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::plugin::SessionPages;
use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext, SessionError};
use lash_kernel_doc::NumberPolicy;
use lash_rlm_types::RlmGlobalsPatchPluginBody;
use lash_tool_presenter_api::ToolPresenterSlot;
use lash_tool_shape::SchemaShape;
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;

pub use python::PythonPrompts;
pub use typescript::TypescriptPrompts;

use crate::executor::{CellServices, CodeModeExecutionState, execute_cell};
use crate::rlm_support::{BoundVariableRenderCache, render_bound_variables};

/// The language a code-mode session's model writes: an installed kernel
/// dialect package, by name, with the adapter that words prompts in it and
/// the tool presenter that shows its tools.
///
/// A cell is one kernel program. The package named here lowers its source,
/// in a worker, to a kernel document (`lash-kernel-dialect`); nothing in
/// this crate parses a cell. A session records the name at creation and
/// reads it back from its record on every later run, so it only ever runs
/// its cells in the dialect it was created in.
///
/// The tool presenter is the dialect's physical slot: a host replaces or
/// wraps it with [`Self::with_tool_presenter`]. A session records the
/// presenter's id and admitted config at creation, and a session reopened
/// on a host whose dialect holds another presenter is refused.
#[derive(Clone)]
pub struct CellDialect {
    name: Arc<str>,
    numbers: NumberPolicy,
    prompts: Arc<dyn DialectPrompts>,
    tool_presenter: ToolPresenterSlot,
}

impl CellDialect {
    /// The dialect package installed as `name`, whose documents read
    /// numbers under `numbers`, worded by `prompts`, its tools shown by
    /// `tool_presenter`.
    pub fn new(
        name: impl Into<Arc<str>>,
        numbers: NumberPolicy,
        prompts: Arc<dyn DialectPrompts>,
        tool_presenter: ToolPresenterSlot,
    ) -> Self {
        Self {
            name: name.into(),
            numbers,
            prompts,
            tool_presenter,
        }
    }

    /// TypeScript (`lash-dialect-typescript`): every number is a float. Its
    /// tools are shown by the TypeScript presenter.
    pub fn typescript() -> Self {
        Self::new(
            typescript::LANGUAGE_ID,
            NumberPolicy::Float,
            Arc::new(TypescriptPrompts),
            ToolPresenterSlot::new(lash_tool_presenter_typescript::TypescriptToolPresenter),
        )
    }

    /// Python (`lash-dialect-python`): a number is an integer or a float
    /// by its spelling. Its tools are shown by the Python presenter.
    pub fn python() -> Self {
        Self::new(
            python::LANGUAGE_ID,
            NumberPolicy::BySpelling,
            Arc::new(PythonPrompts),
            ToolPresenterSlot::new(lash_tool_presenter_python::PythonToolPresenter),
        )
    }

    /// This dialect with its tools shown by `tool_presenter`: a host's own
    /// presenter, or one wrapping the shipped one.
    #[must_use]
    pub fn with_tool_presenter(mut self, tool_presenter: ToolPresenterSlot) -> Self {
        self.tool_presenter = tool_presenter;
        self
    }

    /// The presenter that shows this dialect's tools.
    pub fn tool_presenter(&self) -> &ToolPresenterSlot {
        &self.tool_presenter
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
            .field("tool_presenter", &self.tool_presenter)
            .finish_non_exhaustive()
    }
}

/// How one dialect words what a model reads: tool paths, value types and
/// the prompt fragments that speak its syntax. It lowers nothing; the
/// dialect's kernel package does that in a worker. How its tools read is
/// its [`CellDialect`]'s tool presenter's.
pub trait DialectPrompts: Send + Sync + 'static {
    /// How this dialect spells a canonical name: the one spelling its
    /// lowering, printer and prompts share. A cell calls an operation by its
    /// package path and name in it ([`lash_kernel_dialect::spell_operation`]).
    fn spelling(&self) -> lash_kernel_dialect::Spelling;

    /// A schema shape as a type, in this dialect's syntax. Shared code calls
    /// it wherever a prompt shows the type of a value or of a history item.
    ///
    /// # Errors
    /// Refuses unbound references and shapes the dialect cannot represent.
    fn schema_type(
        &self,
        shape: &SchemaShape,
    ) -> Result<String, lash_tool_presenter_api::PresentationRefusal>;

    /// Defines a named inferred record using the same shape spelling as tools.
    ///
    /// # Errors
    /// Refuses unbound references, invalid names and unsupported shapes.
    fn schema_definition(
        &self,
        name: &str,
        shape: &SchemaShape,
    ) -> Result<String, lash_tool_presenter_api::PresentationRefusal>;

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
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DialectRefusalKind {
    /// The dialect's tool presenter refused to present the tool. The cause
    /// retains its typed fields and stable `PresentationRefusal::code()`.
    PresentationRefused(lash_tool_presenter_api::PresentationRefusal),
}

impl std::fmt::Display for DialectRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DialectRefusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            DialectRefusalKind::PresentationRefused(cause) => Some(cause),
        }
    }
}

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
    /// The callable tools' docs, as the session's tool presenter showed
    /// them; empty when there are none.
    pub tools: &'a str,
    /// The catalog the docs were rendered from.
    pub tool_catalog: &'a lash_core::ToolCatalog,
    /// What a discovery session lists and leaves to its discovery tool, for
    /// the dialect to label the listing complete or partial; `None` when the
    /// session has no discovery and every tool is listed.
    pub catalogue: Option<&'a crate::tool_catalog::ToolCatalogueDisclosure>,
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
    /// The complete config the session's tool presenter was admitted with.
    pub(crate) tool_presentation: serde_json::Value,
    pub(crate) workers: lash_vm_client::service::Service,
    pub(crate) code_renderer: crate::render::CodeRendererSlot,
    pub(crate) execution_bounds: crate::plugin::ExecutionBounds,
    /// The session-pinned transport programs arrive on. Carried with the
    /// services because the executor needs it to decide whether cell-delimiter
    /// advice is true of the source the model actually wrote (FIG-2769).
    pub(crate) channel: crate::plugin::RlmChannel,
    /// What a stored kernel definition is carried forward with.
    pub(crate) kernel: crate::executor::KernelCarry,
    /// Which package set a cell is lowered under; `None` is lash's own
    /// (FIG-6918).
    pub(crate) packages: Option<Arc<dyn crate::plugin::PackageGate>>,
}

/// The label under a discovery session's listed tools: whether the listing
/// is complete, and when it is not, how many tools are left out and the
/// call, in the dialect's spelling, that finds them (FIG-1435). It follows
/// the docs, so a tool appended to the catalogue leaves the docs before it
/// byte for byte as they were.
/// The operation named by the effect name `effect` as `spelling` writes
/// it; `effect` itself when it names no canonical operation.
pub(crate) fn spelled_effect(spelling: lash_kernel_dialect::Spelling, effect: &str) -> String {
    lash_kernel_dialect::operation_text::parse(effect).map_or_else(
        |_| effect.to_owned(),
        |operation| lash_kernel_dialect::spell_operation(spelling, &operation),
    )
}

pub(crate) fn catalogue_label(
    catalogue: &crate::tool_catalog::ToolCatalogueDisclosure,
    discovery_call: &str,
) -> String {
    let total = catalogue.total();
    if catalogue.is_complete() {
        format!("Catalogue: COMPLETE. All {total} tools are listed above.")
    } else {
        format!(
            "Catalogue: PARTIAL. {} of {total} tools are listed above; find the other {} with `{discovery_call}`.",
            catalogue.listed.len(),
            catalogue.omitted.len(),
        )
    }
}

/// Shared cell transport teaching; native transport replaces this whole section.
pub(crate) fn cell_response_shape(tags: CellTags) -> String {
    format!(
        "### Response shape\n\nPut one program after any commentary, between standalone `{open}` and `{close}` lines. Markdown fences do not execute. A line starting with `{close}` after whitespace ends the first program even inside a multiline string; keep that line out of string contents. Everything after that closing tag is discarded; wait for real results before continuing.\n",
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
    events: Arc<crate::projection::EventDecoder>,
    pub(crate) reserved_names: Vec<String>,
}

impl SessionDialect {
    /// The package set the session's next round is written under: the
    /// fleet gate's answer now, or lash's own for a host with no gate.
    /// An empty serving fleet durably installs and answers the writer's
    /// own set. This round's cell keeps the surface shown in its prompt;
    /// a later round asks again.
    ///
    /// # Errors
    ///
    /// The gate's typed `NoCompatibleSnapshot` for a nonempty incompatible
    /// fleet, or a retryable survey, installation or surface-load failure.
    pub(crate) async fn round_packages(&self) -> Result<crate::plugin::CellPackages, SessionError> {
        match &self.services.packages {
            Some(gate) => gate
                .writable()
                .await
                .map_err(|error| SessionError::Plugin(error.into())),
            None => lash_vm_runtime::lash_packages()
                .map_err(|error| SessionError::Protocol(error.to_string()))
                .map(|held| crate::plugin::CellPackages::held(&held)),
        }
    }

    /// The code exports of `packages`, the round's package surface, in the
    /// session's dialect's spelling (§2.3, P3): `None` when the set
    /// exports no code.
    pub(crate) fn package_surface(&self, packages: &crate::plugin::CellPackages) -> Option<String> {
        let surface = crate::package_surface::render(&packages.view(), self.language().spelling());
        (!surface.is_empty()).then_some(surface)
    }

    pub(crate) fn read_only_variables_prompt(
        &self,
        bindings: &crate::projection::CodeModeProjectedBindings,
    ) -> Result<Option<String>, lash_tool_presenter_api::PresentationRefusal> {
        crate::projection::read_only_variables_prompt(bindings, self.language())
    }

    pub(crate) fn new(dialect: CellDialect, services: CodeModeDialectServices) -> Self {
        Self {
            dialect,
            services,
            events: Arc::default(),
            reserved_names: Vec::new(),
        }
    }

    /// A session dialect that can render prompts and diagnostics but is
    /// given no host to execute against. The protocol driver needs one to
    /// answer questions about cells without an execution environment.
    pub(crate) fn prompt_only(dialect: CellDialect) -> Self {
        let tool_presentation = dialect
            .tool_presenter()
            .presenter()
            .admit_config(&serde_json::json!({}))
            .unwrap_or_default();
        Self {
            dialect,
            reserved_names: Vec::new(),
            events: Arc::default(),
            services: CodeModeDialectServices {
                workers: lash_vm_client::service::Service::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                code_renderer: Default::default(),
                channel: crate::plugin::RlmChannel::Cell,
                presentation: crate::RlmPresentationConfig::standard(),
                tool_presentation,
                kernel: crate::executor::KernelCarry::default(),
                packages: None,
            },
        }
    }

    /// This dialect presenting its tools under the admitted
    /// `tool_presentation`.
    pub(crate) fn with_tool_presentation(mut self, tool_presentation: serde_json::Value) -> Self {
        self.services.tool_presentation = tool_presentation;
        self
    }

    /// What a stored kernel definition is carried forward with.
    pub(crate) fn kernel(&self) -> &crate::executor::KernelCarry {
        &self.services.kernel
    }

    pub(crate) fn presentation(&self) -> crate::RlmPresentationConfig {
        self.services.presentation
    }

    /// The complete config the session's tool presenter was admitted with.
    pub(crate) fn tool_presentation(&self) -> &serde_json::Value {
        &self.services.tool_presentation
    }

    /// The presenter that shows the session's tools.
    pub(crate) fn tool_presenter(&self) -> &ToolPresenterSlot {
        self.dialect.tool_presenter()
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

    /// A catalog tool's call path in the selected dialect: its operation,
    /// spelled by the dialect.
    pub(crate) fn tool_call_path(&self, manifest: &lash_core::ToolManifest) -> String {
        lash_kernel_dialect::spell_operation(self.language().spelling(), &manifest.operation)
    }

    pub(crate) fn create_session(&self) -> DialectSession {
        let mut session = DialectSession::new(self.dialect.clone(), self.services.clone());
        session.events = Arc::clone(&self.events);
        session
    }

    pub(crate) fn decode_history_event(
        &self,
        event: &lash_core::ProtocolEvent,
    ) -> Result<
        Option<Arc<lash_protocol_types::allocation::Allocated<lash_rlm_types::RlmProtocolEvent>>>,
        lash_protocol_types::allocation::ProjectionError,
    > {
        self.events.decode(event)
    }

    pub(crate) fn history_index(
        &self,
        events: &[lash_core::SessionHistoryRecord],
        messages: &lash_core::facade_support::MessageSequence,
    ) -> Result<crate::projection::HistoryIndex, lash_protocol_types::allocation::ProjectionError>
    {
        crate::projection::HistoryIndex::from_turn(events, messages, &self.events)
    }

    pub(crate) fn projection_index(
        &self,
        projection: &lash_core::facade_support::ChronologicalProjection,
    ) -> Result<crate::projection::HistoryIndex, lash_protocol_types::allocation::ProjectionError>
    {
        crate::projection::HistoryIndex::from_projection(projection, &self.events)
    }

    /// The execution section as one text, prose then declarations.
    #[cfg(test)]
    pub(crate) fn render_execution_section(
        &self,
        features: crate::protocol::RlmPromptFeatures,
        tool_catalog: &lash_core::ToolCatalog,
        channel: crate::plugin::RlmChannel,
        discovery: Option<&crate::plugin::RlmDiscovery>,
    ) -> Result<String, SessionError> {
        self.execution_section(features, tool_catalog, channel, discovery)
            .map(|section| section.joined())
    }

    pub(crate) fn execution_section(
        &self,
        features: crate::protocol::RlmPromptFeatures,
        tool_catalog: &lash_core::ToolCatalog,
        channel: crate::plugin::RlmChannel,
        discovery: Option<&crate::plugin::RlmDiscovery>,
    ) -> Result<ExecutionSection, SessionError> {
        let listing = crate::tool_catalog::ToolListing::of(tool_catalog, self, features, discovery)
            .map_err(|refusal| {
                crate::tool_catalog::section_refused(
                    self.tool_presenter().presenter().id(),
                    refusal,
                )
            })?;
        Ok(self
            .language()
            .render_execution_section(ExecutionSectionRequest {
                channel,
                tools: &listing.docs,
                tool_catalog,
                catalogue: listing.disclosure.as_ref(),
            }))
    }

    /// The definition of one history item, shown where the prompt introduces
    /// the history collection: the dialect's spelling of the shape the item
    /// serializes as, which `lash-rlm-types` owns.
    pub(crate) fn history_item_definition(
        &self,
        images: bool,
    ) -> Result<String, lash_tool_presenter_api::PresentationRefusal> {
        self.language()
            .schema_definition(self.prompt_vocabulary().history_item_name, &{
                let document = lash_rlm_types::history_item_document(images);
                lash_tool_presenter_api::inline_shape(&document, &document.roots[0], 8)
            })
    }

    /// How a cell calls the finish tool `manifest`: Lash's `control.finish`
    /// as the dialect spells it, or a host's own by its call path. Its input
    /// is what the catalog documents.
    pub(crate) fn finish_call(&self, manifest: &lash_core::ToolManifest) -> String {
        if manifest.id.as_str() == crate::control_tools::FINISH_TOOL_ID {
            return self.prompt_vocabulary().finish_call.to_owned();
        }
        format!("await {}(…)", self.tool_call_path(manifest))
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
            "That reply opened a line with `{open}` in a position the {noun} grammar could not read, so nothing ran and no code was executed. The tag lines are what this depends on: `{open}` must stand alone on its own line with nothing else on it, the source goes on the lines after it, and `{close}` must start a later line after any whitespace. Everything after that close is discarded. To mention a tag in prose instead, wrap it in backticks.",
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

type BoundVariables = Result<
    (
        Arc<str>,
        crate::session_globals::SessionGlobals,
        Vec<String>,
    ),
    lash_tool_presenter_api::PresentationRefusal,
>;

pub(crate) struct BoundVariablesPromptRender {
    render: Box<dyn FnOnce() -> BoundVariables + Send>,
}

impl BoundVariablesPromptRender {
    pub(crate) fn new(render: impl FnOnce() -> BoundVariables + Send + 'static) -> Self {
        Self {
            render: Box::new(render),
        }
    }

    /// The listing's text and the globals it was rendered over.
    pub(crate) fn render(self) -> BoundVariables {
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
    events: Arc<crate::projection::EventDecoder>,
}

impl DialectSession {
    pub(crate) fn new(dialect: CellDialect, services: CodeModeDialectServices) -> Self {
        let state = CodeModeExecutionState::new(dialect.name(), dialect.numbers())
            .carrying(services.kernel.clone())
            .staging(services.workers.spools())
            .caching(services.workers.page_cache());
        Self {
            dialect,
            state,
            services,
            events: Arc::default(),
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
                events: Arc::clone(&self.events),
                workers: self.services.workers.clone(),
                execution_bounds: self.services.execution_bounds,
                channel: self.services.channel,
                code_renderer: self.services.code_renderer.clone(),
                prompts: self.dialect.prompts(),
                packages: self.services.packages.clone(),
                kernel: self.services.kernel.clone(),
            },
            session_projected_bindings,
        )
        .await;
        self.state.mark_code_execution_response_returned();
        Ok(response)
    }

    /// Takes `pages` as where the session's committed state stands, so a
    /// host read of it (the names a catalog check loads) reaches the
    /// store.
    pub(crate) fn note_pages(&mut self, pages: Option<&SessionPages>) {
        self.state.note_pages(pages);
    }

    pub(crate) fn execution_state_dirty(&self) -> bool {
        self.state.execution_state_dirty()
    }

    pub(crate) async fn capture_execution_graph(
        &mut self,
        purpose: lash_core_store::execution_state::ExecutionGraphPurpose,
    ) -> Result<lash_core_store::execution_state::ExecutionGraphCapture, SessionError> {
        self.state.capture_execution_graph(purpose).await
    }

    pub(crate) async fn execution_candidate(
        &mut self,
        root: lash_core_store::checkpoint_graph::NodeId,
        commit: lash_core_store::checkpoint_graph::CommitId,
        tx: &mut lash_durable::ActorTx,
    ) -> Result<Option<lash_core::plugin::ExecutionCandidate>, SessionError> {
        self.state.execution_candidate(root, commit, tx).await
    }

    pub(crate) async fn hydrated_execution_graph(
        &self,
    ) -> Result<lash_core::plugin::HydratedExecutionGraph, SessionError> {
        self.state.hydrated_execution_graph().await
    }

    pub(crate) async fn acknowledge_execution_state_capture(&mut self) -> Result<(), SessionError> {
        self.state.acknowledge_execution_state_capture().await;
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

    pub(crate) fn restore_execution_graph(
        &mut self,
        graph: &lash_core_store::execution_state::HydratedExecutionGraph,
        purpose: lash_core_store::execution_state::ExecutionGraphPurpose,
    ) -> Result<(), SessionError> {
        self.state
            .restore_execution_graph(graph, purpose)
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

    /// The bound-variable prompt and the globals it lists: the session's
    /// bindings, each a value a cell left, with the cell `provenance` names
    /// for it. A binding that was not carried (`K-SES-003`) is listed by
    /// name with why it is not bound, so the listing names every name an
    /// earlier cell wrote.
    /// Notes that the round's prompt shows `packages`: the round's cell is
    /// written under it.
    pub(crate) fn shows_packages(&mut self, packages: crate::plugin::CellPackages) {
        self.state.shows_packages(packages);
    }

    pub(crate) async fn prepare_bound_variables_prompt(
        &mut self,
        exclude: &BTreeSet<String>,
        params: lash_render::RenderParams,
        mut provenance: std::collections::BTreeMap<String, String>,
        pages: Option<&lash_core::plugin::SessionPages>,
    ) -> Result<BoundVariablesPromptRender, SessionError> {
        use crate::session_globals::{SessionGlobal, SessionGlobalKind};
        let values = self.state.bound_variable_values(exclude, pages).await?;
        let not_carried = self.state.not_carried();
        let kinds = values
            .into_iter()
            .chain(
                not_carried
                    .into_iter()
                    .filter(|(name, _)| !exclude.contains(name.as_str()))
                    .map(|(name, why)| (name.to_string(), SessionGlobalKind::NotCarried { why })),
            )
            // A saved function is listed with its signature and the cell
            // whose end froze what it reads.
            .chain(
                self.state
                    .held_functions()
                    .into_iter()
                    .filter(|(name, _, _)| !exclude.contains(name.as_str()))
                    .map(|(name, signature, frozen_at)| {
                        (
                            name,
                            SessionGlobalKind::Function {
                                signature,
                                frozen_at,
                            },
                        )
                    }),
            );
        let globals = kinds
            .map(|(name, kind)| SessionGlobal {
                cell: provenance.remove(&name),
                name,
                kind,
            })
            .collect::<Vec<_>>();
        let cache = Arc::clone(&self.bound_variable_render_cache);
        let renderer = self.services.code_renderer.clone();
        let prompts = self.dialect.prompts();
        let presentation = self.services.presentation;
        Ok(BoundVariablesPromptRender::new(move || {
            let mut cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (text, globals) = render_bound_variables(
                &mut cache,
                globals,
                prompts.as_ref(),
                renderer.0.as_ref(),
                &params,
                presentation.max_inline_keys,
                presentation.max_bound_variable_bytes,
            )?;
            Ok((text, globals, cache.reserved_names.clone()))
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

        fn spelling(&self) -> lash_kernel_dialect::Spelling {
            lash_kernel_dialect::Spelling::TypeScript
        }

        fn schema_type(
            &self,
            shape: &SchemaShape,
        ) -> Result<String, lash_tool_presenter_api::PresentationRefusal> {
            lash_tool_presenter_standard::compact_type(shape, &Default::default())
        }

        fn schema_definition(
            &self,
            name: &str,
            shape: &SchemaShape,
        ) -> Result<String, lash_tool_presenter_api::PresentationRefusal> {
            Ok(format!("shape {name} = {}", self.schema_type(shape)?))
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
            CellDialect::typescript().tool_presenter().clone(),
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
        let (prompt, _, _) = session
            .prepare_bound_variables_prompt(
                &BTreeSet::new(),
                lash_render::RenderParams::default(),
                Default::default(),
                None,
            )
            .await
            .expect("render the extension session's bindings")
            .render()
            .expect("presentable bindings");
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
        let presentation = crate::RlmPresentationConfig::standard();
        let (bound, _) = render_bound_variables(
            &mut BoundVariableRenderCache::default(),
            vec![crate::session_globals::SessionGlobal {
                name: "payload".to_string(),
                cell: None,
                kind: crate::session_globals::SessionGlobalKind::Data(value.clone()),
            }],
            &ExtensionFixture,
            &crate::render::BuiltinCodeRenderer,
            &lash_render::RenderParams::preview(),
            presentation.max_inline_keys,
            presentation.max_bound_variable_bytes,
        )
        .expect("presentable fixture");
        let read_only = crate::rlm_support::render_read_only_variables(
            vec![crate::rlm_support::ReadOnlyVariableDoc {
                name: "payload".to_string(),
                descriptor_type: "object".to_string(),
                value: Some(value),
            }],
            &ExtensionFixture,
        )
        .expect("presentable fixture");
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
            let response = Box::pin(session.execute(
                lash_core::testing::code_execution_context(handler.ports()),
                ExecRequest {
                    code: "const payload = `".to_string(),
                },
                crate::projection::CodeModeProjectedBindings::default(),
            ))
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
                    feedback.contains("line starting with `</fixture>`"),
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
            let response = Box::pin(session.execute(
                lash_core::testing::code_execution_context(handler.ports()),
                ExecRequest {
                    code: "const payload = `".to_string(),
                },
                crate::projection::CodeModeProjectedBindings::default(),
            ))
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
        assert!(
            cell.contains("line starting with `</typescript>`"),
            "{cell}"
        );
    }
}

#[cfg(test)]
pub(crate) fn test_dialect_services() -> CodeModeDialectServices {
    CodeModeDialectServices {
        kernel: crate::executor::KernelCarry::default(),
        presentation: crate::RlmPresentationConfig::standard(),
        tool_presentation: crate::dialect::CellDialect::typescript()
            .tool_presenter()
            .presenter()
            .admit_config(&serde_json::json!({}))
            .unwrap_or_default(),
        workers: lash_vm_client::service::Service::default(),
        execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
        code_renderer: Default::default(),
        channel: crate::plugin::RlmChannel::Cell,
        packages: None,
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
