use super::{ExecutionBounds, InstructionBound, MemoryBound, RlmAbilities, RlmLanguageFeatures};

/// A host's RLM protocol configuration.
///
/// The physical slots (the code renderer) are bound live. Every behavioural
/// choice — the execution bounds, the Lashlang abilities and language
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
    /// The discovery operation a new session records, if any.
    #[serde(skip)]
    pub discovery: Option<lash_core::ToolDiscovery>,
    /// Session-pinned transport used for model-authored programs.
    pub channel: super::RlmChannel,
    pub instruction_limit: InstructionBound,
    pub memory_limit: MemoryBound,
    #[serde(default)]
    pub prompt_features: crate::protocol::RlmPromptFeatures,
    #[serde(default)]
    pub lashlang_abilities: RlmAbilities,
    /// Lashlang language features offered to the model. Absent from a host's
    /// config means the RLM default (label annotations on); a host that spells
    /// a feature `false` gets it off end to end — the plugin never re-enables
    /// it (FIG-2768).
    #[serde(default = "default_lashlang_language_features")]
    pub lashlang_language_features: RlmLanguageFeatures,
    #[serde(default = "default_max_output_chars")]
    pub max_output_chars: usize,
    #[serde(default = "default_continue_as_soft_warn_tokens")]
    pub continue_as_soft_warn_tokens: Option<usize>,
    /// How each step's prompt and state are built: the chronological policy,
    /// or relay (FIG-4441), where each step hands the next only the context
    /// and vars it passes to `control.next`.
    #[serde(default)]
    pub execution_policy: RlmExecutionPolicy,
}

/// An RLM session's execution policy, recorded at creation.
///
/// `Chronological` renders every prior cell and its output as history and
/// keeps the REPL across steps. `Relay` (FIG-4441) keeps nothing between steps
/// but the arguments of the last committed `control.next` call: its `context`
/// is the next prompt's working memory and its `vars` rebuild the REPL, which
/// is otherwise wiped every step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RlmExecutionPolicy {
    #[default]
    Chronological,
    Relay,
}

impl RlmExecutionPolicy {
    pub fn is_relay(self) -> bool {
        matches!(self, Self::Relay)
    }

    fn is_chronological(&self) -> bool {
        matches!(self, Self::Chronological)
    }
}

fn default_max_output_chars() -> usize {
    10_000
}

fn default_continue_as_soft_warn_tokens() -> Option<usize> {
    Some(100_000)
}

/// The RLM protocol's default language-feature set. This is the single site
/// that decides the default: the builder and serde both read it, and the
/// plugin factory applies the host's value verbatim.
fn default_lashlang_language_features() -> RlmLanguageFeatures {
    RlmLanguageFeatures::default().with_label_annotations()
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
    pub lashlang_abilities: RlmAbilities,
    pub lashlang_language_features: RlmLanguageFeatures,
    pub prompt_features: crate::protocol::RlmPromptFeatures,
    pub max_output_chars: usize,
    /// The prompt-token threshold of the soft context-budget warning, or
    /// `None` for no warning.
    pub continue_as_soft_warn_tokens: Option<usize>,
    /// The host operation the model discovers tools omitted from the prompt
    /// with, or `None` when every tool is inline.
    pub discovery_operation: Option<String>,
    /// The render the creating deployment configured: the base a run's
    /// render is resolved over, under the session's own render preferences
    /// (FIG-4527).
    pub render: lash_rlm_types::RlmRenderPatch,
    /// The session's execution policy. Absent means chronological.
    #[serde(default, skip_serializing_if = "RlmExecutionPolicy::is_chronological")]
    pub execution_policy: RlmExecutionPolicy,
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
            render: lash_rlm_types::RlmRenderPatch::default(),
            discovery: None,
            channel: self.channel,
            instruction_limit: self.instruction_limit,
            memory_limit: self.memory_limit,
            prompt_features: crate::protocol::RlmPromptFeatures::default(),
            lashlang_abilities: RlmAbilities::default(),
            lashlang_language_features: default_lashlang_language_features(),
            max_output_chars: default_max_output_chars(),
            continue_as_soft_warn_tokens: default_continue_as_soft_warn_tokens(),
            execution_policy: RlmExecutionPolicy::default(),
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
    /// every behavioural choice it states, with durable sleep enabled when
    /// the deployment has process lifecycle.
    pub fn recorded_behaviour(&self, process_lifecycle: bool) -> RlmRecordedBehaviour {
        let lashlang_abilities = if process_lifecycle {
            self.lashlang_abilities.with_sleep()
        } else {
            self.lashlang_abilities
        };
        RlmRecordedBehaviour {
            instruction_limit: self.instruction_limit,
            memory_limit: self.memory_limit,
            lashlang_abilities,
            lashlang_language_features: self.lashlang_language_features,
            prompt_features: self.prompt_features,
            max_output_chars: self.max_output_chars,
            continue_as_soft_warn_tokens: self.continue_as_soft_warn_tokens,
            discovery_operation: self
                .discovery
                .as_ref()
                .map(|discovery| discovery.operation.clone()),
            render: self.render.clone(),
            execution_policy: self.execution_policy,
        }
    }

    /// This configuration's physical slots under a session's recorded
    /// `behaviour`: what a session's plugin, driver and interpreter run.
    pub(crate) fn under_recorded_behaviour(mut self, behaviour: &RlmRecordedBehaviour) -> Self {
        self.instruction_limit = behaviour.instruction_limit;
        self.memory_limit = behaviour.memory_limit;
        self.lashlang_abilities = behaviour.lashlang_abilities;
        self.lashlang_language_features = behaviour.lashlang_language_features;
        self.prompt_features = behaviour.prompt_features;
        self.max_output_chars = behaviour.max_output_chars;
        self.continue_as_soft_warn_tokens = behaviour.continue_as_soft_warn_tokens;
        self.discovery = behaviour
            .discovery_operation
            .clone()
            .map(|operation| lash_core::ToolDiscovery { operation });
        self.render = behaviour.render.clone();
        self.execution_policy = behaviour.execution_policy;
        self
    }

    /// The same configuration under `policy` (FIG-4441).
    pub fn with_execution_policy(mut self, policy: RlmExecutionPolicy) -> Self {
        self.execution_policy = policy;
        self
    }

    pub fn with_lashlang_abilities(mut self, abilities: impl Into<RlmAbilities>) -> Self {
        self.lashlang_abilities = abilities.into();
        self
    }

    pub fn with_lashlang_language_features(
        mut self,
        language_features: impl Into<RlmLanguageFeatures>,
    ) -> Self {
        self.lashlang_language_features = language_features.into();
        self
    }
}
