use crate::{GenerationOptions, ModelSpec};

/// `ModelSpec` lives in `lash-core-llm`; these two methods stay here because
/// every caller is in `lash-core` and neither belongs on the type's public
/// surface. The trait is crate-internal, so the published API is unchanged.
pub trait ModelGenerationClamp {
    fn clamp_generation_options(&self, generation: &mut GenerationOptions) -> bool;
    fn clamped_generation(&self, generation: &GenerationOptions) -> GenerationOptions;
}

impl ModelGenerationClamp for ModelSpec {
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
    fn clamp_generation_options(&self, generation: &mut GenerationOptions) -> bool {
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
    fn clamped_generation(&self, generation: &GenerationOptions) -> GenerationOptions {
        let mut clamped = generation.clone();
        self.clamp_generation_options(&mut clamped);
        clamped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    #[test]
    fn clamped_generation_bounds_a_cap_by_capacity_and_leaves_the_rest_alone() {
        let requested = crate::GenerationOptions {
            output_token_cap: NonZeroUsize::new(32_000),
            temperature: Some(crate::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
            seed: Some(42),
            stop_sequences: Vec::new(),
            projection_provenance: Default::default(),
        };

        let bounded = ModelSpec::builder("small")
            .context_window_tokens(200_000)
            .output_token_capacity(2_048)
            .build()
            .expect("valid model");
        let clamped = bounded.clamped_generation(&requested);
        assert_eq!(clamped.output_token_cap, NonZeroUsize::new(2_048));
        assert_eq!(clamped.temperature, requested.temperature);
        assert_eq!(clamped.seed, requested.seed);

        let roomy = ModelSpec::builder("roomy")
            .context_window_tokens(200_000)
            .output_token_capacity(64_000)
            .build()
            .expect("valid model");
        assert_eq!(roomy.clamped_generation(&requested), requested);

        // An unknown ceiling is not a ceiling of zero.
        let unbounded = ModelSpec::builder("unknown")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model");
        assert_eq!(unbounded.clamped_generation(&requested), requested);
    }
}
