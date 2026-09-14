//! Session vocabulary owned by the LLM layer.
//!
//! Only the charge-safety policy lives here today: the provider handle needs
//! it by type, and `lash-core`'s `session_model` re-exports it at its original
//! path. The rest of `session_model` stays in `lash-core` until the layers
//! above this crate are carved out.

/// Host appetite for retrying after a provider may already have billed a
/// generation that Lash cannot resume or replay idempotently.
///
/// # Integrator class
///
/// Host applications choose this policy when constructing or reopening a
/// session. Provider adapters continue to report retry guarantees as facts.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ChargeSafetyPolicy {
    /// Require an idempotency or resume guarantee before buying another
    /// generation after output or ambiguous response evidence was observed.
    #[default]
    RequireGuarantee,
    /// Permit bounded duplicate billing when no provider guarantee exists.
    AcceptDuplicateBilling {
        /// Maximum unsafe retries per logical LLM call. Lash hard-clamps this
        /// value to five.
        max_unsafe_retries: u8,
        /// Skip the unsafe retry when provider-reported tokens already billed
        /// for the abandoned generation exceed this bound. When the provider
        /// reports no partial usage, Lash treats the tokens at stake as zero,
        /// so this cost bound does not bind; the hard clamp of at most five
        /// unsafe retries still applies.
        max_duplicate_cost_tokens: Option<u64>,
    },
}
