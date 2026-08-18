//! The hand-written durable encoding of [`SessionPolicy`].
//!
//! This pair lives alone in its own file on purpose. The policy travels inside
//! persisted graph-node bodies, so the version-bump gate has to notice when its
//! encoding moves; because no symbol projection can see an `impl` block, the
//! only guard available is a whole-file one, and a whole-file guard over the
//! module that also holds the runtime session types would demand a schema bump
//! for every unrelated edit there. Keeping the encoding here makes the guard
//! precise: this file changes exactly when the persisted policy shape does.
//!
//! Nothing else belongs in this file. The encoder is also the complete durable
//! projection of the policy -- a field added to the struct is absent from the
//! wire until it is named here -- so a field this file does not mention is a
//! runtime-only field by construction.

use super::{ModelSpec, NoProgressBudget, SessionPolicy, TurnBudget};

impl serde::Serialize for SessionPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut fields = 5;
        if self.no_progress_budget != NoProgressBudget::default() {
            fields += 1;
        }
        if !self.prompt.is_empty() {
            fields += 1;
        }
        if self.generation != crate::GenerationOptions::default() {
            fields += 1;
        }
        let mut state = serializer.serialize_struct("SessionPolicy", fields)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("provider_id", self.recorded_provider_id())?;
        state.serialize_field("session_id", &self.session_id)?;
        state.serialize_field("autonomous", &self.autonomous)?;
        state.serialize_field("turn_budget", &self.turn_budget)?;
        if self.no_progress_budget != NoProgressBudget::default() {
            state.serialize_field("no_progress_budget", &self.no_progress_budget)?;
        }
        if !self.prompt.is_empty() {
            state.serialize_field("prompt", &self.prompt)?;
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
            model: ModelSpec,
            #[serde(default)]
            provider_id: String,
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default)]
            autonomous: bool,
            turn_budget: TurnBudget,
            #[serde(default)]
            no_progress_budget: NoProgressBudget,
            #[serde(default)]
            prompt: crate::PromptLayer,
            #[serde(default)]
            generation: crate::GenerationOptions,
        }

        let value = serde_json::Value::deserialize(deserializer)?;
        if value
            .as_object()
            .is_some_and(|object| object.contains_key("provider"))
        {
            return Err(serde::de::Error::custom(
                "legacy serialized provider config is not supported in session state; persist provider_id only",
            ));
        }
        let wire = Wire::deserialize(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            model: wire.model,
            provider_id: wire.provider_id,
            session_id: wire.session_id,
            autonomous: wire.autonomous,
            turn_budget: wire.turn_budget,
            no_progress_budget: wire.no_progress_budget,
            prompt: wire.prompt,
            generation: wire.generation,
        })
    }
}
