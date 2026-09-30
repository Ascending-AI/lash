//! Durable and host-reconciled session policy.

use crate::{ChargeSafetyPolicy, ModelSpec, NoProgressBudget, SessionId, TurnBudget};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPolicy {
    pub model: ModelSpec,
    pub provider_id: String,
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
    pub prompt: crate::PromptLayer,
    /// Caller-owned generation intent applied to every LLM call this session
    /// makes. It lives on the policy rather than on a single turn because it
    /// is session-wide truth: child sessions resolve their spec against this
    /// policy, and every carrier of a whole session policy carries it too.
    ///
    /// The durable session-head copy restores this intent on a cold load. Per
    /// ADR 0030, a live facade host may still reconcile its current spec over
    /// loaded state at open time, exactly as it does for the model and prompt.
    /// The process/remote policy carrier mirrors the same field.
    pub generation: crate::GenerationOptions,
}
impl SessionPolicy {
    pub fn replace_model_retaining_attachment_acceptance(&mut self, mut model: ModelSpec) {
        model.capability.attachment_acceptance =
            self.model.capability.attachment_acceptance.clone();
        self.model = model;
    }
}
impl SessionPolicy {
    /// Construct a policy with an explicit turn budget and otherwise neutral
    /// settings.
    pub fn new(turn_budget: TurnBudget) -> Self {
        Self {
            model: ModelSpec::default(),
            provider_id: String::new(),
            session_id: None,
            autonomous: false,
            turn_budget,
            no_progress_budget: NoProgressBudget::default(),
            charge_safety: ChargeSafetyPolicy::default(),
            prompt: crate::PromptLayer::new(),
            generation: crate::GenerationOptions::default(),
        }
    }

    /// Exposes the provider ID captured in policy for protocol implementors restoring the same
    /// provider/model assignment on replay.
    pub fn recorded_provider_id(&self) -> &str {
        self.provider_id.trim()
    }

    /// Settle the durable provider pin against the id a host names at this open.
    ///
    /// The recorded id is a durable fact: it is read and guarded, never
    /// smoothed over (ADR 0066). A session with no recorded pin adopts the
    /// host's id; an open that names nothing inherits the recorded pin; an
    /// open naming the recorded provider keeps it; an open naming a
    /// *different* provider is refused with
    /// [`ProviderPinMismatch`] (widened to `SessionError::ProviderMismatch` by
    /// `lash-core`) rather than having its request silently discarded and the
    /// conflict deferred to the first turn.
    pub fn settle_provider_pin(
        session_id: &SessionId,
        recorded: &str,
        requested: &str,
    ) -> Result<String, ProviderPinMismatch> {
        let recorded = recorded.trim();
        let requested = requested.trim();
        if recorded.is_empty() {
            return Ok(requested.to_string());
        }
        if requested.is_empty() || requested == recorded {
            return Ok(recorded.to_string());
        }
        Err(ProviderPinMismatch {
            expected: recorded.to_string(),
            actual: requested.to_string(),
            session_id: session_id.clone(),
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model.id
    }

    pub fn model_variant(&self) -> &crate::ReasoningSelection {
        &self.model.variant
    }

    pub fn context_window_tokens(&self) -> usize {
        self.model.context_window_tokens()
    }
}

/// A recorded provider pin that does not match the live request.
///
/// `lash-core` widens this into `SessionError::ProviderMismatch`; the pin rule
/// itself is durable policy, so it settles here.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "provider mismatch for session `{session_id}`: persisted provider `{expected}` does not match live provider `{actual}`"
)]
pub struct ProviderPinMismatch {
    pub expected: String,
    pub actual: String,
    pub session_id: crate::SessionId,
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
