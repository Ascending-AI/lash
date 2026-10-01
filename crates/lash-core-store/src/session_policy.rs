//! Durable and host-reconciled session policy.

use std::sync::Arc;

use crate::provider::AttachmentCapabilitySnapshot;
use crate::{ChargeSafetyPolicy, ModelConfig, NoProgressBudget, SessionId, TurnBudget};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPolicy {
    /// The session's model selection: the binding the host's registry minted
    /// when the session adopted its key, and the reasoning it runs with.
    /// `None` until a model is selected; such a session cannot run a turn.
    pub model: Option<ModelConfig>,
    /// The attachment-acceptance rules the session renders attachments
    /// against (ADR 0026). Session config of its own: a model change keeps
    /// them, and only an explicit change replaces them.
    pub attachment_acceptance: Arc<AttachmentCapabilitySnapshot>,
    pub session_id: Option<SessionId>,
    pub autonomous: bool,
    /// Required turn-budget decision. A host must choose either a non-zero
    /// bound or explicit unbounded execution; absence is never interpreted.
    pub turn_budget: TurnBudget,
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
    /// The creating core's prompt layer, recorded at creation (FIG-4397) and
    /// rendered beneath [`Self::prompt`]. A core's prompt is a creation
    /// default: a session renders the layer it recorded on every worker, and
    /// a child inherits its parent's.
    pub core_prompt: crate::PromptLayer,
    /// The session's own prompt layer, rendered over [`Self::core_prompt`].
    pub prompt: crate::PromptLayer,
    /// Caller-owned generation intent applied to every LLM call this session
    /// makes. It lives on the policy rather than on a single turn because it
    /// is session-wide truth: child sessions resolve their spec against this
    /// policy, and every carrier of a whole session policy carries it too.
    ///
    /// The durable session-head copy restores this intent on a cold load. Per
    /// ADR 0030, a live facade host may still reconcile its current spec over
    /// loaded state at open time, exactly as it does for the prompt.
    /// The process/remote policy carrier mirrors the same field.
    pub generation: crate::GenerationOptions,
}

impl SessionPolicy {
    /// Construct a policy with an explicit turn budget, no model selected
    /// and otherwise neutral settings.
    pub fn new(turn_budget: TurnBudget) -> Self {
        Self {
            model: None,
            attachment_acceptance: Arc::default(),
            session_id: None,
            autonomous: false,
            turn_budget,
            no_progress_budget: NoProgressBudget::default(),
            charge_safety: ChargeSafetyPolicy::default(),
            core_prompt: crate::PromptLayer::new(),
            prompt: crate::PromptLayer::new(),
            generation: crate::GenerationOptions::default(),
        }
    }

    /// The recorded model key, when the session has selected a model.
    pub fn model_key(&self) -> Option<&crate::ModelKey> {
        self.model.as_ref().map(ModelConfig::key)
    }

    /// The recorded wire model, when the session has selected a model.
    pub fn wire_model(&self) -> Option<&str> {
        self.model.as_ref().map(|model| model.model.wire_model())
    }

    /// The recorded prompt budget, when the session has selected a model.
    pub fn context_window_tokens(&self) -> Option<usize> {
        self.model.as_ref().map(ModelConfig::context_window_tokens)
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
