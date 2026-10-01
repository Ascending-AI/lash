//! The core owner's config commands (FIG-4379): the changes a session's
//! model, reasoning, attachment acceptance, generation, execution
//! controls (turn budget, autonomy, no-progress budget, charge safety;
//! FIG-4376) and tool access admit.
//!
//! The core owner is not a plugin. Its share of the session's config is the
//! [`CoreConfig`] view of the config head and its reducers are the functions
//! here. [`SetModel`] names a key; its reducer mints the key's binding
//! through the host's models when the transaction resolves, and the
//! resolution records it (FIG-4374). The final candidate's reasoning is
//! judged against the model it records.
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
    /// The host's models register no model under the key a [`SetModel`]
    /// named.
    UnknownModel { key: crate::ModelKey },
    /// The candidate's reasoning does not fit the capability its recorded
    /// model declares.
    ReasoningRefused {
        key: crate::ModelKey,
        reasoning: crate::ReasoningSelection,
        category: crate::provider::ModelEffortValidationCategory,
        message: String,
    },
    /// A charge-safety policy that accepts more unsafe retries than Lash admits.
    UnsafeRetriesAboveCeiling { requested: u8, ceiling: u8 },
    /// The candidate selects a reasoning but records no model to run it
    /// with.
    ReasoningWithoutModel {
        reasoning: crate::ReasoningSelection,
    },
}

impl std::fmt::Display for CoreConfigRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownModel { key } => {
                write!(formatter, "the host's models register no model `{key}`")
            }
            Self::ReasoningRefused { message, .. } => formatter.write_str(message),
            Self::ReasoningWithoutModel { reasoning } => write!(
                formatter,
                "reasoning {reasoning:?} needs a model, and the session records none"
            ),
            Self::UnsafeRetriesAboveCeiling { requested, ceiling } => write!(
                formatter,
                "charge safety accepts {requested} unsafe retries, above the ceiling of {ceiling}"
            ),
        }
    }
}

impl std::error::Error for CoreConfigRefusal {}

impl CoreConfigOwner {
    /// Validate the charge safety stated at creation or by `SetChargeSafety`.
    pub fn validate_charge_safety(
        policy: &crate::ChargeSafetyPolicy,
    ) -> Result<(), CoreConfigRefusal> {
        if let crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries, ..
        } = policy
            && *max_unsafe_retries > crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES
        {
            return Err(CoreConfigRefusal::UnsafeRetriesAboveCeiling {
                requested: *max_unsafe_retries,
                ceiling: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES,
            });
        }
        Ok(())
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

    /// The core candidate is judged by [`validate_candidate`] when a
    /// transaction changes it.
    fn validate(
        &self,
        _value: &CoreConfig,
        _base: Option<&CoreConfig>,
        _facts: &CandidateFacts<'_>,
    ) -> Result<(), CoreConfigRefusal> {
        Ok(())
    }
}

/// The model the session runs from here on, by the host's key. The host's
/// models mint the key's binding when the transaction resolves, even when
/// the key is the session's current one, and the session records exactly
/// that binding; a key they do not register is refused typed. It keeps the
/// session's reasoning, which [`SetReasoning`] changes, and its
/// attachment-acceptance snapshot (ADR 0026), which only
/// [`SetAttachmentAcceptance`] replaces.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetModel {
    pub model: crate::ModelKey,
}

impl ConfigCommand for SetModel {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_model";
}

/// The reasoning the session runs its model with from here on. It is judged
/// against the model the transaction's final candidate records, so one
/// transaction can change the model and a reasoning only the new model
/// supports together.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetReasoning {
    pub reasoning: crate::ReasoningSelection,
}

impl ConfigCommand for SetReasoning {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_reasoning";
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

/// The tool-call limit the session runs under from its next root (FIG-4546):
/// the total one cell may make, and the number a process may hold at once. A
/// root already admitted keeps the limit it recorded, and so does a process
/// already started: work the journal accepted is never refused by a later
/// change.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetMaxToolCalls {
    pub max_tool_calls: crate::MaxToolCalls,
}

impl ConfigCommand for SetMaxToolCalls {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_max_tool_calls";
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

/// Judge the core candidate a transaction changed: its reasoning must fit
/// the capability of the model it records. Nothing is judged when neither
/// the model nor the reasoning moved.
pub(super) fn validate_candidate(
    base: &CoreConfig,
    candidate: &CoreConfig,
) -> Result<(), CoreConfigRefusal> {
    if base.model == candidate.model {
        return Ok(());
    }
    let Some(model) = candidate.model.as_ref() else {
        return Ok(());
    };
    model
        .validate_reasoning()
        .map_err(|refused| CoreConfigRefusal::ReasoningRefused {
            key: refused.key,
            reasoning: refused.reasoning,
            category: refused.category,
            message: refused.message,
        })
}

fn changed(core: CoreConfig) -> Result<OwnerChange<CoreConfig, ()>, CoreConfigRefusal> {
    Ok(OwnerChange {
        recorded: core,
        output: (),
    })
}

/// The core owner's registration: the owner and every core command.
pub(super) fn registration() -> Result<RegisteredOwner, ConfigRegistrationError> {
    let mut reg = ConfigRegistrar::new(CORE_CONFIG_OWNER);
    reg.owner(CoreConfigOwner)?;
    reg.models_command::<SetModel>(|core, command, models| {
        let recorded =
            models
                .snapshot(&command.model)
                .map_err(|_| CoreConfigRefusal::UnknownModel {
                    key: command.model.clone(),
                })?;
        let reasoning = core
            .model
            .as_ref()
            .map(|model| model.reasoning.clone())
            .unwrap_or_default();
        changed(CoreConfig {
            model: Some(crate::ModelConfig {
                model: recorded,
                reasoning,
            }),
            ..core.clone()
        })
    })?;
    reg.command::<SetReasoning>(|core, command| {
        let mut next = core.clone();
        let Some(model) = next.model.as_mut() else {
            return Err(CoreConfigRefusal::ReasoningWithoutModel {
                reasoning: command.reasoning,
            });
        };
        model.reasoning = command.reasoning;
        changed(next)
    })?;
    reg.command::<SetAttachmentAcceptance>(|core, command| {
        changed(CoreConfig {
            attachment_acceptance: std::sync::Arc::new(command.acceptance),
            ..core.clone()
        })
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
    reg.command::<SetMaxToolCalls>(|core, command| {
        changed(CoreConfig {
            max_tool_calls: command.max_tool_calls,
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
        CoreConfigOwner::validate_charge_safety(&command.charge_safety)?;
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
