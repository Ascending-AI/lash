use std::num::NonZeroUsize;

use crate::provider::{ModelCapability, ReasoningSelection};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    pub id: String,
    #[serde(default)]
    pub variant: ReasoningSelection,
    pub limits: ModelLimits,
    /// Host-supplied capability metadata: reasoning controls and cache-control
    /// dialect accepted by this model route. Lash validates the requested
    /// variant and threads all capability data onto every provider request.
    #[serde(default, skip_serializing_if = "ModelCapability::is_empty")]
    pub capability: ModelCapability,
}

impl ModelSpec {
    /// Constructs provider-default model policy for protocol implementors with a non-zero prompt
    /// budget, no known output ceiling, and no extra capabilities.
    pub fn new(id: impl Into<String>, context_window_tokens: NonZeroUsize) -> Self {
        Self {
            id: id.into(),
            variant: ReasoningSelection::ProviderDefault,
            limits: ModelLimits {
                context_window_tokens,
                output_token_capacity: None,
            },
            capability: ModelCapability::default(),
        }
    }

    /// Sets the limits carried by a `ModelSpec` for protocol and process-engine implementors while
    /// materializing protocol-specific session and turn state.
    pub fn with_limits(
        id: impl Into<String>,
        variant: ReasoningSelection,
        limits: ModelLimits,
    ) -> Self {
        Self {
            id: id.into(),
            variant,
            limits,
            capability: ModelCapability::default(),
        }
    }

    /// Sets the variant carried by a `ModelSpec` for protocol and process-engine implementors while
    /// materializing protocol-specific session and turn state.
    pub fn with_variant(mut self, variant: ReasoningSelection) -> Self {
        self.variant = variant;
        self
    }

    /// Sets the capability carried by a `ModelSpec` for protocol and process-engine implementors
    /// while materializing protocol-specific session and turn state.
    pub fn with_capability(mut self, capability: ModelCapability) -> Self {
        self.capability = capability;
        self
    }

    /// Build a spec from the prompt budget (`context_window_tokens` — the
    /// maximum input the provider accepts for this model on this route) and the
    /// optional output cap. The budget is what history pruning and the UI
    /// measure against.
    pub fn from_token_limits(
        id: impl Into<String>,
        variant: ReasoningSelection,
        context_window_tokens: usize,
        output_token_capacity: Option<usize>,
    ) -> Result<Self, String> {
        Ok(Self::with_limits(
            id,
            variant,
            ModelLimits::from_token_limits(context_window_tokens, output_token_capacity)?,
        ))
    }

    /// Exposes the non-zero prompt budget protocol implementors use for history pruning rather than
    /// the optional output-token ceiling.
    pub fn context_window_tokens(&self) -> usize {
        self.limits.context_window_tokens.get()
    }

    /// Reduce a requested output-token cap to what this model can produce, and
    /// report whether it had to.
    ///
    /// The cap is a bound, not a demand: a caller asking for at most 32k is
    /// satisfied by a model that can only produce 8k. Because the cap is
    /// durable session policy, failing instead would leave a session that
    /// switched to a smaller model failing every call it makes from then on,
    /// so every path that takes generation options *from a session policy*
    /// clamps them against the model that same policy names.
    ///
    /// A model that declares no `output_token_capacity` clamps nothing: an
    /// unknown ceiling is not a ceiling of zero.
    pub(crate) fn clamp_generation_options(
        &self,
        generation: &mut crate::GenerationOptions,
    ) -> bool {
        let (Some(requested), Some(capacity)) = (
            generation.output_token_cap,
            self.limits.output_token_capacity,
        ) else {
            return false;
        };
        if requested <= capacity {
            return false;
        }
        tracing::debug!(
            model = %self.id,
            requested = requested.get(),
            capacity = capacity.get(),
            "clamping requested output_token_cap to the model's output_token_capacity"
        );
        generation.output_token_cap = Some(capacity);
        true
    }

    /// The session's generation options as they can actually run on this
    /// model. Callers that hand a whole session policy to a path with no
    /// capacity of its own — a plugin's direct request — take the options from
    /// here rather than from the policy directly.
    pub(crate) fn clamped_generation(
        &self,
        generation: &crate::GenerationOptions,
    ) -> crate::GenerationOptions {
        let mut clamped = generation.clone();
        self.clamp_generation_options(&mut clamped);
        clamped
    }
}

impl Default for ModelSpec {
    fn default() -> Self {
        Self::new(
            String::new(),
            NonZeroUsize::new(1).expect("one is non-zero"),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLimits {
    /// The prompt budget: the maximum input tokens the provider accepts for
    /// this model on this route. History pruning and the UI measure against
    /// this — not the model's total context (input + output), which would
    /// over-budget by the whole response reservation.
    pub context_window_tokens: NonZeroUsize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_token_capacity: Option<NonZeroUsize>,
}

impl ModelLimits {
    /// Validates model limits for protocol implementors, rejecting zero prompt budgets and present
    /// zero output capacities while preserving an unknown output ceiling as `None`.
    pub fn from_token_limits(
        context_window_tokens: usize,
        output_token_capacity: Option<usize>,
    ) -> Result<Self, String> {
        Ok(Self {
            context_window_tokens: nonzero_token_limit(
                "context_window_tokens",
                context_window_tokens,
            )?,
            output_token_capacity: optional_nonzero_token_limit(
                "output_token_capacity",
                output_token_capacity,
            )?,
        })
    }
}

impl Default for ModelLimits {
    fn default() -> Self {
        Self {
            context_window_tokens: NonZeroUsize::new(1).expect("one is non-zero"),
            output_token_capacity: None,
        }
    }
}

fn nonzero_token_limit(name: &str, value: usize) -> Result<NonZeroUsize, String> {
    NonZeroUsize::new(value).ok_or_else(|| format!("{name} must be greater than zero"))
}

fn optional_nonzero_token_limit(
    name: &str,
    value: Option<usize>,
) -> Result<Option<NonZeroUsize>, String> {
    value
        .map(|value| nonzero_token_limit(name, value))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_spec_reasoning_selection_serde_is_explicit() {
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
            let spec = ModelSpec::new("model", NonZeroUsize::new(1024).expect("non-zero"))
                .with_variant(selection.clone());
            let json = serde_json::to_value(&spec).expect("serialize model spec");
            assert_eq!(json["variant"], expected);
            let round_trip: ModelSpec =
                serde_json::from_value(json).expect("deserialize model spec");
            assert_eq!(round_trip.variant, selection);
        }
    }

    #[test]
    fn model_spec_constructors_preserve_identity_variant_and_limits() {
        let limits = ModelLimits::from_token_limits(8_192, Some(1_024)).expect("valid limits");

        let spec = ModelSpec::with_limits(
            "provider/model",
            ReasoningSelection::Effort("fast".to_string()),
            limits.clone(),
        );

        assert_eq!(spec.id, "provider/model");
        assert_eq!(spec.variant, ReasoningSelection::Effort("fast".to_string()));
        assert_eq!(spec.limits, limits);

        let changed = spec
            .clone()
            .with_variant(ReasoningSelection::Effort("accurate".to_string()));
        assert_eq!(changed.id, "provider/model");
        assert_eq!(
            changed.variant,
            ReasoningSelection::Effort("accurate".to_string())
        );
        assert_eq!(changed.limits, spec.limits);

        let cleared = changed.with_variant(ReasoningSelection::ProviderDefault);
        assert_eq!(cleared.id, "provider/model");
        assert_eq!(cleared.variant, ReasoningSelection::ProviderDefault);
        assert_eq!(cleared.context_window_tokens(), 8_192);
    }

    #[test]
    fn model_token_limit_constructors_reject_zero_and_preserve_output_cap() {
        let spec = ModelSpec::from_token_limits(
            "provider/model",
            ReasoningSelection::Effort("variant-a".to_string()),
            200_000,
            Some(4_096),
        )
        .expect("valid token limits");

        assert_eq!(spec.id, "provider/model");
        assert_eq!(
            spec.variant,
            ReasoningSelection::Effort("variant-a".to_string())
        );
        assert_eq!(spec.context_window_tokens(), 200_000);
        assert_eq!(
            spec.limits.output_token_capacity.map(NonZeroUsize::get),
            Some(4_096)
        );

        let context_error =
            ModelSpec::from_token_limits("bad-context", Default::default(), 0, Some(1))
                .expect_err("zero context");
        assert!(
            context_error.contains("context_window_tokens"),
            "context error should name the invalid field: {context_error}"
        );

        let output_error = ModelLimits::from_token_limits(1, Some(0)).expect_err("zero output cap");
        assert!(
            output_error.contains("output_token_capacity"),
            "output error should name the invalid field: {output_error}"
        );
    }

    #[test]
    fn clamped_generation_bounds_a_cap_by_capacity_and_leaves_the_rest_alone() {
        let requested = crate::GenerationOptions {
            output_token_cap: NonZeroUsize::new(32_000),
            temperature: Some(crate::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
            seed: Some(42),
        };

        let bounded =
            ModelSpec::from_token_limits("small", Default::default(), 200_000, Some(2_048))
                .expect("valid model");
        let clamped = bounded.clamped_generation(&requested);
        assert_eq!(clamped.output_token_cap, NonZeroUsize::new(2_048));
        assert_eq!(clamped.temperature, requested.temperature);
        assert_eq!(clamped.seed, requested.seed);

        let roomy =
            ModelSpec::from_token_limits("roomy", Default::default(), 200_000, Some(64_000))
                .expect("valid model");
        assert_eq!(roomy.clamped_generation(&requested), requested);

        // An unknown ceiling is not a ceiling of zero.
        let unbounded = ModelSpec::from_token_limits("unknown", Default::default(), 200_000, None)
            .expect("valid model");
        assert_eq!(unbounded.clamped_generation(&requested), requested);
    }
}
