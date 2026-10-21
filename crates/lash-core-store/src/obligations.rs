//! Claim obligations (S7, FIG-6074): the callbacks an actor's outstanding
//! recorded work will call, as the tokens its `requires` carries.
//!
//! One token family names every recorded callback, qualified by the registry
//! that resolves it: a session's plugins or a process's components. A node
//! serves the token of every callback its installed registrations resolve,
//! without invoking any. A deployment may also serve a retired owner's
//! token to claim its outstanding work and refuse it before redelivery.
//! Otherwise, an actor naming an unresolved callback is claimed only for
//! cancellation. Component-format tokens are a separate family of the codec,
//! and so are the hard requirements an execution's record names: the
//! natives, kernels, providers and codec versions its code and links need,
//! which a node serves from the capabilities it advertises (FIG-6993).
//!
//! [`ObligationToken`] is the one encoder: a requirement and a served
//! registration are spelled by it and nowhere else.

/// A registry retirement refuses new admission or a recorded dependency.
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
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum RetirementRefusal {
    #[error("{registry:?} registration `{id}` is retired")]
    Admission { registry: RegistryKind, id: String },
    #[error("{registry:?} registration `{id}` is retired but serves `{effect}`")]
    Serves {
        registry: RegistryKind,
        id: String,
        effect: String,
    },
    #[error("{registry:?} registration `{id}` is retired but holds recorded state")]
    HasState { registry: RegistryKind, id: String },
}

impl RetirementRefusal {
    /// Retain the typed refusal through the runtime boundary.
    #[must_use]
    pub fn into_runtime_error(self) -> crate::RuntimeError {
        crate::RuntimeError::new(crate::RuntimeErrorCode::PluginRetired, self.to_string())
            .with_cause(crate::RuntimeErrorCause::PluginRetired {
                refusal: Box::new(self),
            })
    }
}

pub use lash_vocabulary::claims::{ObligationToken, RegistryKind};
