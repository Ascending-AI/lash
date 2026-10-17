//! The core owner's config commands (FIG-4379): the changes a session's
//! model, reasoning, attachment acceptance, generation, execution
//! controls (turn budget, no-progress budget, charge safety;
//! FIG-4376), tool access and the host's prompt plan (ADR 0133) admit.
//!
//! The core owner is not a plugin. Its share of the session's config is the
//! [`CoreConfig`] view of the config head and its reducers are the functions
//! here. [`SetLlmProfile`] names a key; its reducer mints the key's binding
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
//! 4. A reducer has no identity of its own: a queued transaction is resolved
//!    by the build that admits its command run. Adding a command changes no
//!    existing reducer.

use super::CandidateFacts;
use super::ConfigCommand;
use super::ConfigOwner;
use super::ConfigRegistrar;
use super::OwnerChange;
use super::RegisteredOwner;
use lash_core_store::config_transaction::CORE_CONFIG_OWNER;
use lash_core_store::config_transaction::CoreConfig;
use lash_execution_contracts::config_error::ConfigRegistrationError;
use lash_execution_contracts::config_error::CoreConfigRefusal;

/// The owner of the session's core config.
#[derive(Clone, Copy, Debug, Default)]
pub struct CoreConfigOwner;

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

impl CoreConfigOwner {
    /// The core owner's `refusal` of a session's creation.
    pub fn creation_refusal(refusal: CoreConfigRefusal) -> super::ConfigRefusal {
        lash_core_store::config_transaction::ConfigRefusal::by_owner(
            super::CORE_CONFIG_OWNER,
            lash_core_store::config_transaction::RefusalSite::Creation,
            &refusal,
        )
    }
}

impl ConfigOwner for CoreConfigOwner {
    type Create = CoreConfig;
    type Recorded = CoreConfig;
    type Refusal = CoreConfigRefusal;
    type RunOptions = super::NoRunOptions;

    /// The core config is created from the session's policy, never through
    /// the owner: this records nothing.
    fn create(&self, _input: Option<CoreConfig>) -> Result<Option<CoreConfig>, CoreConfigRefusal> {
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

    /// A run overrides the core share through its spec's typed overrides,
    /// never through options.
    fn apply_run_options(
        &self,
        recorded: &CoreConfig,
        _options: super::NoRunOptions,
    ) -> Result<CoreConfig, CoreConfigRefusal> {
        Ok(recorded.clone())
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
pub struct SetLlmProfile {
    pub model: crate::LlmProfileKey,
}

impl ConfigCommand for SetLlmProfile {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_llm_profile";
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
    pub acceptance: lash_core_llm::provider::AttachmentCapabilitySnapshot,
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

/// The per-turn budget the session runs under from its next run.
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

/// The tool-call limit the session runs under from its next run (FIG-4546):
/// the total one cell may make, and the number a process may hold at once. A
/// run already admitted keeps the limit it recorded, and so does a process
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

/// The consecutive unproductive attempts the session allows from its next
/// run. A zero bound does not decode, so it is refused at submit; every
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

/// The charge-safety policy the session runs under from its next run. A
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

/// Replace the host's prompt plan, whole, from the session's next run (ADR
/// 0133): the section order, the placements that override plugin defaults,
/// and the composition limits. A plan that orders or places a section twice,
/// or states a per-section limit above its total, is refused. Whether each
/// named section is registered is judged against the session's catalog when
/// the command resolves. A removed section in an already recorded plan is
/// skipped and recorded when a call resolves it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetPromptPlan {
    pub plan: crate::prompt_sections::PromptPlan,
}

impl ConfigCommand for SetPromptPlan {
    type Owner = CoreConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_prompt_plan";
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
    reg.llm_profiles_command::<SetLlmProfile>(|core, command, models| {
        let recorded =
            models
                .snapshot(&command.model)
                .map_err(|_| CoreConfigRefusal::UnknownLlmProfile {
                    key: command.model.clone(),
                })?;
        let reasoning = core
            .model
            .as_ref()
            .map(|model| model.reasoning.clone())
            .unwrap_or_default();
        changed(CoreConfig {
            model: Some(crate::LlmProfileConfig {
                model: recorded,
                reasoning,
            }),
            ..core.clone()
        })
    })?;
    reg.command::<SetReasoning>(|core, command| {
        let mut next = core.clone();
        let Some(model) = next.model.as_mut() else {
            return Err(CoreConfigRefusal::ReasoningWithoutLlmProfile {
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
    reg.command::<SetPromptPlan>(|core, command| {
        command
            .plan
            .validate()
            .map_err(|error| CoreConfigRefusal::PromptPlanRefused { error })?;
        changed(CoreConfig {
            prompt_plan: command.plan,
            ..core.clone()
        })
    })?;
    reg.owner
        .ok_or_else(|| ConfigRegistrationError::DuplicateOwner {
            plugin_id: CORE_CONFIG_OWNER.to_string(),
        })
}
