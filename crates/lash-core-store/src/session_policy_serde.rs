//! The hand-written durable encoding of [`SessionPolicy`].
//!
//! This pair lives alone in its own file on purpose. The policy travels inside
//! persisted graph-node bodies; keeping the encoding in its own file means this
//! file changes exactly when the persisted policy shape does.
//!
//! Nothing else belongs in this file. The encoder is also the complete durable
//! projection of the policy -- a field added to the struct is absent from the
//! wire until it is named here -- so a field this file does not mention is a
//! runtime-only field by construction.

use crate::SessionId;
use crate::{MaxToolCalls, ModelConfig, NoProgressBudget, SessionPolicy, TurnBudget};

impl serde::Serialize for SessionPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut fields = 4;
        if self.model.is_some() {
            fields += 1;
        }
        if !self.attachment_acceptance.is_empty() {
            fields += 1;
        }
        if self.no_progress_budget != NoProgressBudget::default() {
            fields += 1;
        }
        if self.charge_safety != crate::ChargeSafetyPolicy::default() {
            fields += 1;
        }
        if self.generation != crate::GenerationOptions::default() {
            fields += 1;
        }
        let mut state = serializer.serialize_struct("SessionPolicy", fields)?;
        if let Some(model) = &self.model {
            state.serialize_field("model", model)?;
        }
        if !self.attachment_acceptance.is_empty() {
            state.serialize_field("attachment_acceptance", &self.attachment_acceptance)?;
        }
        state.serialize_field("session_id", &self.session_id)?;
        state.serialize_field("autonomous", &self.autonomous)?;
        state.serialize_field("turn_budget", &self.turn_budget)?;
        state.serialize_field("max_tool_calls", &self.max_tool_calls)?;
        if self.no_progress_budget != NoProgressBudget::default() {
            state.serialize_field("no_progress_budget", &self.no_progress_budget)?;
        }
        if self.charge_safety != crate::ChargeSafetyPolicy::default() {
            state.serialize_field("charge_safety", &self.charge_safety)?;
        }
        if self.generation != crate::GenerationOptions::default() {
            state.serialize_field("generation", &self.generation)?;
        }
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for SessionPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            model: Option<ModelConfig>,
            #[serde(default)]
            attachment_acceptance: std::sync::Arc<crate::provider::AttachmentCapabilitySnapshot>,
            #[serde(default)]
            session_id: Option<SessionId>,
            #[serde(default)]
            autonomous: bool,
            turn_budget: TurnBudget,
            max_tool_calls: MaxToolCalls,
            #[serde(default)]
            no_progress_budget: NoProgressBudget,
            #[serde(default)]
            charge_safety: crate::ChargeSafetyPolicy,
            #[serde(default)]
            generation: crate::GenerationOptions,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            model: wire.model,
            attachment_acceptance: wire.attachment_acceptance,
            session_id: wire.session_id,
            autonomous: wire.autonomous,
            turn_budget: wire.turn_budget,
            max_tool_calls: wire.max_tool_calls,
            no_progress_budget: wire.no_progress_budget,
            charge_safety: wire.charge_safety,
            generation: wire.generation,
        })
    }
}
