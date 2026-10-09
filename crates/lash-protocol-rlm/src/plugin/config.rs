use super::{ExecutionBounds, InstructionBound, MemoryBound};

/// Prompt and transcript presentation. These choices are pinned with protocol behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlmPresentationConfig {
    pub tools: lash_sansio::ToolPresentationConfig,
    pub max_inline_keys: usize,
    pub max_tool_call_records: usize,
    pub max_inline_scalar_bytes: usize,
    /// How many binding names a cell's record keeps of what the cell did to
    /// the session's bindings; it counts the rest.
    pub max_binding_change_names: usize,
}
impl Default for RlmPresentationConfig {
    fn default() -> Self {
        Self::standard()
    }
}
impl RlmPresentationConfig {
    /// Standard preset: standard tool/schema presentation, 12 inline
    /// catalogue keys, 128 tool-call records, 64 KiB inline scalar bodies and
    /// 32 binding-change names.
    /// These historical presentation cuts have no universal workload measurement.
    pub const fn standard() -> Self {
        Self {
            tools: lash_sansio::ToolPresentationConfig::standard(),
            max_inline_keys: 12,
            max_tool_call_records: 128,
            max_inline_scalar_bytes: 64 * 1024,
            max_binding_change_names: 32,
        }
    }
}

/// A host's RLM protocol configuration.
///
/// The physical slots (the code renderer) are bound live. Every behavioural
/// choice — the execution bounds, the Lash VM language
/// features, the prompt features, discovery, the output limit, the soft
/// context-budget warning and the render — is this deployment's creation
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
    /// The discovery operation a new session records, if any.
    #[serde(skip)]
    pub discovery: Option<lash_core::ToolDiscovery>,
    /// Session-pinned transport used for model-authored programs.
    pub channel: super::RlmChannel,
    pub instruction_limit: InstructionBound,
    pub memory_limit: MemoryBound,
    #[serde(default)]
    pub prompt_features: crate::protocol::RlmPromptFeatures,
    #[serde(default = "default_max_output_chars")]
    pub max_output_chars: usize,
    #[serde(default = "default_continue_as_soft_warn_tokens")]
    pub continue_as_soft_warn_tokens: Option<usize>,
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
    pub instruction_limit: InstructionBound,
    pub memory_limit: MemoryBound,
    pub prompt_features: crate::protocol::RlmPromptFeatures,
    pub max_output_chars: usize,
    /// The prompt-token threshold of the soft context-budget warning, or
    /// `None` for no warning.
    #[serde(deserialize_with = "serde::Deserialize::deserialize")]
    pub continue_as_soft_warn_tokens: Option<usize>,
    /// The host operation the model discovers tools omitted from the prompt
    /// with, or `None` when every tool is inline.
    #[serde(deserialize_with = "serde::Deserialize::deserialize")]
    pub discovery_operation: Option<String>,
    /// The render the creating deployment configured: the base a run's
    /// render is resolved over, under the session's own render preferences
    /// (FIG-4527).
    #[serde(deserialize_with = "deserialize_recorded_render")]
    pub render: lash_rlm_types::RlmRenderPatch,
    pub presentation: RlmPresentationConfig,
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
    instruction_limit: I,
    memory_limit: M,
    channel: C,
}

impl<I, M, C> RlmProtocolPluginConfigBuilder<I, M, C> {
    pub fn channel(
        self,
        channel: super::RlmChannel,
    ) -> RlmProtocolPluginConfigBuilder<I, M, super::RlmChannel> {
        RlmProtocolPluginConfigBuilder {
            instruction_limit: self.instruction_limit,
            memory_limit: self.memory_limit,
            channel,
        }
    }

    pub fn instruction_limit(
        self,
        instruction_limit: InstructionBound,
    ) -> RlmProtocolPluginConfigBuilder<InstructionBound, M, C> {
        RlmProtocolPluginConfigBuilder {
            instruction_limit,
            memory_limit: self.memory_limit,
            channel: self.channel,
        }
    }

    pub fn memory_limit(
        self,
        memory_limit: MemoryBound,
    ) -> RlmProtocolPluginConfigBuilder<I, MemoryBound, C> {
        RlmProtocolPluginConfigBuilder {
            instruction_limit: self.instruction_limit,
            memory_limit,
            channel: self.channel,
        }
    }
}

impl RlmProtocolPluginConfigBuilder<InstructionBound, MemoryBound, super::RlmChannel> {
    /// Available only once both bounds are chosen.
    pub fn build(self) -> RlmProtocolPluginConfig {
        RlmProtocolPluginConfig {
            code_renderer: crate::render::CodeRendererSlot::default(),
            render: crate::render::ResolvedRlmRender::standard().as_patch(),
            presentation: RlmPresentationConfig::standard(),
            discovery: None,
            channel: self.channel,
            instruction_limit: self.instruction_limit,
            memory_limit: self.memory_limit,
            prompt_features: crate::protocol::RlmPromptFeatures::default(),
            max_output_chars: default_max_output_chars(),
            continue_as_soft_warn_tokens: default_continue_as_soft_warn_tokens(),
        }
    }
}

impl RlmProtocolPluginConfig {
    pub fn with_discovery(mut self, discovery: lash_core::ToolDiscovery) -> Self {
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
            instruction_limit: UnsetBound,
            memory_limit: UnsetBound,
            channel: UnsetChannel,
        }
    }

    pub(crate) fn execution_bounds(&self) -> ExecutionBounds {
        ExecutionBounds::new(self.instruction_limit, self.memory_limit)
    }

    /// The behaviour a session created under this configuration records:
    /// every behavioural choice it states.
    pub fn recorded_behaviour(&self) -> RlmRecordedBehaviour {
        RlmRecordedBehaviour {
            instruction_limit: self.instruction_limit,
            memory_limit: self.memory_limit,
            prompt_features: self.prompt_features,
            max_output_chars: self.max_output_chars,
            continue_as_soft_warn_tokens: self.continue_as_soft_warn_tokens,
            discovery_operation: self
                .discovery
                .as_ref()
                .map(|discovery| discovery.operation.clone()),
            render: crate::render::ResolvedRlmRender::resolve(&self.render, &Default::default())
                .as_patch(),
            presentation: self.presentation,
        }
    }

    /// This configuration's physical slots under a session's recorded
    /// `behaviour`: what a session's plugin, driver and interpreter run.
    pub(crate) fn under_recorded_behaviour(mut self, behaviour: &RlmRecordedBehaviour) -> Self {
        self.instruction_limit = behaviour.instruction_limit;
        self.memory_limit = behaviour.memory_limit;

        self.prompt_features = behaviour.prompt_features;
        self.max_output_chars = behaviour.max_output_chars;
        self.continue_as_soft_warn_tokens = behaviour.continue_as_soft_warn_tokens;
        self.discovery = behaviour
            .discovery_operation
            .clone()
            .map(|operation| lash_core::ToolDiscovery { operation });
        self.render = behaviour.render.clone();
        self.presentation = behaviour.presentation;
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
