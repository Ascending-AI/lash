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
    /// Host-owned live policy in the sense of ADR 0030: it is reconciled from
    /// the host's configuration on every reopen and is deliberately not
    /// persisted with the session head, so a host that changes the bound
    /// changes it for sessions already on disk. Its default is bounded, so a
    /// carrier that predates the field resolves to the bound rather than to a
    /// loop.
    pub no_progress_budget: NoProgressBudget,
    /// Live host risk appetite for duplicate provider billing.
    ///
    /// Like `no_progress_budget`, this is reconciled from host configuration
    /// on every reopen and is deliberately absent from the persisted session
    /// head. Old carriers therefore resolve to the charge-safe default.
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

/// Durable session-policy mutation carried by
/// [`crate::SessionCommand::ApplyConfigPatch`].
///
/// Every field is applied at the session-command drain. The command commit is
/// therefore the publication boundary: resident policy is never changed by a
/// setter before the durable head accepts the same values.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ApplyConfigPatch {
    /// Exact session-config wire generation. The patch and the head row share
    /// one schema because they carry the same durable policy facts.
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<crate::ModelSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<crate::PromptLayer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<crate::GenerationOverlay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_budget: Option<crate::TurnBudget>,
    /// Session-owned tool authority. This durable fact lives beside the
    /// protocol turn options on runtime state and replaces the whole access
    /// value when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_access: Option<crate::SessionToolAccess>,
    /// Protocol-owned turn options. Unlike the other fields this durable fact
    /// lives on the runtime session state rather than inside
    /// [`crate::SessionPolicy`], but it settles through the same commanded
    /// path: the drain commit publishes it to the session head (v6) and to
    /// resident state in one step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_turn_options: Option<crate::ProtocolTurnOptions>,
}
impl Default for ApplyConfigPatch {
    fn default() -> Self {
        Self {
            schema_version: crate::store::SESSION_HEAD_META_SCHEMA_VERSION,
            provider_id: None,
            model: None,
            prompt: None,
            generation: None,
            turn_budget: None,
            tool_access: None,
            protocol_turn_options: None,
        }
    }
}
impl ApplyConfigPatch {
    pub fn between(previous: &crate::SessionPolicy, next: &crate::SessionPolicy) -> Self {
        Self {
            provider_id: (previous.provider_id != next.provider_id)
                .then(|| next.provider_id.clone()),
            model: (previous.model != next.model).then(|| next.model.clone()),
            prompt: (previous.prompt != next.prompt).then(|| next.prompt.clone()),
            generation: (previous.generation != next.generation)
                .then(|| crate::GenerationOverlay::Replace(next.generation.clone())),
            turn_budget: (previous.turn_budget != next.turn_budget).then_some(next.turn_budget),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), crate::RuntimeError> {
        if self.schema_version != crate::store::SESSION_HEAD_META_SCHEMA_VERSION {
            return Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::SessionCommandClaim,
                format!(
                    "unsupported config patch schema version {}; expected {}",
                    self.schema_version,
                    crate::store::SESSION_HEAD_META_SCHEMA_VERSION
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn apply_to(&self, policy: &mut crate::SessionPolicy) {
        if let Some(provider_id) = self.provider_id.as_ref() {
            policy.provider_id = provider_id.clone();
        }
        if let Some(model) = self.model.as_ref() {
            policy.replace_model_retaining_attachment_acceptance(model.clone());
        }
        if let Some(prompt) = self.prompt.as_ref() {
            policy.prompt = prompt.clone();
        }
        if let Some(generation) = self.generation.as_ref() {
            policy.generation = generation.resolve(&policy.generation);
        }
        if let Some(turn_budget) = self.turn_budget {
            policy.turn_budget = turn_budget;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.provider_id.is_none()
            && self.model.is_none()
            && self.prompt.is_none()
            && self.generation.is_none()
            && self.turn_budget.is_none()
            && self.tool_access.is_none()
            && self.protocol_turn_options.is_none()
    }

    /// Publish every settled field to resident session state.
    ///
    /// Policy-homed fields land through [`Self::apply_to`]; the protocol turn
    /// options land on their runtime-state home. Both publications happen only
    /// after the durable head accepted the same values.
    pub fn apply_to_state(&self, state: &mut crate::RuntimeSessionState) {
        self.apply_to(&mut state.policy);
        if let Some(access) = self.tool_access.as_ref() {
            state.authority.tool_access = access.clone();
        }
        if let Some(options) = self.protocol_turn_options.as_ref() {
            state.protocol_turn_options = options.clone();
        }
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
