//! Durable and host-reconciled session policy.

use std::sync::Arc;

use crate::provider::AttachmentCapabilitySnapshot;
use crate::{
    ChargeSafetyPolicy, LlmProfileConfig, MaxToolCalls, NoProgressBudget, SessionId, TurnBudget,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPolicy {
    /// The session's model selection: the binding the host's registry minted
    /// when the session adopted its key, and the reasoning it runs with.
    /// `None` until a model is selected; such a session cannot run a turn.
    pub model: Option<LlmProfileConfig>,
    /// The attachment-acceptance rules the session renders attachments
    /// against (ADR 0026). Session config of its own: a model change keeps
    /// them, and only an explicit change replaces them.
    pub attachment_acceptance: Arc<AttachmentCapabilitySnapshot>,
    pub session_id: Option<SessionId>,
    pub autonomous: bool,
    /// Required turn-budget decision. A host must choose either a non-zero
    /// bound or explicit unbounded execution; absence is never interpreted.
    pub turn_budget: TurnBudget,
    /// Required tool-call limit (FIG-4546): the total one cell may make, and
    /// the number a process may hold at once. A host must state it; there is
    /// no default and no built-in ceiling. Session config like the turn
    /// budget: recorded at creation, changed only by a config command, and
    /// read back from the record by every replay, redrive and reopen.
    pub max_tool_calls: MaxToolCalls,
    /// Bound on consecutive provider attempts within one turn that commit no
    /// successful execution.
    ///
    /// Session config like the turn budget (FIG-4376), recorded at creation.
    /// Its default is bounded, so a carrier that states none resolves to the
    /// bound rather than to a loop.
    pub no_progress_budget: NoProgressBudget,
    /// The session's appetite for duplicate provider billing: session config
    /// like the turn budget (FIG-4376). A carrier that states none resolves to
    /// the charge-safe default.
    pub charge_safety: ChargeSafetyPolicy,
    /// Caller-owned generation intent applied to every LLM call this session
    /// makes. It lives on the policy rather than on a single turn because it
    /// is session-wide truth: child sessions resolve their spec against this
    /// policy, and every carrier of a whole session policy carries it too.
    ///
    /// The durable session-head copy restores this intent on a cold load. Per
    /// ADR 0030, a live facade host may still reconcile its current spec over
    /// loaded state at open time.
    /// The process/remote policy carrier mirrors the same field.
    pub generation: crate::GenerationOptions,
}

impl SessionPolicy {
    /// Construct a policy with an explicit turn budget and tool-call limit,
    /// no model selected and otherwise neutral settings.
    pub fn new(turn_budget: TurnBudget, max_tool_calls: MaxToolCalls) -> Self {
        Self {
            model: None,
            attachment_acceptance: Arc::default(),
            session_id: None,
            autonomous: false,
            turn_budget,
            max_tool_calls,
            no_progress_budget: NoProgressBudget::default(),
            charge_safety: ChargeSafetyPolicy::default(),
            generation: crate::GenerationOptions::default(),
        }
    }

    /// The recorded model key, when the session has selected a model.
    pub fn profile_key(&self) -> Option<&crate::LlmProfileKey> {
        self.model.as_ref().map(LlmProfileConfig::key)
    }

    /// The recorded wire model, when the session has selected a model.
    pub fn wire_model(&self) -> Option<&str> {
        self.model.as_ref().map(|model| model.model.wire_model())
    }

    /// The recorded prompt budget, when the session has selected a model.
    pub fn context_window_tokens(&self) -> Option<usize> {
        self.model
            .as_ref()
            .map(LlmProfileConfig::context_window_tokens)
    }
}

/// How a [`SessionSpec`] layers generation intent over the policy it resolves
/// against.
///
/// [`crate::GenerationOptions`] is a set of independently optional controls, not
/// one value, so the default overlay is per-field: a child that caps output
/// tokens keeps the temperature and seed its parent pinned. Discarding
/// inherited intent stays available, but it has to be asked for.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "mode", content = "generation", rename_all = "snake_case")]
pub enum GenerationOverlay {
    /// Layer the set options over the inherited ones. Options this overlay
    /// leaves unset keep the value they inherit.
    Merge(crate::GenerationOptions),
    /// Use exactly these options, discarding every inherited one. A default
    /// [`crate::GenerationOptions`] therefore clears the inherited intent.
    Replace(crate::GenerationOptions),
}
impl GenerationOverlay {
    pub fn resolve(&self, inherited: &crate::GenerationOptions) -> crate::GenerationOptions {
        match self {
            Self::Merge(generation) => generation.merged_over(inherited),
            Self::Replace(generation) => generation.clone(),
        }
    }
}
