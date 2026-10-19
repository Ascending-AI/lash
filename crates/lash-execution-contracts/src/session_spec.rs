use crate::*;
use lash_core_llm::provider::AttachmentCapabilitySnapshot;
use lash_core_llm::provider::ReasoningSelection;
/// A root creation's whole spec: [`SessionSpec::new`] takes the model key,
/// turn budget, tool-call limit and stall bound, the four parts nothing
/// defaults. Other fields have neutral values. Nothing stands beneath it: a
/// deployment keeps no default spec, so a host that wants one keeps its own
/// `SessionSpec` value and passes it (FIG-4594). Every creation states its own policy.
///
/// It selects a model by key; resolving the spec mints that key's binding
/// through the host's models, once, and the resulting policy records it.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSpec {
    pub model: LlmProfileKey,
    /// The reasoning the session runs its model with. `None` runs the
    /// provider's default.
    pub reasoning: Option<ReasoningSelection>,
    /// The attachment-acceptance rules the session renders attachments
    /// against (ADR 0026). `None` keeps the neutral rules.
    pub attachment_acceptance: Option<std::sync::Arc<AttachmentCapabilitySnapshot>>,
    pub turn_budget: TurnBudget,
    /// The tool-call limit: the total one cell may make, and the number a
    /// process may hold at once.
    pub max_tool_calls: MaxToolCalls,
    /// Bound on consecutive unproductive provider attempts, or the explicit
    /// opt-out [`NoProgressBudget::Unbounded`].
    pub no_progress_budget: NoProgressBudget,
    /// Duplicate-billing appetite. `None` keeps the charge-safe default.
    pub charge_safety: Option<ChargeSafetyPolicy>,
    /// Plugin-keyed, serializable creation options (FIG-4379). The core
    /// prompt plan is recorded separately. Each installed plugin creates its
    /// recorded namespace from its key, and only its owner's typed config
    /// commands change it afterwards. A key no installed plugin owns, or a
    /// value its owner refuses, fails the creation typed as
    /// `SessionError::SessionConfigRefused`.
    pub plugin_options: crate::PluginOptions,
    /// Generation intent for every LLM call the session makes, as a
    /// [`GenerationOverlay`] over the neutral options. `None` states none.
    pub generation: Option<GenerationOverlay>,
}

// `serde_json::Value` never holds NaN or an infinite number, so the plugin
// options' `PartialEq` is reflexive.
impl Eq for SessionSpec {}

impl SessionSpec {
    /// A root session's spec: the model it runs, by the host's key, its turn
    /// budget, its tool-call limit and its stall bound, the four parts
    /// nothing defaults. Other fields start at the neutral value
    /// [`SessionPolicy::new`] states.
    pub fn new(
        model: impl Into<LlmProfileKey>,
        turn_budget: TurnBudget,
        max_tool_calls: MaxToolCalls,
        no_progress_budget: NoProgressBudget,
    ) -> Self {
        Self {
            model: model.into(),
            reasoning: None,
            attachment_acceptance: None,
            turn_budget,
            max_tool_calls,
            no_progress_budget,
            charge_safety: None,
            plugin_options: crate::PluginOptions::default(),
            generation: None,
        }
    }

    /// The policy a run records from this spec alone: its stated fields
    /// over the neutral [`SessionPolicy::new`], its key minted through
    /// `models` now. The minted model and the reasoning it records are
    /// judged against the recorded capability, so an unsupported selection
    /// is refused here and nothing is created with it.
    pub fn resolve_root(
        &self,
        models: &dyn LlmProfiles,
    ) -> Result<SessionPolicy, SpecResolveError> {
        let model = LlmProfileConfig {
            model: models
                .snapshot(&self.model)
                .map_err(SpecResolveError::Model)?,
            reasoning: self.reasoning.clone().unwrap_or_default(),
        };
        model
            .validate_reasoning()
            .map_err(SpecResolveError::Reasoning)?;
        let mut policy = self.stated_root_policy();
        policy.model = Some(model);
        Ok(policy)
    }

    /// The policy fields of a root this spec states, with no model minted:
    /// what a host session-turn start carries beside its model key and
    /// reasoning, so the start states nothing a catalog derives.
    pub fn stated_root_policy(&self) -> SessionPolicy {
        let mut policy = SessionPolicy::new(
            self.turn_budget,
            self.max_tool_calls,
            self.no_progress_budget,
        );
        if let Some(acceptance) = self.attachment_acceptance.as_ref() {
            policy.attachment_acceptance = acceptance.clone();
        }
        if let Some(charge_safety) = self.charge_safety.as_ref() {
            policy.charge_safety = charge_safety.clone();
        }
        if let Some(generation) = self.generation.as_ref() {
            policy.generation = generation.resolve(&policy.generation);
        }
        policy
    }

    /// The model the session runs, by the host's key.
    pub fn model(mut self, key: impl Into<LlmProfileKey>) -> Self {
        self.model = key.into();
        self
    }

    /// The reasoning the session runs its model with.
    pub fn reasoning(mut self, reasoning: ReasoningSelection) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// The attachment-acceptance rules the session renders attachments
    /// against.
    pub fn attachment_acceptance(
        mut self,
        acceptance: std::sync::Arc<AttachmentCapabilitySnapshot>,
    ) -> Self {
        self.attachment_acceptance = Some(acceptance);
        self
    }

    pub fn turn_budget(mut self, turn_budget: TurnBudget) -> Self {
        self.turn_budget = turn_budget;
        self
    }

    /// The tool-call limit: the total one cell may make, and the number a
    /// process may hold at once.
    pub fn max_tool_calls(mut self, max_tool_calls: MaxToolCalls) -> Self {
        self.max_tool_calls = max_tool_calls;
        self
    }

    /// Bound consecutive provider attempts that commit no successful
    /// execution, or opt the session out of that bound.
    pub fn no_progress_budget(mut self, no_progress_budget: NoProgressBudget) -> Self {
        self.no_progress_budget = no_progress_budget;
        self
    }

    /// # Integrator class
    ///
    /// Host applications use this setting; protocol and provider implementors
    /// do not override it.
    pub fn charge_safety(mut self, charge_safety: ChargeSafetyPolicy) -> Self {
        self.charge_safety = Some(charge_safety);
        self
    }

    /// State `plugin_id`'s creation options, replacing what this spec stated
    /// for that plugin before.
    pub fn plugin<T: serde::Serialize>(
        mut self,
        plugin_id: impl Into<String>,
        options: T,
    ) -> Result<Self, serde_json::Error> {
        self.plugin_options.insert_typed(plugin_id, options)?;
        Ok(self)
    }

    /// State every plugin's creation options at once.
    pub fn plugin_options(mut self, plugin_options: crate::PluginOptions) -> Self {
        self.plugin_options = plugin_options;
        self
    }

    /// Layer generation options over the ones the session inherits.
    ///
    /// Options this call leaves unset keep their base value, so a spec that
    /// caps output tokens does not silently drop the temperature and seed
    /// its base pinned. Use
    /// [`replace_generation`](Self::replace_generation) or
    /// [`clear_generation`](Self::clear_generation) to discard what is
    /// inherited.
    pub fn generation(mut self, generation: crate::GenerationOptions) -> Self {
        self.generation = Some(GenerationOverlay::Merge(generation));
        self
    }

    /// Use exactly these generation options, discarding every inherited one.
    pub fn replace_generation(mut self, generation: crate::GenerationOptions) -> Self {
        self.generation = Some(GenerationOverlay::Replace(generation));
        self
    }

    /// Drop the inherited generation options and express none of your own.
    pub fn clear_generation(mut self) -> Self {
        self.generation = Some(GenerationOverlay::Replace(
            crate::GenerationOptions::default(),
        ));
        self
    }
}

/// Why a [`SessionSpec`] did not resolve to a policy.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SpecResolveError {
    /// The spec's model key has no binding on this deployment.
    #[error(transparent)]
    Model(LlmProfileUnavailable),
    /// The reasoning the spec records is one its model's recorded capability
    /// refuses.
    #[error(transparent)]
    Reasoning(crate::ReasoningRefused),
}
