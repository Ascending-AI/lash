//! The host's model registry: which models a deployment serves, and the
//! executable transport behind each one.
//!
//! Selection and execution are separate moments. A session selects a model
//! by its opaque [`ModelKey`]; the registry mints a [`RecordedModel`] for it
//! ([`RuntimeModels::snapshot`]), and the session records that value at
//! creation and at every model change. Execution later binds the recorded
//! value to a live [`ProviderHandle`] ([`RuntimeModels::bind`]), which serves
//! the recorded contract or refuses typed. Nothing re-reads the catalog to
//! decide how a recorded session or root runs.

use std::collections::BTreeMap;

use super::ProviderHandle;
use crate::model::{ModelKey, ModelMetadata, RecordedModel};

/// Why a key or a recorded model has no binding on this deployment.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("model `{key}` is unavailable: {reason}")]
pub struct ModelUnavailable {
    pub key: ModelKey,
    pub reason: ModelUnavailableReason,
}

impl ModelUnavailable {
    pub fn new(key: ModelKey, reason: ModelUnavailableReason) -> Self {
        Self { key, reason }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ModelUnavailableReason {
    /// The deployment registers no models at all.
    #[error("this deployment registers no models")]
    NoModels,
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
pub trait RuntimeModels: Send + Sync {
    /// Whether this deployment registers models at all. Only the
    /// [`EmptyModels`] sentinel of a core built without a registry reports
    /// `false`.
    fn is_configured(&self) -> bool {
        true
    }

    /// Mint the binding `key` names now. Called when a selection is adopted:
    /// at session creation, at a model patch (even one naming the current
    /// key), and when a root resolves an explicit per-run key. Never called
    /// for a value that is already recorded.
    fn snapshot(&self, key: &ModelKey) -> Result<RecordedModel, ModelUnavailable>;

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
    fn bind(&self, recorded: &RecordedModel) -> Result<ProviderHandle, ModelUnavailable>;
}

/// The runtime sentinel of a core built without a model registry: it
/// answers every lookup with [`ModelUnavailableReason::NoModels`].
#[derive(Clone, Debug, Default)]
pub struct EmptyModels;

impl RuntimeModels for EmptyModels {
    fn is_configured(&self) -> bool {
        false
    }

    fn snapshot(&self, key: &ModelKey) -> Result<RecordedModel, ModelUnavailable> {
        Err(ModelUnavailable::new(
            key.clone(),
            ModelUnavailableReason::NoModels,
        ))
    }

    fn bind(&self, recorded: &RecordedModel) -> Result<ProviderHandle, ModelUnavailable> {
        Err(ModelUnavailable::new(
            recorded.key().clone(),
            ModelUnavailableReason::NoModels,
        ))
    }
}

/// One registration: the model's metadata and the transport that serves it.
/// Several registrations may share one transport handle (and so its rate
/// limiter), and several may share one provider kind.
#[derive(Clone, Debug)]
pub struct RegisteredModel {
    metadata: ModelMetadata,
    provider: ProviderHandle,
}

impl RegisteredModel {
    pub fn new(metadata: ModelMetadata, provider: ProviderHandle) -> Self {
        Self { metadata, provider }
    }

    pub fn metadata(&self) -> &ModelMetadata {
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
    DuplicateKey { key: ModelKey },
}

/// The standard [`RuntimeModels`]: registrations keyed by opaque
/// [`ModelKey`].
#[derive(Clone, Debug, Default)]
pub struct ModelRegistry {
    models: BTreeMap<ModelKey, RegisteredModel>,
}

impl ModelRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `entry` under `key`. Keys are opaque: the registry never
    /// derives one from a provider kind or parses a model from one.
    pub fn register(
        mut self,
        key: impl Into<ModelKey>,
        entry: RegisteredModel,
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

    fn registered(&self, key: &ModelKey) -> Result<&RegisteredModel, ModelUnavailable> {
        self.models
            .get(key)
            .ok_or_else(|| ModelUnavailable::new(key.clone(), ModelUnavailableReason::UnknownKey))
    }
}

impl RuntimeModels for ModelRegistry {
    fn snapshot(&self, key: &ModelKey) -> Result<RecordedModel, ModelUnavailable> {
        let entry = self.registered(key)?;
        Ok(RecordedModel::mint(key.clone(), entry.metadata.clone()))
    }

    /// Refuses a key that is not registered and a key whose registration
    /// names another wire model. The transport's kind is not compared: see
    /// [`RuntimeModels::bind`].
    fn bind(&self, recorded: &RecordedModel) -> Result<ProviderHandle, ModelUnavailable> {
        let entry = self.registered(recorded.key())?;
        if entry.metadata.wire_model != recorded.wire_model() {
            return Err(ModelUnavailable::new(
                recorded.key().clone(),
                ModelUnavailableReason::WireModelChanged {
                    recorded: recorded.wire_model().to_string(),
                    served: entry.metadata.wire_model.clone(),
                },
            ));
        }
        Ok(entry.provider.clone())
    }
}
