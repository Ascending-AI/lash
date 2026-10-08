//! The required durable session configuration and its pending observer change.

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PersistedSessionConfig {
    /// The recorded model selection; `None` for a session that has selected
    /// no model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<crate::LlmProfileConfig>,
    /// The session's attachment-acceptance rules (ADR 0026), recorded apart
    /// from the model so a model change keeps them.
    #[serde(
        default,
        skip_serializing_if = "crate::provider::AttachmentCapabilitySnapshot::is_empty_arc"
    )]
    pub attachment_acceptance: std::sync::Arc<crate::provider::AttachmentCapabilitySnapshot>,
    /// The bound on protocol iterations per turn (FIG-4376). Like every
    /// execution control below it is session config: recorded at creation and
    /// snapshotted per root in its recorded
    /// [`ResolvedRun`](crate::run_spec::ResolvedRun).
    pub turn_budget: crate::TurnBudget,
    /// The tool-call limit (FIG-4546): the total one cell may make and the
    /// number a process may hold at once. Required on the wire: a head that
    /// states none is refused at load, never defaulted.
    pub max_tool_calls: crate::MaxToolCalls,

    /// The bound on consecutive provider attempts within one turn that
    /// commit no successful execution.
    pub no_progress_budget: crate::NoProgressBudget,
    /// The session's appetite for duplicate provider billing.
    pub charge_safety: crate::ChargeSafetyPolicy,
    /// Generation controls required to continue a cold-loaded session with
    /// the options it last committed.
    #[serde(default)]
    pub generation: crate::GenerationOptions,
    /// Authority inputs needed to reconstruct the same tool policy on a
    /// stateless worker. Catalog membership remains separate host curation.
    pub tool_access: crate::SessionToolAccess,
    /// The host's prompt plan (ADR 0133): the section order, the placements
    /// that override plugin defaults, and the composition limits. Absent is
    /// the empty plan.
    #[serde(
        default,
        skip_serializing_if = "crate::prompt_sections::PromptPlan::is_default"
    )]
    pub prompt_plan: crate::prompt_sections::PromptPlan,
    /// Every plugin's recorded configuration namespace, the protocol's
    /// included (FIG-4379): created by its owner at creation, changed only by
    /// an owner-validated config transaction, and delivered unchanged on every
    /// open.
    /// The protocol turn options are a view of it. Required on the wire.
    pub plugin_config: crate::PluginConfig,
    /// The config's own compare-and-set revision (ADR 0101 §12): `0` at
    /// creation, `+1` per applied config transaction, and otherwise unchanged
    /// — in particular it does not move with `head_revision` on every commit.
    /// It is the value a config transaction's `expected_revision` is checked
    /// against, and it is required on the wire: a head written before
    /// the contract existed is refused at load, not defaulted.
    pub config_revision: u64,
    /// The applied change the session's config-change observers are still
    /// owed (FIG-5397): recorded by the commit that applies it, and retired
    /// by the session's next head commit once a plugin build delivered it.
    /// Only the head carries it: it is not config, and no config view of a
    /// session's state, a run's recorded config among them, holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undelivered_change: Option<Box<UndeliveredConfigChange>>,
}

/// A committed change of a session's policy its config-change observers
/// have not been delivered (FIG-5397). It is delivered at least once: a
/// delivery the session's head did not retire before its node was lost is
/// delivered again, under the same [`revision`](Self::revision).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct UndeliveredConfigChange {
    /// The config revision the change reached: its stable identity.
    pub revision: u64,
    /// The session's policy before the change.
    pub previous: crate::SessionPolicy,
    /// The session's policy the change committed.
    pub current: crate::SessionPolicy,
}

impl PersistedSessionConfig {
    /// The recorded model key, when the session has selected a model.
    pub fn profile_key(&self) -> Option<&crate::LlmProfileKey> {
        self.model.as_ref().map(crate::LlmProfileConfig::key)
    }

    /// The recorded wire model, when the session has selected a model.
    pub fn wire_model(&self) -> Option<&str> {
        self.model.as_ref().map(|model| model.model.wire_model())
    }

    /// The session policy this config records. Only the session binding is
    /// not config, and starts unbound.
    pub fn session_policy(&self) -> crate::SessionPolicy {
        let mut policy = crate::SessionPolicy::new(
            self.turn_budget,
            self.max_tool_calls,
            self.no_progress_budget,
        );
        policy.model = self.model.clone();
        policy.attachment_acceptance = self.attachment_acceptance.clone();
        policy.charge_safety = self.charge_safety.clone();
        policy.generation = self.generation.clone();
        policy
    }

    /// Builds an empty persisted config carrying the required per-turn budget
    /// and tool-call limit, the host's stall bound and the session's tool
    /// authority.
    ///
    /// Store implementors reading durable session heads populate the model
    /// fields from the row; the budget and the limit have no default by
    /// doctrine, so every construction names `TurnBudget::Bounded(n)` or
    /// `Unbounded`, a `MaxToolCalls`, and ambient or restricted tool
    /// authority, explicitly. The other execution
    /// controls start at the values
    /// [`SessionPolicy::new`](crate::SessionPolicy::new) states.
    pub fn new(
        turn_budget: crate::TurnBudget,
        max_tool_calls: crate::MaxToolCalls,
        no_progress_budget: crate::NoProgressBudget,
        tool_access: crate::SessionToolAccess,
    ) -> Self {
        let neutral = crate::SessionPolicy::new(turn_budget, max_tool_calls, no_progress_budget);
        Self {
            model: None,
            attachment_acceptance: std::sync::Arc::default(),
            turn_budget,
            max_tool_calls,
            no_progress_budget: neutral.no_progress_budget,
            charge_safety: neutral.charge_safety,
            generation: crate::GenerationOptions::default(),
            tool_access,
            prompt_plan: crate::prompt_sections::PromptPlan::default(),
            plugin_config: crate::PluginConfig::default(),
            config_revision: 0,
            undelivered_change: None,
        }
    }

    /// The config a session records from `policy` under `tool_access`. A
    /// policy carries no tool authority, so its caller states it: the
    /// creator's choice at creation, the recorded one anywhere else.
    pub fn from_policy(
        policy: &crate::SessionPolicy,
        tool_access: crate::SessionToolAccess,
    ) -> Self {
        Self {
            model: policy.model.clone(),
            attachment_acceptance: policy.attachment_acceptance.clone(),
            turn_budget: policy.turn_budget,
            max_tool_calls: policy.max_tool_calls,
            no_progress_budget: policy.no_progress_budget,
            charge_safety: policy.charge_safety.clone(),
            generation: policy.generation.clone(),
            tool_access,
            prompt_plan: crate::prompt_sections::PromptPlan::default(),
            // A `SessionPolicy` carries no plugin configuration; its creator
            // records what the owners resolved.
            plugin_config: crate::PluginConfig::default(),
            // A `SessionPolicy` does not carry the revision; the caller that
            // knows the durable value assigns it
            // (`persisted_session_config_from_state`).
            config_revision: 0,
            undelivered_change: None,
        }
    }
}
