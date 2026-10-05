use std::num::NonZeroUsize;

use crate::llm::capability::{
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
                output_tokens: OutputTokenLimits::default(),
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
    default_output_token_cap: Option<u64>,
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
            default_output_token_cap: None,
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

    pub fn request_defaults(mut self, request_defaults: LlmProfileRequestDefaults) -> Self {
        self.request_defaults = request_defaults;
        self
    }

    /// Surface the reasoning the provider streams in this model's responses.
    pub fn expose_thinking(mut self, expose_thinking: bool) -> Self {
        self.request_defaults.expose_thinking = expose_thinking;
        self
    }

    /// The output-token cap of this model's calls whose request sets none.
    pub fn max_output_tokens(mut self, max_output_tokens: u64) -> Self {
        self.default_output_token_cap = Some(max_output_tokens);
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
            LlmProfileLimits {
                context_window_tokens: NonZeroUsize::new(context_window_tokens)
                    .ok_or(LlmProfileLimitsError::ZeroContextWindowTokens)?,
                output_tokens: OutputTokenLimits::new(
                    self.output_token_capacity,
                    self.default_output_token_cap,
                )?,
            },
        )
        .with_capability(self.capability)
        .with_extra_body(self.extra_body)
        .with_request_defaults(self.request_defaults))
    }
}

/// A model binding a host's `LlmProfiles`
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
    /// `LlmProfiles` implementation
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

    pub fn wire_model(&self) -> &str {
        self.model.wire_model()
    }

    /// Edit this owned binding before dispatch; recorded owners are copied intact.
    pub fn metadata_mut(&mut self) -> &mut LlmProfileMetadata {
        &mut self.model.metadata
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
    pub output_tokens: OutputTokenLimits,
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
    #[error("default output-token cap must be greater than zero")]
    ZeroOutputTokenDefault,
    #[error("default output-token cap does not fit the platform's token count")]
    OutputTokenDefaultOutOfRange,
    #[error("default output-token cap {default_cap} exceeds capacity {capacity}")]
    OutputTokenDefaultExceedsCapacity {
        default_cap: NonZeroUsize,
        capacity: NonZeroUsize,
    },
}

/// Output capacity and the default cap recorded together for a profile.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(try_from = "OutputTokenLimitsWire", into = "OutputTokenLimitsWire")]
pub struct OutputTokenLimits {
    capacity: Option<NonZeroUsize>,
    default_cap: Option<NonZeroUsize>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct OutputTokenLimitsWire {
    pub capacity: Option<NonZeroUsize>,
    pub default_cap: Option<NonZeroUsize>,
}

impl OutputTokenLimits {
    /// A nonzero default with no declared capacity, valid by construction.
    pub fn from_default_cap(default_cap: NonZeroUsize) -> Self {
        Self {
            capacity: None,
            default_cap: Some(default_cap),
        }
    }

    pub fn new(
        capacity: Option<usize>,
        default_cap: Option<u64>,
    ) -> Result<Self, LlmProfileLimitsError> {
        let capacity = capacity
            .map(|value| {
                NonZeroUsize::new(value).ok_or(LlmProfileLimitsError::ZeroOutputTokenCapacity)
            })
            .transpose()?;
        let default_cap = default_cap
            .map(|value| {
                let value = usize::try_from(value)
                    .map_err(|_| LlmProfileLimitsError::OutputTokenDefaultOutOfRange)?;
                NonZeroUsize::new(value).ok_or(LlmProfileLimitsError::ZeroOutputTokenDefault)
            })
            .transpose()?;
        if let (Some(capacity), Some(default_cap)) = (capacity, default_cap)
            && default_cap > capacity
        {
            return Err(LlmProfileLimitsError::OutputTokenDefaultExceedsCapacity {
                default_cap,
                capacity,
            });
        }
        Ok(Self {
            capacity,
            default_cap,
        })
    }

    pub fn capacity(&self) -> Option<NonZeroUsize> {
        self.capacity
    }
    pub fn default_cap(&self) -> Option<NonZeroUsize> {
        self.default_cap
    }
}

impl TryFrom<OutputTokenLimitsWire> for OutputTokenLimits {
    type Error = LlmProfileLimitsError;
    fn try_from(value: OutputTokenLimitsWire) -> Result<Self, Self::Error> {
        Self::new(
            value.capacity.map(NonZeroUsize::get),
            value.default_cap.map(|cap| cap.get() as u64),
        )
    }
}

impl From<OutputTokenLimits> for OutputTokenLimitsWire {
    fn from(value: OutputTokenLimits) -> Self {
        Self {
            capacity: value.capacity,
            default_cap: value.default_cap,
        }
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

    /// Recorded owners expose the recorded prompt budget, independently of
    /// output capacity (ADR 0026), through both binding and session config.
    #[test]
    fn recorded_profile_and_config_preserve_the_prompt_budget() {
        for prompt_budget in [1, 8_192, 200_000] {
            let metadata = LlmProfileMetadata::builder("provider/model")
                .context_window_tokens(prompt_budget)
                .output_token_capacity(1_024)
                .build()
                .expect("valid metadata");
            let recorded = RecordedLlmProfile::mint("host/model".into(), metadata);
            assert_eq!(recorded.context_window_tokens(), prompt_budget);
            let config = LlmProfileConfig::new(recorded);
            assert_eq!(config.context_window_tokens(), prompt_budget);
        }
    }

    #[test]
    fn output_token_limits_validate_stored_facts_and_reject_the_old_shape() {
        for wire in [
            serde_json::json!({"capacity": 0, "default_cap": null}),
            serde_json::json!({"capacity": 1024, "default_cap": 0}),
            serde_json::json!({"capacity": 1024, "default_cap": 2048}),
            serde_json::json!({"output_token_capacity": 1024}),
        ] {
            assert!(serde_json::from_value::<OutputTokenLimits>(wire).is_err());
        }
        for (capacity, default_cap) in [
            (None, None),
            (Some(1024), None),
            (None, Some(2048)),
            (Some(1024), Some(1024)),
        ] {
            let limits = OutputTokenLimits::new(capacity, default_cap).expect("valid limits");
            let wire = serde_json::to_value(&limits).expect("serialize limits");
            assert_eq!(
                wire,
                serde_json::json!({"capacity": capacity, "default_cap": default_cap})
            );
            assert_eq!(
                serde_json::from_value::<OutputTokenLimits>(wire).expect("decode limits"),
                limits
            );
        }
        assert_eq!(
            OutputTokenLimits::new(Some(1024), Some(2048)),
            Err(LlmProfileLimitsError::OutputTokenDefaultExceedsCapacity {
                capacity: NonZeroUsize::new(1024).expect("positive"),
                default_cap: NonZeroUsize::new(2048).expect("positive"),
            })
        );
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
                reasoning: Some(crate::llm::capability::ReasoningCapability {
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
            spec.limits.output_tokens.capacity().map(NonZeroUsize::get),
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
