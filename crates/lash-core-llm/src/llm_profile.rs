use std::num::NonZeroUsize;

use crate::provider::{
    CacheRetention, LlmProfileCapability, LlmProfileEffortValidationCategory,
    LlmProfileRequestDefaults, ReasoningSelection,
};

/// The host's opaque name for one registered model.
///
/// Lash never parses it: `glm-5.3-flash@tensorx` names a registration, not a
/// provider and a model. A session records the key with the metadata the
/// host's registry minted for it ([`RecordedLlmProfile`]), so the key alone never
/// decides how a recorded root runs.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct LlmProfileKey(String);

impl LlmProfileKey {
    /// A key as the host spells it. A registry refuses an empty key when the
    /// model is registered, so a lookup of one simply finds nothing.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LlmProfileKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for LlmProfileKey {
    fn from(key: &str) -> Self {
        Self::new(key)
    }
}

impl From<String> for LlmProfileKey {
    fn from(key: String) -> Self {
        Self::new(key)
    }
}

/// Host-supplied facts about one registered model (ADR 0026): the exact wire
/// model a request names, its limits, its execution capabilities and the
/// request extensions it records.
///
/// It carries no reasoning selection and no attachment-acceptance rules: both
/// are session config ([`LlmProfileConfig::reasoning`] and the session policy's
/// attachment acceptance), chosen separately from the model.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct LlmProfileMetadata {
    /// The model id the provider's wire names.
    pub wire_model: String,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    pub limits: LlmProfileLimits,
    /// Host-supplied capability metadata: reasoning controls and cache-control
    /// dialect accepted by this model. Lash validates the session's reasoning
    /// selection against it and threads it onto every provider request.
    #[serde(default, skip_serializing_if = "LlmProfileCapability::is_empty")]
    pub capability: LlmProfileCapability,
    /// What this model's requests do where a request states nothing:
    /// recorded with the binding, so a transport change never alters it.
    #[serde(default, skip_serializing_if = "LlmProfileRequestDefaults::is_default")]
    pub request_defaults: LlmProfileRequestDefaults,
}

impl LlmProfileMetadata {
    pub fn builder(wire_model: impl Into<String>) -> LlmProfileMetadataBuilder {
        LlmProfileMetadataBuilder::new(wire_model)
    }

    pub fn new(wire_model: impl Into<String>, context_window_tokens: NonZeroUsize) -> Self {
        Self::with_limits(
            wire_model,
            LlmProfileLimits {
                context_window_tokens,
                output_token_capacity: None,
            },
        )
    }

    pub fn with_limits(wire_model: impl Into<String>, limits: LlmProfileLimits) -> Self {
        Self {
            wire_model: wire_model.into(),
            extra_body: serde_json::Map::new(),
            limits,
            capability: LlmProfileCapability::default(),
            request_defaults: LlmProfileRequestDefaults::default(),
        }
    }

    pub fn with_capability(mut self, capability: LlmProfileCapability) -> Self {
        self.capability = capability;
        self
    }

    pub fn with_extra_body(
        mut self,
        extra_body: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        self.extra_body = extra_body;
        self
    }

    pub fn with_request_defaults(mut self, request_defaults: LlmProfileRequestDefaults) -> Self {
        self.request_defaults = request_defaults;
        self
    }

    /// Exposes the non-zero prompt budget protocol implementors use for history pruning rather than
    /// the optional output-token ceiling.
    pub fn context_window_tokens(&self) -> usize {
        self.limits.context_window_tokens.get()
    }
}

/// Builder for host-supplied [`LlmProfileMetadata`].
///
/// The context-window token budget is required; output capacity and
/// capability metadata are absent when omitted. Setters follow the builder
/// convention and therefore have no `with_` prefix.
#[derive(Clone, Debug)]
pub struct LlmProfileMetadataBuilder {
    wire_model: String,
    context_window_tokens: Option<usize>,
    output_token_capacity: Option<usize>,
    capability: LlmProfileCapability,
    extra_body: serde_json::Map<String, serde_json::Value>,
    request_defaults: LlmProfileRequestDefaults,
}

impl LlmProfileMetadataBuilder {
    fn new(wire_model: impl Into<String>) -> Self {
        Self {
            wire_model: wire_model.into(),
            context_window_tokens: None,
            output_token_capacity: None,
            capability: LlmProfileCapability::default(),
            extra_body: serde_json::Map::new(),
            request_defaults: LlmProfileRequestDefaults::default(),
        }
    }

    pub fn context_window_tokens(mut self, context_window_tokens: usize) -> Self {
        self.context_window_tokens = Some(context_window_tokens);
        self
    }

    pub fn output_token_capacity(mut self, output_token_capacity: usize) -> Self {
        self.output_token_capacity = Some(output_token_capacity);
        self
    }

    /// Attach host-supplied reasoning and cache-control capability metadata.
    pub fn capability(mut self, capability: LlmProfileCapability) -> Self {
        self.capability = capability;
        self
    }

    pub fn extra_body(mut self, extra_body: serde_json::Map<String, serde_json::Value>) -> Self {
        self.extra_body = extra_body;
        self
    }

    /// Surface the reasoning the provider streams in this model's responses.
    pub fn expose_thinking(mut self, expose_thinking: bool) -> Self {
        self.request_defaults.expose_thinking = expose_thinking;
        self
    }

    /// The output-token cap of this model's calls whose request sets none.
    pub fn max_output_tokens(mut self, max_output_tokens: u64) -> Self {
        self.request_defaults.max_output_tokens = Some(max_output_tokens);
        self
    }

    /// The prompt-cache lifetime hint of this model's requests.
    pub fn cache_retention(mut self, cache_retention: CacheRetention) -> Self {
        self.request_defaults.cache_retention = cache_retention;
        self
    }

    /// The response header names (case-insensitive) this model's calls
    /// capture into `LlmResponse.response_metadata`.
    pub fn response_metadata_headers(mut self, headers: Vec<String>) -> Self {
        self.request_defaults.response_metadata_headers = headers;
        self
    }

    /// The JSON pointers this model's calls capture from response bodies and
    /// SSE events into `LlmResponse.response_metadata`.
    pub fn response_metadata_body_paths(mut self, body_paths: Vec<String>) -> Self {
        self.request_defaults.response_metadata_body_paths = body_paths;
        self
    }

    pub fn build(self) -> Result<LlmProfileMetadata, LlmProfileLimitsError> {
        let context_window_tokens = self
            .context_window_tokens
            .ok_or(LlmProfileLimitsError::MissingContextWindowTokens)?;
        Ok(LlmProfileMetadata::with_limits(
            self.wire_model,
            LlmProfileLimits::validated(context_window_tokens, self.output_token_capacity)?,
        )
        .with_capability(self.capability)
        .with_extra_body(self.extra_body)
        .with_request_defaults(self.request_defaults))
    }
}

/// A model binding a host's [`LlmProfiles`](crate::provider::LlmProfiles)
/// minted for one key: the key and the metadata it served at that moment.
///
/// A session records it at creation and at every model change, and a root
/// copies it into its recorded run. Every consumer — admission, pruning,
/// request construction, replay — reads these recorded facts; a later
/// catalog edit reaches the session only through another model patch.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RecordedLlmProfile {
    key: LlmProfileKey,
    metadata: LlmProfileMetadata,
}

impl RecordedLlmProfile {
    /// Mint the binding a registry serves `key` with. Only a
    /// [`LlmProfiles`](crate::provider::LlmProfiles) implementation
    /// calls this: everything else copies a recorded value.
    pub fn mint(key: LlmProfileKey, metadata: LlmProfileMetadata) -> Self {
        Self { key, metadata }
    }

    pub fn key(&self) -> &LlmProfileKey {
        &self.key
    }

    pub fn metadata(&self) -> &LlmProfileMetadata {
        &self.metadata
    }

    /// The recorded wire model a request names.
    pub fn wire_model(&self) -> &str {
        &self.metadata.wire_model
    }

    pub fn context_window_tokens(&self) -> usize {
        self.metadata.context_window_tokens()
    }
}

/// A session's model selection: the recorded binding and the reasoning it
/// runs that model with. Reasoning is chosen independently of the model and
/// validated against the recorded capability; the metadata never carries a
/// second default for it.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct LlmProfileConfig {
    pub model: RecordedLlmProfile,
    #[serde(default)]
    pub reasoning: ReasoningSelection,
}

impl LlmProfileConfig {
    /// `model` with the provider's default reasoning.
    pub fn new(model: RecordedLlmProfile) -> Self {
        Self {
            model,
            reasoning: ReasoningSelection::ProviderDefault,
        }
    }

    pub fn with_reasoning(mut self, reasoning: ReasoningSelection) -> Self {
        self.reasoning = reasoning;
        self
    }

    pub fn key(&self) -> &LlmProfileKey {
        self.model.key()
    }

    pub fn metadata(&self) -> &LlmProfileMetadata {
        self.model.metadata()
    }

    pub fn context_window_tokens(&self) -> usize {
        self.model.context_window_tokens()
    }

    /// Judge the reasoning selection against the recorded capability of the
    /// model it would run with. Every point that records a model or a
    /// reasoning selection judges the pair it would record here — creation,
    /// a config transaction, a per-run override and a child's model key — so
    /// an unsupported selection is refused where it is stated and never
    /// becomes a later turn failure (FIG-4531).
    ///
    /// # Errors
    ///
    /// [`ReasoningRefused`] when the capability does not accept the
    /// selection.
    pub fn validate_reasoning(&self) -> Result<(), ReasoningRefused> {
        self.metadata()
            .capability
            .reasoning_intent(
                self.model.wire_model(),
                &format!("key `{}`", self.key()),
                &self.reasoning,
            )
            .map(drop)
            .map_err(|error| ReasoningRefused {
                key: self.key().clone(),
                reasoning: self.reasoning.clone(),
                category: error.category,
                message: error.message,
            })
    }
}

/// A reasoning selection the recorded capability of the model under `key`
/// refuses.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[error("reasoning selection refused for model key `{key}`: {message}")]
pub struct ReasoningRefused {
    pub key: LlmProfileKey,
    pub reasoning: ReasoningSelection,
    pub category: LlmProfileEffortValidationCategory,
    pub message: String,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct LlmProfileLimits {
    /// The prompt budget: the maximum input tokens the provider accepts for
    /// this model on this route. History pruning measures against this — not
    /// the model's total context (input + output), which would over-budget by
    /// the whole response reservation.
    pub context_window_tokens: NonZeroUsize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_token_capacity: Option<NonZeroUsize>,
}

/// Invalid or incomplete token-limit metadata supplied for a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LlmProfileLimitsError {
    #[error("a context-window token budget is required")]
    MissingContextWindowTokens,
    #[error("context_window_tokens must be greater than zero")]
    ZeroContextWindowTokens,
    #[error("output_token_capacity must be greater than zero")]
    ZeroOutputTokenCapacity,
}

impl LlmProfileLimits {
    fn validated(
        context_window_tokens: usize,
        output_token_capacity: Option<usize>,
    ) -> Result<Self, LlmProfileLimitsError> {
        Ok(Self {
            context_window_tokens: NonZeroUsize::new(context_window_tokens)
                .ok_or(LlmProfileLimitsError::ZeroContextWindowTokens)?,
            output_token_capacity: output_token_capacity
                .map(|value| {
                    NonZeroUsize::new(value).ok_or(LlmProfileLimitsError::ZeroOutputTokenCapacity)
                })
                .transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> LlmProfileMetadata {
        LlmProfileMetadata::builder("provider/model")
            .context_window_tokens(8_192)
            .output_token_capacity(1_024)
            .build()
            .expect("valid metadata")
    }

    #[test]
    fn llm_profile_config_reasoning_selection_serde_is_explicit() {
        for (selection, expected) in [
            (
                ReasoningSelection::ProviderDefault,
                serde_json::json!("provider_default"),
            ),
            (ReasoningSelection::Disabled, serde_json::json!("disabled")),
            (
                ReasoningSelection::Effort("high".to_string()),
                serde_json::json!({ "effort": "high" }),
            ),
        ] {
            let config = LlmProfileConfig::new(RecordedLlmProfile::mint(
                LlmProfileKey::new("key"),
                metadata(),
            ))
            .with_reasoning(selection.clone());
            let json = serde_json::to_value(&config).expect("serialize model config");
            assert_eq!(json["reasoning"], expected);
            assert_eq!(json["model"]["key"], "key");
            let round_trip: LlmProfileConfig =
                serde_json::from_value(json).expect("deserialize model config");
            assert_eq!(round_trip.reasoning, selection);
            assert_eq!(round_trip, config);
        }
    }

    /// Metadata has no reasoning selection of its own: a recorded binding
    /// that carried one would be a second default beside the session's.
    #[test]
    fn model_metadata_refuses_a_reasoning_variant() {
        let mut json = serde_json::to_value(metadata()).expect("serialize metadata");
        json["variant"] = serde_json::json!("disabled");
        serde_json::from_value::<LlmProfileMetadata>(json)
            .expect_err("a variant is not model metadata");
    }

    #[test]
    fn model_metadata_builder_covers_limits_capability_and_requires_context_window() {
        let spec = LlmProfileMetadata::builder("provider/model")
            .context_window_tokens(200_000)
            .output_token_capacity(8_192)
            .capability(LlmProfileCapability {
                reasoning: Some(crate::provider::ReasoningCapability {
                    efforts: vec!["high".to_string()],
                    ..Default::default()
                }),
                ..Default::default()
            })
            .build()
            .expect("valid model metadata");

        assert_eq!(spec.wire_model, "provider/model");
        assert_eq!(spec.context_window_tokens(), 200_000);
        assert_eq!(
            spec.limits.output_token_capacity.map(NonZeroUsize::get),
            Some(8_192)
        );
        assert!(!spec.capability.is_empty());

        assert_eq!(
            LlmProfileMetadata::builder("missing-context")
                .build()
                .expect_err("context budget is required"),
            LlmProfileLimitsError::MissingContextWindowTokens
        );
        let context_error = LlmProfileMetadata::builder("bad-context")
            .context_window_tokens(0)
            .output_token_capacity(1)
            .build()
            .expect_err("zero context");
        assert_eq!(
            context_error,
            LlmProfileLimitsError::ZeroContextWindowTokens
        );
        let output_error = LlmProfileMetadata::builder("bad-output")
            .context_window_tokens(1)
            .output_token_capacity(0)
            .build()
            .expect_err("zero output cap");
        assert_eq!(output_error, LlmProfileLimitsError::ZeroOutputTokenCapacity);
    }
}
