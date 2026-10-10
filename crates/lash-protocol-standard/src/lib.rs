//! Standard protocol stack: the model executes tools via the native
//! function-calling envelope of its LLM transport.
//!
//! This crate owns:
//!
//! - [`StandardDriver`] — the [`ProtocolDriverHandle`] that dispatches
//!   native tool calls and weaves reasoning parts into the assistant
//!   message timeline.
//! - The [`StandardProtocolPluginFactory`] plugin that claims the
//!   protocol-driver slot so the runtime can run standard-protocol
//!   sessions.
//! - The `batch` protocol sugar: the driver expands each `batch` call into
//!   the step's one tool group beside the response's native calls, and folds
//!   the members' results back into one batch result (ADR 0116 §2).
//! - The step's one control call ([`round`]): reserved in response order,
//!   held behind the step's other calls, and refused when one of them
//!   failed.

use lash_sansio::TurnId;
use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use std::sync::Arc;

use async_trait::async_trait;
use lash_core::facade_support::JsonSchema;
use lash_core::llm::types::{ProviderReasoningReplay, ProviderReplayMeta, ResponseTextMeta};
use lash_core::plugin::{
    CandidateFacts, ConfigCommand, ConfigOwner, ConfigRegistrar, ConfigRegistrationError,
    OwnerChange, PluginError, PluginFactory, PluginRegistrar, PluginSessionContext,
    ProtocolDriverPlugin, ProtocolSessionContext, ProtocolSessionPlugin, SessionPlugin,
};
use lash_core::sansio::{
    CheckpointResumeAction, CompletedToolCall, PendingToolCall, PendingWork, ProtocolDriverHandle,
};
#[cfg(test)]
use lash_core::session_model::PartKind;
use lash_core::session_model::{
    ConversationRecord, Message, MessageRole, Part, SessionHistoryRecord, SessionStreamEvent,
    reassign_part_ids, shared_parts,
};

mod batch;
mod finish;
mod prompt;
mod round;
pub use prompt::section_keys;
pub mod render;
pub use batch::BatchResultRow;
pub use render::{
    BuiltinToolOutputRenderer, StandardRenderConfig, ToolOutputRenderer, ToolOutputRendererSlot,
    ToolRenderParams,
};
pub mod scenario_contracts;
use batch::batch_tool_definition;
use lash_core::{
    CheckpointKind, DriverAction, DriverContextView, LlmOutputPart, LlmResponse,
    ProtocolBuildInput, SessionError, TurnDriverConfig, TurnDriverPreamble,
    facade_support::TurnFinish, facade_support::TurnOutcome, facade_support::TurnStop,
    facade_support::normalized_response_parts, facade_support::reasoning_part,
};
use serde_json::Value;

/// The standard protocol plugin's id: the key of its creation options in a
/// session spec's plugin options, and the owner of its config commands.
pub const STANDARD_PROTOCOL_PLUGIN_ID: &str = "standard_protocol";

/// The execution section of the prompt, naming `batch` and its maximum only
/// when the sugar is offered, and `finish` when only a control call ends the
/// session's turns.
fn standard_execution_section(
    batch: BatchSugar,
    termination: lash_core::TerminationMode,
) -> String {
    let ending = if termination.prose_ends_turn() {
        "Answer in prose only when no tool is needed."
    } else {
        "A prose reply does not end the turn: once the work is done, call `finish` with the answer, on its own."
    };
    match batch {
        BatchSugar::Enabled { max_members } => format!(
            "Call tools directly with their declared JSON arguments. Use `batch` for two or more independent calls (at most {max_members} per batch); make dependent calls after their inputs return. Check each batch result’s success flag before using its value. {ending}"
        ),
        BatchSugar::Disabled => format!(
            "Call tools directly with their declared JSON arguments. Make independent calls together; make dependent calls after their inputs return. {ending}"
        ),
    }
}

/// The hard ceiling on members per `batch` call. A configured maximum above
/// it is refused when the plugin builds.
pub const BATCH_MEMBER_CEILING: usize = 64;

/// Whether the driver offers `batch`, and with how many members per call. A
/// session records its choice at creation ([`StandardRecordedBehaviour`]).
///
/// `batch` is protocol sugar, not a tool: the driver expands each call into
/// the step's one tool group beside the response's native calls, so every
/// member starts before any finishes, and folds the members' results into one
/// batch result. It is not a Tool Catalog entry, so tool membership does not
/// apply to it and RLM cells and processes cannot call it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchSugar {
    /// `batch` is offered, with at most `max_members` members per call.
    Enabled { max_members: std::num::NonZeroUsize },
    /// `batch` is not offered. A call named `batch` is an ordinary unknown
    /// tool.
    Disabled,
}

impl Default for BatchSugar {
    fn default() -> Self {
        Self::standard()
    }
}

impl BatchSugar {
    /// Standard preset: batch enabled with 64 members. The immutable member
    /// ceiling is 64; no workload measurement establishes the preset for all hosts.
    pub fn standard() -> Self {
        Self::Enabled {
            max_members: std::num::NonZeroUsize::MIN.saturating_add(BATCH_MEMBER_CEILING - 1),
        }
    }
}

/// Plugin factory that installs the standard-protocol driver,
/// session plugin, and native tool catalog.
#[derive(Default)]
pub struct StandardProtocolPluginFactory {
    config: StandardProtocolConfig,
}

/// A host's standard-protocol configuration. The renderer slot is bound
/// live; the discovery operation and the batch choice are this deployment's
/// creation defaults, which a session records at creation and runs under on
/// every open, whichever deployment opens it (FIG-4398).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandardProtocolConfig {
    pub discovery: Option<lash_core::ToolDiscovery>,
    pub render: StandardRenderConfig,
    pub renderer: ToolOutputRendererSlot,
    pub batch: BatchSugar,
}

impl Default for StandardProtocolConfig {
    fn default() -> Self {
        Self::standard()
    }
}

impl StandardProtocolConfig {
    /// Standard protocol preset: no discovery, built-in renderer, the complete
    /// standard tool render and batch presets. Cuts have no universal workload
    /// measurement. All settings remain optional host overrides.
    pub fn standard() -> Self {
        Self {
            discovery: None,
            render: StandardRenderConfig::standard(),
            renderer: ToolOutputRendererSlot::default(),
            batch: BatchSugar::standard(),
        }
    }

    /// Offer or withhold the `batch` sugar. A maximum above
    /// [`BATCH_MEMBER_CEILING`] is refused when the plugin builds.
    pub fn batch(mut self, sugar: BatchSugar) -> Self {
        self.batch = sugar;
        self
    }

    /// The behaviour a session created under this configuration records.
    pub fn recorded_behaviour(&self) -> StandardRecordedBehaviour {
        StandardRecordedBehaviour {
            discovery_operation: self
                .discovery
                .as_ref()
                .map(|discovery| discovery.operation.clone()),
            batch: self.batch,
            render: self.render.recorded_base(),
        }
    }

    /// This configuration's renderer under a session's recorded `behaviour`:
    /// what the session's driver runs.
    fn under_recorded_behaviour(mut self, behaviour: &StandardRecordedBehaviour) -> Self {
        self.discovery = behaviour
            .discovery_operation
            .clone()
            .map(|operation| lash_core::ToolDiscovery { operation });
        self.batch = behaviour.batch;
        self.render = behaviour.render.clone();
        self
    }
}

/// The standard-protocol behaviour a session records at creation
/// (FIG-4398): the discovery operation, the batch choice and the configured
/// render its driver runs under. It is pinned: no config command changes it, a run override cannot
/// state it again, and a session opened, redriven or resumed by a deployment
/// configured otherwise still runs under it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandardRecordedBehaviour {
    /// The host operation the model discovers tools omitted from the prompt
    /// with, or `None` when every tool is inline.
    #[serde(deserialize_with = "serde::Deserialize::deserialize")]
    pub discovery_operation: Option<String>,
    pub batch: BatchSugar,
    /// The render the creating deployment configured: the base a run's
    /// render is resolved over, under the session's own render options
    /// (FIG-4527).
    #[serde(deserialize_with = "deserialize_recorded_render")]
    pub render: StandardRenderConfig,
}

fn deserialize_recorded_render<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<StandardRenderConfig, D::Error> {
    let render = <StandardRenderConfig as serde::Deserialize>::deserialize(deserializer)?;
    if render.recorded_base() != render {
        return Err(serde::de::Error::custom(
            "recorded standard render must state a complete base",
        ));
    }
    Ok(render)
}

/// The standard protocol's recorded session namespace (FIG-4379,
/// FIG-4398): the render options its tool results render with,
/// and the behaviour the session was created with. Render options apply over
/// the recorded render. Every reader decodes this type (FIG-4652).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
pub struct StandardRecordedConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub render: Option<StandardRenderConfig>,
    /// How the session's turns may end. Absence is the `Natural` default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub termination: Option<lash_core::TerminationMode>,
    #[schemars(with = "serde_json::Value")]
    pub behaviour: StandardRecordedBehaviour,
}

impl StandardRecordedConfig {
    /// How a turn under the namespace `namespace` may end: as it recorded,
    /// or `Natural` for a session that recorded no standard namespace.
    fn termination_of(
        namespace: &lash_core::ProtocolTurnOptions,
    ) -> Result<lash_core::TerminationMode, lash_core::ProtocolTurnOptionsError> {
        if namespace.is_empty() {
            return Ok(lash_core::TerminationMode::default());
        }
        Ok(namespace.decode::<Self>()?.termination.unwrap_or_default())
    }
}

/// The recorded key of the session's behaviour, named when a rebuilt
/// session recorded none.
const BEHAVIOUR_FIELD: &str = "behaviour";

/// Standard protocol creation input (FIG-4379): the render options its tool
/// results render with, over the host's configured render, and how its
/// turns may end (FIG-5801). A stated `null` reads as unstated. The prompt is not creation input: the protocol
/// contributes keyed sections, and a host adds or wraps sections of its own
/// (ADR 0133).
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(try_from = "serde_json::Value")]
pub struct StandardTurnOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub render: Option<StandardRenderConfig>,
    /// `TerminalRequired` turns end only through a control call, and the
    /// session is offered `finish`; absence is `Natural`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub termination: Option<lash_core::TerminationMode>,
}

/// The wire form a [`StandardTurnOptions`] decodes through once its nulls
/// are dropped.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StandardTurnOptionsWire {
    #[serde(default)]
    render: Option<StandardRenderConfig>,
    #[serde(default)]
    termination: Option<lash_core::TerminationMode>,
}

impl TryFrom<serde_json::Value> for StandardTurnOptions {
    type Error = serde_json::Error;

    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        let wire: StandardTurnOptionsWire = serde_json::from_value(render::without_nulls(value))?;
        Ok(Self {
            render: wire.render,
            termination: wire.termination,
        })
    }
}

/// The options a run states for the standard protocol (FIG-4589): its render
/// options, which the owner applies field by field over the session's
/// ([`ConfigOwner::apply_run_options`]), and how the run's turn may end,
/// which replaces the session's. Nothing else is a field: a payload
/// that names the session's behaviour does not decode, so a run cannot
/// state it, whatever value it gives. A stated `null` reads as
/// unstated.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(try_from = "serde_json::Value")]
pub struct StandardRunOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub render: Option<StandardRenderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub termination: Option<lash_core::TerminationMode>,
}

/// The wire form a [`StandardRunOptions`] decodes through once its nulls are
/// dropped.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StandardRunOptionsWire {
    #[serde(default)]
    render: Option<StandardRenderConfig>,
    #[serde(default)]
    termination: Option<lash_core::TerminationMode>,
}

impl TryFrom<serde_json::Value> for StandardRunOptions {
    type Error = serde_json::Error;

    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        let wire: StandardRunOptionsWire = serde_json::from_value(render::without_nulls(value))?;
        Ok(Self {
            render: wire.render,
            termination: wire.termination,
        })
    }
}

/// The standard protocol's config owner: it records the creator's render
/// options and this host's configured behaviour. Its render command keeps
/// the behaviour pinned.
#[derive(Clone, Debug)]
pub struct StandardConfigOwner {
    behaviour: StandardRecordedBehaviour,
}

/// Why the standard owner refused a candidate.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StandardConfigRefusal {
    /// The candidate changes the behaviour the session recorded at
    /// creation.
    BehaviourChanged { recorded: String, candidate: String },
}

/// Why the standard protocol resolved no render for a run.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StandardRenderRefusal {
    /// A resolved head share is not a percentage.
    HeadShareOutOfRange { head_share_percent: u8 },
    /// The resolved render parameters did not encode as a record.
    Unencodable { message: String },
}

impl std::fmt::Display for StandardRenderRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HeadShareOutOfRange { head_share_percent } => write!(
                formatter,
                "head_share_percent must be within 0..=100, got {head_share_percent}"
            ),
            Self::Unencodable { message } => {
                write!(formatter, "the resolved render does not encode: {message}")
            }
        }
    }
}

impl std::fmt::Display for StandardConfigRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BehaviourChanged {
                recorded,
                candidate,
            } => write!(
                formatter,
                "the session's standard-protocol behaviour is recorded as {recorded} and cannot \
                 become {candidate}"
            ),
        }
    }
}

impl ConfigOwner for StandardConfigOwner {
    type Create = StandardTurnOptions;
    type Recorded = StandardRecordedConfig;
    type Refusal = StandardConfigRefusal;
    type RunOptions = StandardRunOptions;

    /// Every session records its namespace: the creator's render options,
    /// or none, under which the recorded render applies, and this host's
    /// behaviour (FIG-4527). A child records what its creator states, like a
    /// root; only a fork copies a recorded namespace (ADR 0134).
    fn create(
        &self,
        input: Option<StandardTurnOptions>,
    ) -> Result<Option<StandardRecordedConfig>, StandardConfigRefusal> {
        let input = input.unwrap_or_default();
        Ok(Some(StandardRecordedConfig {
            render: input.render,
            termination: input.termination,
            behaviour: self.behaviour.clone(),
        }))
    }

    /// A candidate keeps the behaviour its base recorded.
    fn validate(
        &self,
        value: &StandardRecordedConfig,
        base: Option<&StandardRecordedConfig>,
        _facts: &CandidateFacts<'_>,
    ) -> Result<(), StandardConfigRefusal> {
        let Some(base) = base else {
            return Ok(());
        };
        if value.behaviour != base.behaviour {
            let spelled = |behaviour: &StandardRecordedBehaviour| {
                serde_json::to_string(behaviour).unwrap_or_default()
            };
            return Err(StandardConfigRefusal::BehaviourChanged {
                recorded: spelled(&base.behaviour),
                candidate: spelled(&value.behaviour),
            });
        }
        Ok(())
    }

    /// A run's render options apply over the session's, field by field
    /// and tool by tool, and its stated termination replaces the session's.
    /// The behaviour stays as recorded.
    fn apply_run_options(
        &self,
        recorded: &StandardRecordedConfig,
        options: StandardRunOptions,
    ) -> Result<StandardRecordedConfig, StandardConfigRefusal> {
        let render = match (options.render, recorded.render.as_ref()) {
            (Some(stated), Some(recorded)) => Some(stated.over(recorded)),
            (stated, recorded) => stated.or_else(|| recorded.cloned()),
        };
        Ok(StandardRecordedConfig {
            render,
            termination: options.termination.or(recorded.termination),
            ..recorded.clone()
        })
    }
}

/// Replace the session's render options, whole; `None` clears them, and the
/// render the session recorded at creation applies.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
pub struct SetStandardRender {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub render: Option<StandardRenderConfig>,
}

impl ConfigCommand for SetStandardRender {
    type Owner = StandardConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_render";
}

impl StandardProtocolPluginFactory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: StandardProtocolConfig) -> Self {
        Self { config }
    }
}

impl PluginFactory for StandardProtocolPluginFactory {
    fn id(&self) -> &'static str {
        STANDARD_PROTOCOL_PLUGIN_ID
    }

    /// The session's standard-protocol namespace and its typed commands
    /// (FIG-4379).
    fn register_config(
        &self,
        registrar: &mut ConfigRegistrar,
    ) -> Result<(), ConfigRegistrationError> {
        registrar.owner(StandardConfigOwner {
            behaviour: self.config.recorded_behaviour(),
        })?;
        registrar.command::<SetStandardRender>(|recorded, command| {
            Ok(OwnerChange {
                recorded: StandardRecordedConfig {
                    render: command.render,
                    ..recorded.clone()
                },
                output: (),
            })
        })
    }

    /// The session's plugin runs under the behaviour the session recorded
    /// (FIG-4398). A session being created has recorded none yet and runs
    /// under what its creation records, this deployment's; a rebuilt session
    /// that recorded none is refused, never given this deployment's.
    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        let recorded = ctx
            .plugin_config
            .decode::<StandardRecordedConfig>(STANDARD_PROTOCOL_PLUGIN_ID)
            .map_err(|error| {
                PluginError::Session(format!(
                    "invalid recorded standard-protocol session config: {error}"
                ))
            })?;
        // A session whose turns only a control call ends is offered
        // `finish`. A run that requires one of a session that is not finds
        // no `finish` unless the host installed a tool that declares it,
        // and is refused when its turn is formed.
        let termination = recorded
            .as_ref()
            .and_then(|recorded| recorded.termination)
            .unwrap_or_default();
        let behaviour = match recorded {
            Some(recorded) => recorded.behaviour,
            None if matches!(
                ctx.materialization,
                lash_core::plugin::PluginSessionMaterialization::Rematerialization
            ) =>
            {
                return Err(PluginError::MissingRecordedSessionConfig {
                    plugin_id: STANDARD_PROTOCOL_PLUGIN_ID.to_string(),
                    field: BEHAVIOUR_FIELD.to_string(),
                });
            }
            None => self.config.recorded_behaviour(),
        };
        if let BatchSugar::Enabled { max_members } = behaviour.batch
            && max_members.get() > BATCH_MEMBER_CEILING
        {
            return Err(PluginError::InvalidBatchMaximum {
                requested: max_members.get(),
                ceiling: BATCH_MEMBER_CEILING,
            });
        }
        Ok(Arc::new(StandardProtocolPlugin {
            config: self.config.clone().under_recorded_behaviour(&behaviour),
            termination,
        }))
    }
}

impl lash_core::plugin::PluginDefinition for StandardProtocolPluginFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(STANDARD_PROTOCOL_PLUGIN_ID)
    }
}

struct StandardProtocolPlugin {
    config: StandardProtocolConfig,
    termination: lash_core::TerminationMode,
}

impl SessionPlugin for StandardProtocolPlugin {
    fn id(&self) -> &'static str {
        STANDARD_PROTOCOL_PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let renderer = self.config.renderer.clone();
        reg.tool_results().presenter(Arc::new(move |input| {
            let renderer = renderer.clone();
            Box::pin(async move { render::present(input, &renderer).await })
        }))?;
        reg.protocol().session(Arc::new(StandardProtocolSession))?;
        prompt::register_sections(
            reg,
            prompt::StandardPromptBehaviour {
                batch: self.config.batch,
                termination: self.termination,
            },
        )?;
        reg.protocol()
            .protocol_driver(Arc::new(StandardProtocolDriver {
                config: self.config.clone(),
            }))?;
        if !self.termination.prose_ends_turn() {
            reg.tools().provider(Arc::new(finish::FinishToolProvider))?;
        }
        let discovery = self.config.discovery.clone();
        let batch = self.config.batch;
        reg.tool_catalog().contribute(
            lash_core::hook_key!("standard-catalog"),
            Arc::new(move |ctx| {
                validate_discovery(&ctx.tools, discovery.as_ref())?;
                validate_batch_name(&ctx.tools, batch)?;
                Ok(Default::default())
            }),
        )?;
        Ok(())
    }
}

fn validate_discovery(
    tools: &[lash_core::ToolManifest],
    discovery: Option<&lash_core::ToolDiscovery>,
) -> Result<(), PluginError> {
    if let Some(discovery) = discovery
        && !tools
            .iter()
            .any(|tool| tool.inline && tool.name == discovery.operation)
    {
        return Err(PluginError::InvalidToolDiscovery {
            operation: discovery.operation.clone(),
        });
    }
    Ok(())
}

/// While the sugar is offered, `batch` names it in every request, so a
/// catalogue tool of that name could never be called: it is refused.
fn validate_batch_name(
    tools: &[lash_core::ToolManifest],
    batch: BatchSugar,
) -> Result<(), PluginError> {
    if matches!(batch, BatchSugar::Enabled { .. })
        && let Some(tool) = tools
            .iter()
            .find(|tool| tool.name == batch::BATCH_TOOL_NAME)
    {
        return Err(PluginError::ResidentToolDuplicateName {
            name: tool.name.clone(),
        });
    }
    Ok(())
}

/// The standard protocol's session plugin: it keeps no protocol state.
struct StandardProtocolSession;

#[async_trait]
impl ProtocolSessionPlugin for StandardProtocolSession {
    async fn initialize_session(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), SessionError> {
        Ok(())
    }
}

struct StandardProtocolDriver {
    config: StandardProtocolConfig,
}

impl ProtocolDriverPlugin for StandardProtocolDriver {
    fn resolve_render(
        &self,
        namespace: &lash_core::ProtocolTurnOptions,
    ) -> Result<Option<lash_core::RecordedRender>, lash_core::RenderFault> {
        // A session that recorded no standard namespace renders under the
        // configured render alone.
        let recorded = if namespace.is_empty() {
            None
        } else {
            Some(
                namespace
                    .decode::<StandardRecordedConfig>()
                    .map_err(|error| lash_core::RecordedNamespaceCorrupt {
                        owner: STANDARD_PROTOCOL_PLUGIN_ID.to_string(),
                        message: error.to_string(),
                    })?,
            )
        };
        let resolved = render::resolve(
            &StandardRenderConfig::builtin(),
            &self.config.render,
            &recorded
                .and_then(|recorded| recorded.render)
                .unwrap_or_default(),
        )
        .map_err(|refusal| lash_core::RenderRefusal::new(&refusal))?;
        Ok(Some(lash_core::RecordedRender {
            renderer_id: self.config.renderer.0.id().to_string(),
            params: serde_json::to_value(resolved).map_err(|error| {
                lash_core::RenderRefusal::new(&StandardRenderRefusal::Unencodable {
                    message: error.to_string(),
                })
            })?,
        }))
    }

    fn prompt_tools(
        &self,
        catalog: Arc<lash_core::ToolCatalog>,
    ) -> lash_core::plugin::prompt::OfferedTools {
        lash_core::plugin::prompt::OfferedTools::new(catalog, self.config.discovery.is_some())
    }

    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        let tool_names = input.tool_catalog.tool_names();
        let visible_catalog;
        let catalog = if self.config.discovery.is_some() {
            visible_catalog = input.tool_catalog.inline_tools();
            &visible_catalog
        } else {
            input.tool_catalog.as_ref()
        };
        let catalog_specs = catalog.model_tool_specs();
        let tool_specs = match self.config.batch {
            BatchSugar::Enabled { max_members } => {
                let model_tool = batch_tool_definition(max_members).model_tool();
                let mut specs = catalog_specs.as_ref().clone();
                specs.push(lash_core::llm::types::LlmToolSpec {
                    name: model_tool.name,
                    description: model_tool.description,
                    input_schema: model_tool.input_schema,
                    output_schema: model_tool.output_schema,
                });
                Arc::new(specs)
            }
            BatchSugar::Disabled => catalog_specs,
        };
        TurnDriverPreamble {
            config: TurnDriverConfig::chat(Arc::new(StandardDriver {
                discovery: self.config.discovery.is_some(),
                batch: self.config.batch,
            })),
            tool_specs,
            tool_names,
            writer_formats: input.writer_formats,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Standard protocol driver
// ─────────────────────────────────────────────────────────────────────

/// Protocol driver for the Standard protocol. Consumes native
/// tool-call envelopes from the LLM, expands `batch` sugar into the step's
/// one tool group and dispatches it via `PendingWork::WaitingForToolResults`, and splices
/// reasoning parts into the assistant message so provider replay metadata
/// preserves chain-of-thought ordering.
#[derive(Default)]
pub struct StandardDriver {
    discovery: bool,
    batch: BatchSugar,
}

#[derive(Clone, Debug)]
struct StandardToolCall {
    call_id: lash_core::ToolCallId,
    provider_call_id: String,
    tool_name: String,
    input_json: String,
    replay: Option<ProviderReplayMeta>,
}

#[derive(Clone, Debug)]
enum StandardResponsePart {
    Text {
        text: String,
        response_meta: Option<ResponseTextMeta>,
    },
    Reasoning {
        text: String,
        replay: Option<ProviderReasoningReplay>,
    },
    ToolCall(StandardToolCall),
}

#[derive(Debug)]
struct StandardResponse {
    assistant_text: String,
    parts: Vec<StandardResponsePart>,
}

fn collect_standard_response(
    llm_response: &LlmResponse,
    calls: &lash_core::sansio::ResponseToolCalls,
) -> StandardResponse {
    let mut assistant_text = String::new();
    let mut parts = Vec::new();
    let mut call_ids = calls.call_ids(llm_response).into_iter();

    for part in normalized_response_parts(llm_response) {
        match part {
            LlmOutputPart::Text {
                text,
                response_meta,
            } => {
                if text.trim().is_empty() {
                    continue;
                }
                let previous_len = assistant_text.len();
                lash_core::facade_support::append_assistant_text_part(&mut assistant_text, &text);
                parts.push(StandardResponsePart::Text {
                    text: assistant_text[previous_len..].to_string(),
                    response_meta,
                });
            }
            LlmOutputPart::Reasoning { text, replay } => {
                let text = text.trim().to_string();
                if text.is_empty() && replay.as_ref().is_none_or(|meta| meta.is_empty()) {
                    continue;
                }
                parts.push(StandardResponsePart::Reasoning { text, replay });
            }
            LlmOutputPart::ToolCall {
                call_id: provider_call_id,
                tool_name,
                input_json,
                replay,
            } => {
                let Some(call_id) = call_ids.next() else {
                    continue;
                };
                parts.push(StandardResponsePart::ToolCall(StandardToolCall {
                    call_id,
                    provider_call_id,
                    tool_name,
                    input_json,
                    replay,
                }));
            }
        }
    }

    StandardResponse {
        assistant_text,
        parts,
    }
}

fn reassemble_standard_response(
    assistant_id: &str,
    parts: Vec<StandardResponsePart>,
) -> (Vec<Part>, Vec<round::ResponseCall>) {
    let mut message_parts = Vec::with_capacity(parts.len());
    let mut calls = Vec::new();

    for part in parts {
        match part {
            StandardResponsePart::Text {
                text,
                response_meta,
            } => {
                if text.trim().is_empty() {
                    continue;
                }
                message_parts.push(Part::prose(
                    format!("{assistant_id}.p{}", message_parts.len()),
                    text,
                    response_meta,
                ));
            }
            StandardResponsePart::Reasoning { text, replay } => {
                message_parts.push(reasoning_part(
                    assistant_id,
                    message_parts.len(),
                    text,
                    replay,
                ));
            }
            StandardResponsePart::ToolCall(tool_call) => {
                message_parts.push(Part::tool_call(
                    format!("{assistant_id}.p{}", message_parts.len()),
                    tool_call.input_json.clone(),
                    tool_call.call_id.clone(),
                    tool_call.provider_call_id.clone(),
                    tool_call.tool_name.clone(),
                    tool_call.replay.clone(),
                ));
                let (args, parse_error) = match serde_json::from_str::<Value>(&tool_call.input_json)
                {
                    Ok(args) => (args, None),
                    Err(error) => (Value::Null, Some(error.to_string())),
                };
                calls.push(round::ResponseCall {
                    call: PendingToolCall {
                        call_id: tool_call.call_id,
                        provider_call_id: Some(tool_call.provider_call_id),
                        tool_name: tool_call.tool_name,
                        args,
                        replay: tool_call.replay,
                    },
                    input_json: tool_call.input_json,
                    parse_error,
                });
            }
        }
    }

    (message_parts, calls)
}

/// Build the `CompletedToolCall` for a call refused before dispatch. The
/// refusal is typed (`output`) and the model sees its serialized form
/// through `model_return`, the same shape execution results take.
#[expect(
    clippy::expect_used,
    reason = "the typed refusal is a crate-owned ToolCallOutput tree whose serde_json encoding cannot fail"
)]
fn refused_tool_call_completion(
    call_id: lash_core::ToolCallId,
    provider_call_id: Option<String>,
    tool_name: String,
    args: Value,
    output: lash_core::ToolCallOutput,
    replay: Option<ProviderReplayMeta>,
) -> CompletedToolCall {
    let model_return = lash_core::facade_support::ModelToolReturn {
        attachment_notices: Vec::new(),
        tool_name: tool_name.clone(),
        parts: vec![lash_core::facade_support::ModelToolReturnPart::Text {
            text: serde_json::to_string(&output).expect("typed refusal serializes"),
        }],
    };
    CompletedToolCall {
        call_id,
        provider_call_id,
        tool_name,
        args,
        output,
        model_return,
        intent_outcomes: Vec::new(),
        replay,
    }
}

impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for StandardDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        if let Err(refusal) = finish_available(&ctx) {
            return invalid_turn_options_actions(refusal);
        }
        let request = match ctx.project_llm_request(true) {
            Ok(request) => request,
            Err(error) => return lash_sansio::sansio::stored_history_refusal_actions(error),
        };
        vec![DriverAction::Start(PendingWork::Llm {
            request,
            driver_state: None,
        })]
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        request: Arc<lash_core::LlmRequest>,
        _driver_state: Option<lash_core::ProtocolDriverState>,
        llm_response: LlmResponse,
        calls: &lash_core::sansio::ResponseToolCalls,
        text_streamed: bool,
    ) -> Vec<DriverAction> {
        let response = collect_standard_response(&llm_response, calls);
        let mut actions = Vec::new();

        if !text_streamed {
            // Buffered completions publish the same Started/Delta/Completed
            // lifecycle the streaming lane emits, with the same identity
            // scheme (`item_id` where the provider named the item,
            // `part:{index}` otherwise), so replay and live sessions are
            // indistinguishable to hosts.
            let mut ordinal = 0u64;
            for (part_index, part) in response.parts.iter().enumerate() {
                if let StandardResponsePart::Text {
                    text,
                    response_meta,
                } = part
                    && !text.is_empty()
                {
                    let item_id = response_meta.as_ref().and_then(|meta| meta.id.clone());
                    let block = lash_sansio::llm::types::StreamBlockIdentity {
                        id: item_id
                            .clone()
                            .unwrap_or_else(|| format!("part:{part_index}")),
                        ordinal,
                        item_id,
                    };
                    ordinal += 1;
                    actions.push(DriverAction::Emit(SessionStreamEvent::StreamBlock(
                        StreamBlockEvent::Started {
                            kind: StreamBlockKind::AssistantText,
                            block: block.clone(),
                        },
                    )));
                    actions.push(DriverAction::Emit(SessionStreamEvent::StreamBlock(
                        StreamBlockEvent::Delta {
                            kind: StreamBlockKind::AssistantText,
                            text: text.clone(),
                            block: block.clone(),
                        },
                    )));
                    actions.push(DriverAction::Emit(SessionStreamEvent::StreamBlock(
                        StreamBlockEvent::Completed {
                            kind: StreamBlockKind::AssistantText,
                            block,
                            text: text.clone(),
                        },
                    )));
                }
            }
        }

        actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: ctx.protocol_iteration(),
            content: response.assistant_text.clone(),
        }));

        let has_tool_calls = response
            .parts
            .iter()
            .any(|part| matches!(part, StandardResponsePart::ToolCall(_)));
        let asst_id = standard_message_id(ctx.turn_id(), ctx.protocol_iteration(), "assistant");
        let (assistant_parts, reassembled_calls) =
            reassemble_standard_response(&asst_id, response.parts);

        if !has_tool_calls {
            if !assistant_parts.is_empty() {
                actions.push(DriverAction::AppendEvents(vec![conversation_event(
                    Message {
                        id: asst_id,
                        role: MessageRole::Assistant,
                        parts: shared_parts(assistant_parts),
                        origin: Some(standard_message_origin(ctx.turn_id())),
                        reply_marker: None,
                    },
                )]));
            }
            match StandardRecordedConfig::termination_of(ctx.termination()) {
                Err(error) => {
                    return invalid_turn_options_actions(format!(
                        "the turn's standard-protocol options do not decode: {error}"
                    ));
                }
                Ok(termination) if !termination.prose_ends_turn() => {
                    actions.extend(finish_required_repair(&ctx));
                    return actions;
                }
                Ok(_) => {}
            }
            actions.push(DriverAction::Start(PendingWork::Checkpoint {
                checkpoint: CheckpointKind::BeforeCompletion,
                on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                    TurnFinish::AssistantMessage {
                        text: response.assistant_text,
                    },
                )),
            }));
            return actions;
        }

        if !assistant_parts.is_empty() {
            actions.push(DriverAction::AppendEvents(vec![conversation_event(
                Message {
                    id: asst_id,
                    role: MessageRole::Assistant,
                    parts: shared_parts(assistant_parts),
                    origin: Some(standard_message_origin(ctx.turn_id())),
                    reply_marker: None,
                },
            )]));
        }

        let listed = |name: &str| request.tools.iter().any(|tool| tool.name == name);
        let ends_the_turn = |name: &str| ctx.ends_the_turn(name);
        let round = round::plan_round(
            reassembled_calls,
            &round::RoundRules {
                batch: self.batch,
                listed: self.discovery.then_some(&listed as &dyn Fn(&str) -> bool),
                ends_the_turn: &ends_the_turn,
            },
        );
        let calls = round.calls;
        let refused = round
            .refused
            .into_iter()
            .map(|(call, output)| {
                refused_tool_call_completion(
                    call.call_id,
                    call.provider_call_id,
                    call.tool_name,
                    call.args,
                    output,
                    call.replay,
                )
            })
            .collect::<Vec<_>>();
        if !refused.is_empty() {
            let completed = refused;
            actions.push(DriverAction::ReportToolCalls {
                completed: completed.clone(),
            });
            if calls.is_empty() && round.plan.is_empty() && round.control.is_none() {
                actions.extend(self.handle_tool_results(ctx, completed));
                return actions;
            }
            let mut parts: Vec<Part> = completed
                .into_iter()
                .map(|outcome| tool_result_part(outcome.call_id, outcome.model_return))
                .collect();
            let message_id =
                standard_message_id(ctx.turn_id(), ctx.protocol_iteration(), "refused_tools");
            reassign_part_ids(&message_id, &mut parts);
            actions.push(DriverAction::AppendEvents(vec![conversation_event(
                Message {
                    id: message_id,
                    role: MessageRole::User,
                    parts: shared_parts(parts),
                    origin: Some(standard_message_origin(ctx.turn_id())),
                    reply_marker: None,
                },
            )]));
        }
        actions.push(DriverAction::Start(match round.control {
            Some((slot, control)) => {
                PendingWork::tool_round_with_control(calls, round.plan, slot, control)
            }
            None => PendingWork::tool_round(calls, round.plan),
        }));
        actions
    }

    fn fold_tool_results(
        &self,
        plan: &lash_core::sansio::ToolExpansionPlan,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<CompletedToolCall> {
        batch::fold(plan, completed)
    }

    fn handle_tool_results(
        &self,
        ctx: DriverContextView<'_>,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        let mut actions = Vec::new();
        let mut result_parts = Vec::new();
        let mut terminal_outcome = None;
        // The round's settled control call: the dispatcher admits at most
        // one, after its siblings, so the first is the only one.
        let mut candidate = None;

        for outcome in completed {
            if terminal_outcome.is_none() && outcome.output.is_success() {
                match outcome.output.control.as_ref() {
                    Some(lash_core::ToolControl::Turn { control }) if candidate.is_none() => {
                        // A `batch` member's control is the member's own:
                        // its wrapper only carries it.
                        let (call_id, tool_name) = match self.batch {
                            BatchSugar::Enabled { .. }
                                if outcome.tool_name == batch::BATCH_TOOL_NAME =>
                            {
                                batch::control_member(&outcome, &|name| ctx.ends_the_turn(name))
                                    .unwrap_or_else(|| {
                                        (outcome.call_id.clone(), outcome.tool_name.clone())
                                    })
                            }
                            _ => (outcome.call_id.clone(), outcome.tool_name.clone()),
                        };
                        candidate = Some(lash_core::CompletionCandidate::pending(
                            ctx.protocol_iteration(),
                            call_id,
                            tool_name,
                            control.clone(),
                        ));
                    }
                    Some(control) => {
                        terminal_outcome =
                            lash_core::turn_stop_from_tool_control(&outcome.tool_name, control);
                    }
                    None => {}
                }
            }

            result_parts.push(tool_result_part(outcome.call_id, outcome.model_return));
        }

        if !result_parts.is_empty() {
            let user_id =
                standard_message_id(ctx.turn_id(), ctx.protocol_iteration(), "tool_results");
            reassign_part_ids(&user_id, &mut result_parts);
            actions.push(DriverAction::AppendEvents(vec![conversation_event(
                Message {
                    id: user_id,
                    role: MessageRole::User,
                    parts: shared_parts(result_parts),
                    origin: Some(standard_message_origin(ctx.turn_id())),
                    reply_marker: None,
                },
            )]));
        }

        if let Some(outcome) = terminal_outcome {
            actions.push(DriverAction::Finish(outcome));
            return actions;
        }

        // A settled control call does not end the turn here: its candidate
        // is decided at BeforeCompletion, where arriving input supersedes
        // it and the turn goes on.
        if let Some(candidate) = candidate {
            let superseded = completion_superseded_message(
                standard_message_id(
                    ctx.turn_id(),
                    ctx.protocol_iteration(),
                    "completion_superseded",
                ),
                ctx.turn_id(),
                &candidate,
            );
            actions.push(DriverAction::Start(PendingWork::Checkpoint {
                checkpoint: CheckpointKind::BeforeCompletion,
                on_empty: CheckpointResumeAction::Complete {
                    candidate: Box::new(candidate),
                    superseded: vec![superseded],
                },
            }));
            return actions;
        }

        actions.push(DriverAction::AdvanceProtocolIteration);
        let next_protocol_iteration = ctx.protocol_iteration() + 1;
        if let Some(max_turns) = ctx.turn_budget().max_turns()
            && next_protocol_iteration >= ctx.protocol_run_offset() + max_turns
        {
            actions.push(DriverAction::Finish(TurnOutcome::Stopped(
                TurnStop::MaxTurns,
            )));
            return actions;
        }

        actions.push(DriverAction::Start(PendingWork::Checkpoint {
            checkpoint: CheckpointKind::AfterWork,
            on_empty: CheckpointResumeAction::PrepareIteration,
        }));
        actions
    }

    // Equivalent mutant: cargo-mutants' `vec![]` replacement is the same value
    // as this body's `Vec::new()`, so no test can tell them apart.
    #[cfg_attr(test, mutants::skip)]
    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _driver_state: lash_core::ProtocolDriverState,
        _result: Result<lash_core::ExecResponse, lash_core::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

/// A turn that only a control call ends can end only if a tool it can
/// call declares Finish: one that cannot is refused when its turn is
/// formed, whether the session recorded the mode or the run stated it.
fn finish_available(ctx: &DriverContextView<'_>) -> Result<(), String> {
    let termination = StandardRecordedConfig::termination_of(ctx.termination())
        .map_err(|error| format!("the turn's standard-protocol options do not decode: {error}"))?;
    if termination.prose_ends_turn() || ctx.can_finish() {
        return Ok(());
    }
    Err(
        "the turn requires a finish (TerminalRequired), but no tool it can call declares Finish: create the session with `terminal_required` or install a tool that declares it"
            .to_owned(),
    )
}

fn invalid_turn_options_actions(error: String) -> Vec<DriverAction> {
    vec![
        DriverAction::Emit(lash_core::session_model::make_error_event(
            lash_core::session_model::TurnFailureKind::Runtime,
            Some(lash_core::session_model::TurnFailureCode::InvalidTurnOptions.into()),
            error.clone(),
            Some(error),
            lash_sansio::session_model::RuntimeOutputCuts::standard(),
        )),
        DriverAction::Finish(TurnOutcome::Stopped(TurnStop::RuntimeError)),
    ]
}

/// A reply that ends no turn its prose cannot end: the model is told to end
/// it through a control call, and the turn goes on, within its turn budget
/// and its no-progress budget, which counts the replies repaired since the
/// turn's last tool results.
fn finish_required_repair(ctx: &DriverContextView<'_>) -> Vec<DriverAction> {
    let mut actions = vec![DriverAction::AdvanceProtocolIteration];
    let next_protocol_iteration = ctx.protocol_iteration() + 1;
    if let Some(max_turns) = ctx.turn_budget().max_turns()
        && next_protocol_iteration >= ctx.protocol_run_offset() + max_turns
    {
        actions.push(DriverAction::Finish(TurnOutcome::Stopped(
            TurnStop::MaxTurns,
        )));
        return actions;
    }
    if ctx
        .no_progress_budget()
        .is_exhausted_by(repaired_replies(ctx) + 1)
    {
        actions.push(DriverAction::Finish(TurnOutcome::Stopped(
            TurnStop::MaxTurns,
        )));
        return actions;
    }
    let id = standard_message_id(
        ctx.turn_id(),
        ctx.protocol_iteration(),
        FINISH_REQUIRED_PURPOSE,
    );
    let tools = ctx
        .finishing_tools()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    actions.push(DriverAction::AppendEvents(vec![conversation_event(
        Message {
            id: id.clone(),
            role: MessageRole::System,
            parts: shared_parts(vec![Part::text(
                format!("{id}.p0"),
                format!(
                    "That reply did not end the turn: this turn ends only through a call that ends it ({tools}). Finish any remaining work, then make that call with the answer."
                ),
                None,
            )]),
            origin: Some(standard_message_origin(ctx.turn_id())),
            reply_marker: None,
        },
    )]));
    actions.push(DriverAction::Start(PendingWork::Checkpoint {
        checkpoint: CheckpointKind::AfterWork,
        on_empty: CheckpointResumeAction::PrepareIteration,
    }));
    actions
}

/// The purpose of a finish-required repair message's id.
const FINISH_REQUIRED_PURPOSE: &str = "finish_required";

/// The replies this turn repaired since its last tool results.
fn repaired_replies(ctx: &DriverContextView<'_>) -> usize {
    let prefix = format!("m_standard_{}_", ctx.turn_id());
    ctx.events()
        .iter()
        .rev()
        .filter_map(|record| match record {
            SessionHistoryRecord::Conversation(record) => record.id.strip_prefix(&prefix),
            SessionHistoryRecord::Protocol(_) => None,
        })
        .take_while(|id| !id.ends_with("_tool_results") && !id.ends_with("_refused_tools"))
        .filter(|id| id.ends_with(FINISH_REQUIRED_PURPOSE))
        .count()
}

/// What the model reads when input arrived at BeforeCompletion and its
/// control call therefore did not end the turn.
fn completion_superseded_message(
    id: String,
    turn_id: &TurnId,
    candidate: &lash_core::CompletionCandidate,
) -> Message {
    let did = match candidate.control {
        lash_core::TurnControl::Finish { .. } => "finish the turn",
        lash_core::TurnControl::SwitchAgentFrame { .. } => "switch the agent frame",
    };
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            format!(
                "The `{}` call did not {did}: new input arrived before it took effect, so it was superseded and the turn goes on. Read the new input, and end the turn again when the work is complete.",
                candidate.tool_name
            ),
            None,
        )]),
        origin: Some(standard_message_origin(turn_id)),
        reply_marker: None,
    }
}

/// Every Standard conversation message belongs to its producing turn,
/// including results represented as user messages for provider replay.
fn standard_message_origin(turn_id: &TurnId) -> lash_core::MessageOrigin {
    lash_core::MessageOrigin::TurnOutput {
        turn_id: turn_id.clone(),
        source: lash_core::TurnOutputSource::Plugin {
            plugin_id: STANDARD_PROTOCOL_PLUGIN_ID.to_string(),
        },
        cell_id: None,
    }
}

fn standard_message_id(turn_id: &TurnId, protocol_iteration: usize, purpose: &str) -> String {
    format!("m_standard_{turn_id}_{protocol_iteration}_{purpose}")
}

/// The one transcript part answering a tool call: the model return's text
/// and attachment blocks in the tool value's order, under the call's id.
/// Empty text blocks carry nothing and are dropped; a call whose return is
/// empty is still answered, so the transcript stays resume-safe.
fn tool_result_part(
    call_id: lash_core::ToolCallId,
    model_return: lash_core::facade_support::ModelToolReturn,
) -> Part {
    let content = model_return
        .parts
        .into_iter()
        .filter(|block| {
            !matches!(
                block,
                lash_core::facade_support::ModelToolReturnPart::Text { text } if text.is_empty()
            )
        })
        .collect();
    Part::tool_result(String::new(), content, call_id, model_return.tool_name)
}

fn conversation_event(message: Message) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord::from_message(message))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod discovery_tests;

#[cfg(test)]
mod tool_result_tests;

#[cfg(test)]
mod driver_contract_tests;

#[cfg(test)]
mod recorded_behaviour_tests;

#[cfg(test)]
mod retention_failure_tests;

#[cfg(test)]
mod prompt_tests;
