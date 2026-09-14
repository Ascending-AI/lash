//! Durable and host-reconciled session policy.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

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
    pub(crate) fn replace_model_retaining_attachment_acceptance(&mut self, mut model: ModelSpec) {
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
    /// [`SessionError::ProviderMismatch`](crate::SessionError::ProviderMismatch)
    /// rather than having its request silently discarded and the conflict
    /// deferred to the first turn.
    pub fn settle_provider_pin(
        session_id: &SessionId,
        recorded: &str,
        requested: &str,
    ) -> Result<String, crate::SessionError> {
        let recorded = recorded.trim();
        let requested = requested.trim();
        if recorded.is_empty() {
            return Ok(requested.to_string());
        }
        if requested.is_empty() || requested == recorded {
            return Ok(recorded.to_string());
        }
        Err(crate::SessionError::ProviderMismatch {
            expected: recorded.to_string(),
            actual: requested.to_string(),
            session_id: session_id.clone(),
        })
    }

    /// Exposes model id to protocol and process-engine implementors while materializing
    /// protocol-specific session and turn state.
    pub fn model_id(&self) -> &str {
        &self.model.id
    }

    /// Exposes model variant to protocol and process-engine implementors while materializing
    /// protocol-specific session and turn state.
    pub fn model_variant(&self) -> &crate::ReasoningSelection {
        &self.model.variant
    }

    /// Exposes context window tokens to store, effect-host, and protocol implementors while
    /// materializing, executing, or persisting a session turn.
    pub fn context_window_tokens(&self) -> usize {
        self.model.context_window_tokens()
    }
}
