use super::{ExecutionBounds, InstructionBound, MemoryBound, MemoryLimit};

/// Prompt presentation of values and calls. These choices are pinned with
/// protocol behaviour; how tools read is the dialect's tool presenter's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlmPresentationConfig {
    pub max_inline_keys: usize,
    /// How many of a cell's calls its next prompt lists; durable history
    /// keeps all calls.
    pub prompt_max_tool_calls: usize,
    /// The byte cuts of a cell's listed calls: each result's summary, and
    /// the whole block (FIG-6293).
    pub call_feedback: crate::CallFeedbackPolicy,
    /// How many binding names a cell's record keeps of what the cell did to
    /// the session's bindings; it counts the rest.
    pub max_binding_change_names: usize,
    /// The bytes the bound-variables listing may take, its rows and their
    /// schema definitions together. The rows past it are counted, not
    /// listed (FIG-1438).
    pub max_bound_variable_bytes: usize,
}
impl Default for RlmPresentationConfig {
    fn default() -> Self {
        Self::standard()
    }
}
impl RlmPresentationConfig {
    /// Standard preset: 12 inline catalogue keys, 128 calls listed in the next prompt with 512-byte
    /// result summaries in a 16 KiB block,
    /// 32 binding-change names and a 24 KiB bound-variables listing, which
    /// leaves room under the standard 32 KiB prompt section.
    /// These historical presentation cuts have no universal workload measurement.
    pub const fn standard() -> Self {
        Self {
            max_inline_keys: 12,
            prompt_max_tool_calls: 128,
            call_feedback: crate::CallFeedbackPolicy::standard(),
            max_binding_change_names: 32,
            max_bound_variable_bytes: 24 * 1024,
        }
    }
}

/// How a session lists a catalogue too large to show whole (FIG-1435): the
/// host operation that finds the tools its prompt leaves out, and the bytes
/// the listed tools' docs share. A host that offers discovery states both;
/// neither has a preset.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmDiscovery {
    /// The discovery tool's call path, as a cell writes it. The tool is
    /// always listed.
    pub operation: String,
    /// The bytes the listed tools' docs take together, the discovery tool's
    /// included; module headings and separators are not counted.
    pub catalogue_bytes: usize,
}

/// A host's RLM protocol configuration.
///
/// The physical slots (the code renderer, and each dialect's tool
/// presenter) are bound live. Every behavioural
/// choice — the execution bounds, the Lash VM language
/// features, the prompt features, discovery, the output limit, the soft
/// context-budget warning, the render and the tool presentation — is this
/// deployment's creation
/// default: a session
/// records it in its RLM namespace when it is created
/// ([`RlmRecordedBehaviour`]), and every open, run and process of that
/// session runs under the recorded value, never under the configuration of
/// the deployment that happens to open it (FIG-4398).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmProtocolPluginConfig {
    #[serde(skip)]
    pub code_renderer: crate::render::CodeRendererSlot,
    #[serde(default)]
    pub render: lash_rlm_types::RlmRenderPatch,
    #[serde(default)]
    pub presentation: RlmPresentationConfig,
    /// The config a new session's tool presenter is admitted with: the keys
    /// it states over the presenter's standard ones
    /// ([`lash_tool_presenter_api::ToolPresenter::admit_config`]).
    #[serde(default = "default_tool_presentation")]
    pub tool_presentation: serde_json::Value,
    /// The discovery a new session records, if any.
    #[serde(skip)]
    pub discovery: Option<RlmDiscovery>,
    /// Session-pinned transport used for model-authored programs.
    pub channel: super::RlmChannel,
    pub charge: super::Charge,
    pub instruction_limit: InstructionBound,
    pub memory_limit: MemoryBound,
    /// Whether a cell's run can be saved. Durable unless the host states
    /// otherwise; see [`lash_vm_client::Durability`].
    #[serde(
        default,
        skip_serializing_if = "lash_vm_client::Durability::is_durable"
    )]
    pub durability: lash_vm_client::Durability,
    pub on_memory_limit: MemoryLimit,
    #[serde(default)]
    pub prompt_features: crate::protocol::RlmPromptFeatures,
    #[serde(default = "default_max_output_chars")]
    pub max_output_chars: usize,
    #[serde(default = "default_continue_as_soft_warn_tokens")]
    pub continue_as_soft_warn_tokens: Option<usize>,
}

fn default_tool_presentation() -> serde_json::Value {
    serde_json::json!({})
}

fn default_max_output_chars() -> usize {
    10_000
}

fn default_continue_as_soft_warn_tokens() -> Option<usize> {
    Some(100_000)
}

/// The RLM behaviour a session records at creation (FIG-4398): the logical
/// choices its driver, prompt and interpreter run under. It is created from
/// the creating deployment's [`RlmProtocolPluginConfig`], recorded in the
/// session's RLM namespace, and pinned there: no config command changes it,
/// a run override cannot state it again, and a session opened, redriven or
/// resumed by a deployment configured otherwise still runs under it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmRecordedBehaviour {
    pub charge: super::Charge,
    pub instruction_limit: InstructionBound,
    pub memory_limit: MemoryBound,
    /// Whether the session's cells run durably, which every open, run and
    /// resume of the session keeps.
    #[serde(
        default,
        skip_serializing_if = "lash_vm_client::Durability::is_durable"
    )]
    pub durability: lash_vm_client::Durability,
    pub on_memory_limit: MemoryLimit,
    pub prompt_features: crate::protocol::RlmPromptFeatures,
    pub max_output_chars: usize,
    /// The prompt-token threshold of the soft context-budget warning, or
    /// `None` for no warning.
    #[serde(deserialize_with = "serde::Deserialize::deserialize")]
    pub continue_as_soft_warn_tokens: Option<usize>,
    /// How the model finds tools the prompt omits and the budget of the
    /// tools it lists, or `None` when every tool is listed.
    #[serde(deserialize_with = "serde::Deserialize::deserialize")]
    pub discovery: Option<RlmDiscovery>,
    /// The render the creating deployment configured: the base a run's
    /// render is resolved over, under the session's own render preferences
    /// (FIG-4527).
    #[serde(deserialize_with = "deserialize_recorded_render")]
    pub render: lash_rlm_types::RlmRenderPatch,
    pub presentation: RlmPresentationConfig,
    /// The session's dialect's tool presenter and the complete config it
    /// admitted at creation. A session reopened on a host whose dialect
    /// holds another presenter is refused; its config is never read again
    /// from a host's defaults.
    pub tool_presenter: lash_tool_presenter_api::RecordedToolPresenter,
}

/// A builder slot that has not been filled in yet. [`RlmProtocolPluginConfigBuilder::build`]
/// exists only once every execution bound and the channel have replaced their
/// unset slots, so a config that forgot a required value is a compile error
/// rather than a silent default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnsetBound;

/// A builder slot that has not been filled in yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnsetChannel;

/// Builder for [`RlmProtocolPluginConfig`]. Each execution bound and the
/// channel are set by name and carry their own type, so required values cannot
/// be omitted or transposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RlmProtocolPluginConfigBuilder<I = UnsetBound, M = UnsetBound, C = UnsetChannel> {
    charge: super::Charge,
    instruction_limit: I,
    memory_limit: M,
    on_memory_limit: MemoryLimit,
    channel: C,
    durability: lash_vm_client::Durability,
}

impl<I, M, C> RlmProtocolPluginConfigBuilder<I, M, C> {
    /// Select exact charge metering (the default), or remove it entirely.
    pub fn charge(mut self, charge: super::Charge) -> Self {
        self.charge = charge;
        self
    }

    /// Choose the response to conservative memory pressure. Defaults to Recount.
    pub fn on_memory_limit(mut self, policy: MemoryLimit) -> Self {
        self.on_memory_limit = policy;
        self
    }

    pub fn channel(
        self,
        channel: super::RlmChannel,
    ) -> RlmProtocolPluginConfigBuilder<I, M, super::RlmChannel> {
        RlmProtocolPluginConfigBuilder {
            charge: self.charge,
            instruction_limit: self.instruction_limit,
            memory_limit: self.memory_limit,
            on_memory_limit: self.on_memory_limit,
            channel,
            durability: self.durability,
        }
    }

    pub fn instruction_limit(
        self,
        instruction_limit: InstructionBound,
    ) -> RlmProtocolPluginConfigBuilder<InstructionBound, M, C> {
        RlmProtocolPluginConfigBuilder {
            charge: self.charge,
            instruction_limit,
            memory_limit: self.memory_limit,
            on_memory_limit: self.on_memory_limit,
            channel: self.channel,
            durability: self.durability,
        }
    }

    pub fn memory_limit(
        self,
        memory_limit: MemoryBound,
    ) -> RlmProtocolPluginConfigBuilder<I, MemoryBound, C> {
        RlmProtocolPluginConfigBuilder {
            charge: self.charge,
            instruction_limit: self.instruction_limit,
            memory_limit,
            on_memory_limit: self.on_memory_limit,
            channel: self.channel,
            durability: self.durability,
        }
    }

    /// Whether a cell's run can be saved; durable when this is not called.
    /// An ephemeral cell is never saved and does none of the work a save
    /// needs, so it cannot be checkpointed, moved or recovered, and its
    /// effects are not exactly once across crashes. Under the engine, whose
    /// turns are saved at every effect, an ephemeral cell completes only
    /// when it makes no recorded effect and carries no session; otherwise
    /// it is refused with [`lash_vm_client::RunRefusal::Ephemeral`].
    #[must_use]
    pub fn durability(mut self, durability: lash_vm_client::Durability) -> Self {
        self.durability = durability;
        self
    }
}

impl RlmProtocolPluginConfigBuilder<InstructionBound, MemoryBound, super::RlmChannel> {
    /// Available only once both bounds are chosen.
    pub fn build(self) -> RlmProtocolPluginConfig {
        RlmProtocolPluginConfig {
            code_renderer: crate::render::CodeRendererSlot::default(),
            render: crate::render::ResolvedRlmRender::standard().as_patch(),
            presentation: RlmPresentationConfig::standard(),
            tool_presentation: default_tool_presentation(),
            discovery: None,
            channel: self.channel,
            charge: self.charge,
            instruction_limit: self.instruction_limit,
            memory_limit: self.memory_limit,
            durability: self.durability,
            on_memory_limit: self.on_memory_limit,
            prompt_features: crate::protocol::RlmPromptFeatures::default(),
            max_output_chars: default_max_output_chars(),
            continue_as_soft_warn_tokens: default_continue_as_soft_warn_tokens(),
        }
    }
}

impl RlmProtocolPluginConfig {
    pub fn with_discovery(mut self, discovery: RlmDiscovery) -> Self {
        self.discovery = Some(discovery);
        self
    }

    /// Every execution bound is named and separately typed; there is no positional constructor
    /// to get them in the wrong order.
    pub fn builder() -> RlmProtocolPluginConfigBuilder {
        Self::standard()
    }

    /// Standard preset builder: complete standard print/preview render, images
    /// and decomposition on, 10,000 output
    /// characters, soft warning at 100,000 tokens, no discovery, and standard
    /// presentation. The historical values have no universal workload measurement.
    /// Execution budgets and channel are still explicit named inputs.
    pub fn standard() -> RlmProtocolPluginConfigBuilder {
        RlmProtocolPluginConfigBuilder {
            charge: super::Charge::Exact,
            instruction_limit: UnsetBound,
            memory_limit: UnsetBound,
            on_memory_limit: MemoryLimit::Recount,
            channel: UnsetChannel,
            durability: lash_vm_client::Durability::Durable,
        }
    }

    pub(crate) fn execution_bounds(&self) -> ExecutionBounds {
        ExecutionBounds::new(self.instruction_limit, self.memory_limit)
            .with_charge(self.charge)
            .with_durability(self.durability)
            .on_memory_limit(self.on_memory_limit)
    }

    /// The behaviour a session created in `dialect` under this
    /// configuration records: every behavioural choice it states, with the
    /// dialect's tool presenter and the config it admits.
    ///
    /// # Errors
    ///
    /// The dialect's tool presenter refuses [`Self::tool_presentation`].
    pub fn recorded_behaviour(
        &self,
        dialect: &crate::CellDialect,
    ) -> Result<RlmRecordedBehaviour, lash_tool_presenter_api::PresentationRefusal> {
        Ok(RlmRecordedBehaviour {
            charge: self.charge,
            instruction_limit: self.instruction_limit,
            memory_limit: self.memory_limit,
            durability: self.durability,
            on_memory_limit: self.on_memory_limit,
            prompt_features: self.prompt_features,
            max_output_chars: self.max_output_chars,
            continue_as_soft_warn_tokens: self.continue_as_soft_warn_tokens,
            discovery: self.discovery.clone(),
            render: crate::render::ResolvedRlmRender::resolve(&self.render, &Default::default())
                .as_patch(),
            presentation: self.presentation,
            tool_presenter: dialect.tool_presenter().admit(&self.tool_presentation)?,
        })
    }

    /// This configuration's physical slots under a session's recorded
    /// `behaviour`: what a session's plugin, driver and interpreter run.
    pub(crate) fn under_recorded_behaviour(mut self, behaviour: &RlmRecordedBehaviour) -> Self {
        self.charge = behaviour.charge;
        self.instruction_limit = behaviour.instruction_limit;
        self.memory_limit = behaviour.memory_limit;
        self.durability = behaviour.durability;
        self.on_memory_limit = behaviour.on_memory_limit;

        self.prompt_features = behaviour.prompt_features;
        self.max_output_chars = behaviour.max_output_chars;
        self.continue_as_soft_warn_tokens = behaviour.continue_as_soft_warn_tokens;
        self.discovery = behaviour.discovery.clone();
        self.render = behaviour.render.clone();
        self.presentation = behaviour.presentation;
        self.tool_presentation = behaviour.tool_presenter.config.clone();
        self
    }
}

fn deserialize_recorded_render<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<lash_rlm_types::RlmRenderPatch, D::Error> {
    let render = <lash_rlm_types::RlmRenderPatch as serde::Deserialize>::deserialize(deserializer)?;
    if crate::ResolvedRlmRender::resolve(&render, &Default::default()).as_patch() != render {
        return Err(serde::de::Error::custom(
            "recorded RLM render must state a complete base",
        ));
    }
    Ok(render)
}
