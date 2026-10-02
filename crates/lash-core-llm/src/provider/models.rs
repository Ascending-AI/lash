//! The host's model registry: which models a deployment serves, and the
//! executable transport behind each one.
//!
//! Selection and execution are separate moments. A session selects a model
//! by its opaque [`LlmProfileKey`]; the registry mints a [`RecordedLlmProfile`] for it
//! ([`LlmProfiles::snapshot`]), and the session records that value at
//! creation and at every model change. Execution later binds the recorded
//! value to a live [`ProviderHandle`] ([`LlmProfiles::bind`]), which serves
//! the recorded contract or refuses typed. Nothing re-reads the catalog to
//! decide how a recorded session or root runs.

use std::collections::BTreeMap;

use super::ProviderHandle;
use crate::llm_profile::{LlmProfileKey, LlmProfileMetadata, RecordedLlmProfile};

/// Why a key or a recorded model has no binding on this deployment.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("model `{key}` is unavailable: {reason}")]
pub struct LlmProfileUnavailable {
    pub key: LlmProfileKey,
    pub reason: LlmProfileUnavailableReason,
}

impl LlmProfileUnavailable {
    pub fn new(key: LlmProfileKey, reason: LlmProfileUnavailableReason) -> Self {
        Self { key, reason }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LlmProfileUnavailableReason {
    /// The deployment registers no models at all.
    #[error("this deployment registers no models")]
    NoLlmProfiles,
    /// The registry holds no model under the key.
    #[error("no model is registered under the key")]
    UnknownKey,
    /// The key is registered, but its transport now serves another wire
    /// model than the one recorded: running it would send the recorded
    /// request to a model it was never recorded for.
    #[error("the key's transport serves wire model `{served}`, not the recorded `{recorded}`")]
    WireModelChanged { recorded: String, served: String },
}

/// The host's models: mint a binding for a key, and bind a recorded binding
/// to a live transport.
pub trait LlmProfiles: Send + Sync {
    /// Whether this deployment registers models at all. Only the
    /// [`EmptyLlmProfiles`] sentinel of a core built without a registry reports
    /// `false`.
    fn is_configured(&self) -> bool {
        true
    }

    /// Mint the binding `key` names now. Called when a selection is adopted:
    /// at session creation, at a model patch (even one naming the current
    /// key), and when a root resolves an explicit per-run key. Never called
    /// for a value that is already recorded.
    fn snapshot(&self, key: &LlmProfileKey) -> Result<RecordedLlmProfile, LlmProfileUnavailable>;

    /// The transport that executes `recorded`. It must serve the recorded
    /// wire model and contract, never a newer descriptor; a transport that
    /// cannot refuses typed.
    ///
    /// The handle is this worker's capability, not a recorded fact
    /// (FIG-4531). A binding records its key and metadata: the wire model,
    /// limits, capability and request defaults every request of the session
    /// is built from. It records nothing about the transport, so `bind`
    /// answers whichever transport the key is registered with on this
    /// worker, of whatever provider kind, and a session that is mid-root
    /// continues on it. Registering a recorded key with a transport is the
    /// host's statement that the transport serves the recorded contract
    /// (instruction role, cache-control dialect, reasoning encoding): lash
    /// checks the wire model and nothing else. A host that moves a key to a
    /// transport of a kind that cannot honour bindings minted on the old one
    /// registers the new transport under a new key instead.
    fn bind(&self, recorded: &RecordedLlmProfile) -> Result<ProviderHandle, LlmProfileUnavailable>;
}

/// The runtime sentinel of a core built without a model registry: it
/// answers every lookup with [`LlmProfileUnavailableReason::NoLlmProfiles`].
#[derive(Clone, Debug, Default)]
pub struct EmptyLlmProfiles;

impl LlmProfiles for EmptyLlmProfiles {
    fn is_configured(&self) -> bool {
        false
    }

    fn snapshot(&self, key: &LlmProfileKey) -> Result<RecordedLlmProfile, LlmProfileUnavailable> {
        Err(LlmProfileUnavailable::new(
            key.clone(),
            LlmProfileUnavailableReason::NoLlmProfiles,
        ))
    }

    fn bind(&self, recorded: &RecordedLlmProfile) -> Result<ProviderHandle, LlmProfileUnavailable> {
        Err(LlmProfileUnavailable::new(
            recorded.key().clone(),
            LlmProfileUnavailableReason::NoLlmProfiles,
        ))
    }
}

/// One registration: the model's metadata and the transport that serves it.
/// Several registrations may share one transport handle (and so its rate
/// limiter), and several may share one provider kind.
#[derive(Clone, Debug)]
pub struct RegisteredLlmProfile {
    metadata: LlmProfileMetadata,
    provider: ProviderHandle,
}

impl RegisteredLlmProfile {
    pub fn new(metadata: LlmProfileMetadata, provider: ProviderHandle) -> Self {
        Self { metadata, provider }
    }

    pub fn metadata(&self) -> &LlmProfileMetadata {
        &self.metadata
    }

    pub fn provider(&self) -> &ProviderHandle {
        &self.provider
    }
}

/// A registration the registry refuses.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RegistrationError {
    #[error("a model key must not be empty")]
    EmptyKey,
    #[error("model `{key}` is registered twice")]
    DuplicateKey { key: LlmProfileKey },
}

/// The standard [`LlmProfiles`]: registrations keyed by opaque
/// [`LlmProfileKey`].
#[derive(Clone, Debug, Default)]
pub struct LlmProfileRegistry {
    models: BTreeMap<LlmProfileKey, RegisteredLlmProfile>,
}

impl LlmProfileRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `entry` under `key`. Keys are opaque: the registry never
    /// derives one from a provider kind or parses a model from one.
    pub fn register(
        mut self,
        key: impl Into<LlmProfileKey>,
        entry: RegisteredLlmProfile,
    ) -> Result<Self, RegistrationError> {
        let key = key.into();
        if key.as_str().trim().is_empty() {
            return Err(RegistrationError::EmptyKey);
        }
        if self.models.contains_key(&key) {
            return Err(RegistrationError::DuplicateKey { key });
        }
        self.models.insert(key, entry);
        Ok(self)
    }

    fn registered(
        &self,
        key: &LlmProfileKey,
    ) -> Result<&RegisteredLlmProfile, LlmProfileUnavailable> {
        self.models.get(key).ok_or_else(|| {
            LlmProfileUnavailable::new(key.clone(), LlmProfileUnavailableReason::UnknownKey)
        })
    }
}

impl LlmProfiles for LlmProfileRegistry {
    fn snapshot(&self, key: &LlmProfileKey) -> Result<RecordedLlmProfile, LlmProfileUnavailable> {
        let entry = self.registered(key)?;
        Ok(RecordedLlmProfile::mint(
            key.clone(),
            entry.metadata.clone(),
        ))
    }

    /// Refuses a key that is not registered and a key whose registration
    /// names another wire model. The transport's kind is not compared: see
    /// [`LlmProfiles::bind`].
    fn bind(&self, recorded: &RecordedLlmProfile) -> Result<ProviderHandle, LlmProfileUnavailable> {
        let entry = self.registered(recorded.key())?;
        if entry.metadata.wire_model != recorded.wire_model() {
            return Err(LlmProfileUnavailable::new(
                recorded.key().clone(),
                LlmProfileUnavailableReason::WireModelChanged {
                    recorded: recorded.wire_model().to_string(),
                    served: entry.metadata.wire_model.clone(),
                },
            ));
        }
        Ok(entry.provider.clone())
    }
}
