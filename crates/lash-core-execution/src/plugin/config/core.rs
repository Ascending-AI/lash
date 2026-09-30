//! The core owner's config commands (FIG-4379): the changes a session's
//! provider, model, prompt, generation, execution controls (turn budget,
//! autonomy, no-progress budget, charge safety; FIG-4376) and tool access
//! admit.
//!
//! The core owner is not a plugin. Its share of the session's config is the
//! [`CoreConfig`] view of the config head, its reducers are the functions
//! here, and its candidate is validated by the runtime, which alone knows
//! which provider routes this host serves.
//!
//! Adding a core command:
//! 1. Give the fact a field on [`CoreConfig`], read by `CoreConfig::of` and
//!    written by `CoreConfig::apply_to`, so the command changes the recorded
//!    head and nothing else holds it.
//! 2. Declare the command as a wire type implementing [`ConfigCommand`] with
//!    `Owner = CoreConfigOwner` and a snake-case `NAME` unique among core
//!    commands, and register its reducer in [`registration`]. A reducer maps
//!    the recorded `CoreConfig` to the next one and sees nothing else.
//! 3. Re-export it from `lash::config`. The catalog
//!    (`SessionConfigAdmin::commands`) is generated from the registrations,
//!    so it needs no edit; the facade's catalog law lists every core
//!    command name and moves with it.
//! 4. Bump [`CORE_CONFIG_IMPLEMENTATION`] only when an existing reducer's
//!    behavior changes; a transaction recorded under the old identity then
//!    waits for a build that runs it. Adding a command changes no existing
//!    reducer.

use super::{
    CORE_CONFIG_OWNER, CandidateFacts, ConfigCommand, ConfigOwner, ConfigRegistrar,
    ConfigRegistrationError, CoreConfig, CreationFacts, OwnerChange, RegisteredOwner,
};

/// The core owner's reducer implementation identity.
pub const CORE_CONFIG_IMPLEMENTATION: &str = "lash-core-config:1";

/// The owner of the session's core config.
#[derive(Clone, Copy, Debug, Default)]
pub struct CoreConfigOwner;

/// Why the core owner refused a candidate.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoreConfigRefusal {
    /// No provider of this host serves the candidate's route.
    UnservableRoute {
        code: crate::provider::ConfigRefusalCode,
        provider_id: String,
        model: String,
    },
    /// A charge-safety policy that accepts more unsafe retries than Lash
    /// ever buys: recorded, it would claim retries the provider handle
    /// clamps away.
    UnsafeRetriesAboveCeiling { requested: u8, ceiling: u8 },
}

impl std::fmt::Display for CoreConfigRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnservableRoute {
                code,
                provider_id,
                model,
            } => write!(
                formatter,
                "no provider of this host serves provider `{provider_id}` with model `{model}`: \
                 {code}"
            ),
            Self::UnsafeRetriesAboveCeiling { requested, ceiling } => write!(
                formatter,
                "charge safety accepts {requested} unsafe retries, above the ceiling of {ceiling}"
            ),
        }
    }
}

impl ConfigOwner for CoreConfigOwner {
    type Create = CoreConfig;
    type Recorded = CoreConfig;
    type Refusal = CoreConfigRefusal;

    fn implementation(&self) -> &str {
        CORE_CONFIG_IMPLEMENTATION
    }

    /// The core config is created from the session's policy, never through
    /// the owner: this records nothing.
    fn create(
        &self,
        _input: Option<CoreConfig>,
        _facts: CreationFacts<'_, CoreConfig>,
    ) -> Result<Option<CoreConfig>, CoreConfigRefusal> {
        Ok(None)
    }

    /// The core candidate is validated by the runtime's route check.
    fn validate(
        &self,
        _value: &CoreConfig,
        _base: Option<&CoreConfig>,
        _facts: &CandidateFacts<'_>,
    ) -> Result<(), CoreConfigRefusal> {
        Ok(())
    }
}

/// The provider route the session runs from here on. A route this host
/// cannot serve is refused.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetProvider {
    pub provider_id: String,
}

impl ConfigCommand for SetProvider {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_provider";
}

/// The model the session runs from here on. It keeps the session's
/// attachment-acceptance snapshot (ADR 0026); only
/// [`SetAttachmentAcceptance`] replaces that.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetModel {
    pub model: crate::ModelSpec,
}

impl ConfigCommand for SetModel {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_model";
}

/// Replace the session's attachment-acceptance snapshot, whole.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetAttachmentAcceptance {
    pub acceptance: crate::provider::AttachmentCapabilitySnapshot,
}

impl ConfigCommand for SetAttachmentAcceptance {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_attachment_acceptance";
}

/// Replace the session's prompt layer, whole.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetPrompt {
    pub prompt: crate::PromptLayer,
}

impl ConfigCommand for SetPrompt {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_prompt";
}

/// Set the prompt layer's template.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetPromptTemplate {
    pub template: crate::PromptTemplate,
}

impl ConfigCommand for SetPromptTemplate {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_prompt_template";
}

/// Remove the prompt layer's template.
#[derive(
    Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ClearPromptTemplate {}

impl ConfigCommand for ClearPromptTemplate {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "clear_prompt_template";
}

/// Add one contribution to the prompt layer.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddPromptContribution {
    pub contribution: crate::PromptContribution,
}

impl ConfigCommand for AddPromptContribution {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "add_prompt_contribution";
}

/// Replace every contribution of one prompt slot.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplacePromptSlot {
    pub slot: crate::PromptSlot,
    pub contributions: Vec<crate::PromptContribution>,
}

impl ConfigCommand for ReplacePromptSlot {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "replace_prompt_slot";
}

/// Remove every contribution of one prompt slot.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClearPromptSlot {
    pub slot: crate::PromptSlot,
}

impl ConfigCommand for ClearPromptSlot {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "clear_prompt_slot";
}

/// Layer generation options over the session's: merged per option, or
/// replacing them, as the overlay says.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetGeneration {
    pub generation: crate::GenerationOverlay,
}

impl ConfigCommand for SetGeneration {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_generation";
}

/// The per-turn budget the session runs under from its next root.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetTurnBudget {
    pub turn_budget: crate::TurnBudget,
}

impl ConfigCommand for SetTurnBudget {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_turn_budget";
}

/// Whether the session's turns run autonomously from its next root. Every
/// value is admissible.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetAutonomy {
    pub autonomous: bool,
}

impl ConfigCommand for SetAutonomy {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_autonomy";
}

/// The consecutive unproductive attempts the session allows from its next
/// root. A zero bound does not decode, so it is refused at submit; every
/// decodable value is admissible.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetNoProgressBudget {
    pub no_progress_budget: crate::NoProgressBudget,
}

impl ConfigCommand for SetNoProgressBudget {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_no_progress_budget";
}

/// The charge-safety policy the session runs under from its next root. A
/// policy accepting more unsafe retries than
/// [`ChargeSafetyPolicy::MAX_UNSAFE_RETRIES`](crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES)
/// is refused.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetChargeSafety {
    pub charge_safety: crate::ChargeSafetyPolicy,
}

impl ConfigCommand for SetChargeSafety {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_charge_safety";
}

/// Replace the session's tool authority, whole.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetToolAccess {
    pub access: crate::SessionToolAccess,
}

impl ConfigCommand for SetToolAccess {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_tool_access";
}

fn changed(core: CoreConfig) -> Result<OwnerChange<CoreConfig, ()>, CoreConfigRefusal> {
    Ok(OwnerChange {
        recorded: core,
        output: (),
    })
}

fn edit_prompt(
    core: &CoreConfig,
    edit: impl FnOnce(&mut crate::PromptLayer),
) -> Result<OwnerChange<CoreConfig, ()>, CoreConfigRefusal> {
    let mut next = core.clone();
    let mut prompt = next.prompt_layer();
    edit(&mut prompt);
    next.prompt = Some(prompt);
    changed(next)
}

/// The core owner's registration: the owner and every core command.
pub(super) fn registration() -> Result<RegisteredOwner, ConfigRegistrationError> {
    let mut reg = ConfigRegistrar::new(CORE_CONFIG_OWNER);
    reg.owner(CoreConfigOwner)?;
    reg.command::<SetProvider>(|core, command| {
        changed(CoreConfig {
            provider_id: command.provider_id,
            ..core.clone()
        })
    })?;
    reg.command::<SetModel>(|core, command| {
        let mut model = command.model;
        model.capability.attachment_acceptance =
            core.model.capability.attachment_acceptance.clone();
        changed(CoreConfig {
            model,
            ..core.clone()
        })
    })?;
    reg.command::<SetAttachmentAcceptance>(|core, command| {
        let mut next = core.clone();
        next.model.capability.attachment_acceptance = std::sync::Arc::new(command.acceptance);
        changed(next)
    })?;
    reg.command::<SetPrompt>(|core, command| {
        changed(CoreConfig {
            prompt: Some(command.prompt),
            ..core.clone()
        })
    })?;
    reg.command::<SetPromptTemplate>(|core, command| {
        edit_prompt(core, |prompt| prompt.template = Some(command.template))
    })?;
    reg.command::<ClearPromptTemplate>(|core, _| {
        edit_prompt(core, |prompt| prompt.template = None)
    })?;
    reg.command::<AddPromptContribution>(|core, command| {
        edit_prompt(core, |prompt| prompt.add_contribution(command.contribution))
    })?;
    reg.command::<ReplacePromptSlot>(|core, command| {
        edit_prompt(core, |prompt| {
            prompt.replace_slot(command.slot, command.contributions)
        })
    })?;
    reg.command::<ClearPromptSlot>(|core, command| {
        edit_prompt(core, |prompt| prompt.clear_slot(command.slot))
    })?;
    reg.command::<SetGeneration>(|core, command| {
        changed(CoreConfig {
            generation: command.generation.resolve(&core.generation),
            ..core.clone()
        })
    })?;
    reg.command::<SetTurnBudget>(|core, command| {
        changed(CoreConfig {
            turn_budget: command.turn_budget,
            ..core.clone()
        })
    })?;
    reg.command::<SetAutonomy>(|core, command| {
        changed(CoreConfig {
            autonomous: command.autonomous,
            ..core.clone()
        })
    })?;
    reg.command::<SetNoProgressBudget>(|core, command| {
        changed(CoreConfig {
            no_progress_budget: command.no_progress_budget,
            ..core.clone()
        })
    })?;
    reg.command::<SetChargeSafety>(|core, command| {
        if let crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries, ..
        } = command.charge_safety
            && max_unsafe_retries > crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES
        {
            return Err(CoreConfigRefusal::UnsafeRetriesAboveCeiling {
                requested: max_unsafe_retries,
                ceiling: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES,
            });
        }
        changed(CoreConfig {
            charge_safety: command.charge_safety,
            ..core.clone()
        })
    })?;
    reg.command::<SetToolAccess>(|core, command| {
        changed(CoreConfig {
            tool_access: command.access,
            ..core.clone()
        })
    })?;
    reg.owner
        .ok_or_else(|| ConfigRegistrationError::DuplicateOwner {
            plugin_id: CORE_CONFIG_OWNER.to_string(),
        })
}
