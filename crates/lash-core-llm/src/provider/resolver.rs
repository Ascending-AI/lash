use super::ProviderHandle;

#[derive(Clone, Debug)]
pub struct ProviderBinding {
    pub provider: ProviderHandle,
}

impl ProviderBinding {
    pub fn new(
        provider_id: impl Into<String>,
        provider: ProviderHandle,
    ) -> Result<Self, ProviderResolutionError> {
        let provider_id = provider_id.into();
        let requested = provider_id.trim();
        if requested.is_empty() {
            return Err(ProviderResolutionError::MissingProviderId);
        }
        let actual = provider.kind();
        if actual != requested {
            return Err(ProviderResolutionError::ProviderIdMismatch {
                expected: requested.to_string(),
                actual: actual.to_string(),
            });
        }
        Ok(Self { provider })
    }

    pub fn from_provider(provider: ProviderHandle) -> Self {
        Self { provider }
    }
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProviderResolutionError {
    #[error("session policy does not specify provider_id")]
    MissingProviderId,
    #[error("provider `{provider_id}` is not registered with the runtime host")]
    UnknownProvider { provider_id: String },
    #[error("provider resolver returned `{actual}` for requested provider `{expected}`")]
    ProviderIdMismatch { expected: String, actual: String },
}

/// Why a session-config route is refused (FIG-3600 S6, D3 §3.3): the typed
/// refusal a config command that changes the route settles with, at send or
/// at apply. The route is never changed when it is refused.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
    thiserror::Error,
)]
#[serde(rename_all = "snake_case")]
pub enum ConfigRefusalCode {
    /// No provider this host resolves serves the route.
    #[error("no provider serves the route")]
    ProviderRouteUnknown,
    /// The route's provider is known, but the host holds no credentials for
    /// it.
    #[error("the route's provider has no credentials")]
    ProviderCredentialsMissing,
}

pub trait RuntimeProviderResolver: Send + Sync {
    /// Resolver implementations are configured by default. The runtime's
    /// [`EmptyProviderResolver`] sentinel overrides this to report absence.
    fn is_configured(&self) -> bool {
        true
    }

    fn resolve_provider_binding(
        &self,
        provider_id: &str,
    ) -> Result<ProviderBinding, ProviderResolutionError>;

    /// Whether this host can serve the route `provider_id` + `model`
    /// (FIG-3600 S6, D3 §3.3). A config command that changes the route is
    /// validated here when it is sent and again when it is applied; a turn
    /// never validates, it binds the route its config recorded.
    ///
    /// The default serves exactly the providers
    /// [`resolve_provider_binding`](Self::resolve_provider_binding) binds, so
    /// credentials and model availability stay the host resolver's business:
    /// a resolver that knows them refuses with the precise code.
    fn validate_route(
        &self,
        provider_id: &str,
        model: &crate::model::ModelSpec,
    ) -> Result<(), ConfigRefusalCode> {
        let _ = model;
        self.resolve_provider_binding(provider_id)
            .map(|_| ())
            .map_err(|_| ConfigRefusalCode::ProviderRouteUnknown)
    }
}

#[derive(Clone, Debug, Default)]
pub struct EmptyProviderResolver;

impl RuntimeProviderResolver for EmptyProviderResolver {
    fn is_configured(&self) -> bool {
        false
    }

    fn resolve_provider_binding(
        &self,
        provider_id: &str,
    ) -> Result<ProviderBinding, ProviderResolutionError> {
        let provider_id = provider_id.trim();
        if provider_id.is_empty() {
            return Err(ProviderResolutionError::MissingProviderId);
        }
        Err(ProviderResolutionError::UnknownProvider {
            provider_id: provider_id.to_string(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct SingleProviderResolver {
    provider_id: String,
    provider: ProviderHandle,
}

impl SingleProviderResolver {
    pub fn new(provider: ProviderHandle) -> Self {
        Self {
            provider_id: provider.kind().to_string(),
            provider,
        }
    }
}

impl RuntimeProviderResolver for SingleProviderResolver {
    fn resolve_provider_binding(
        &self,
        provider_id: &str,
    ) -> Result<ProviderBinding, ProviderResolutionError> {
        let requested = provider_id.trim();
        if requested.is_empty() {
            return Err(ProviderResolutionError::MissingProviderId);
        }
        if requested != self.provider_id {
            return Err(ProviderResolutionError::UnknownProvider {
                provider_id: requested.to_string(),
            });
        }
        ProviderBinding::new(requested, self.provider.clone())
    }
}

/// A resolver over several providers, keyed by each one's
/// [`kind`](ProviderHandle::kind) (D3 Q10): a session config names its route
/// by provider id, and a config command can move the session to any provider
/// the registry holds.
#[derive(Clone, Debug, Default)]
pub struct ProviderRegistry {
    providers: std::collections::BTreeMap<String, ProviderHandle>,
}

/// A second provider registered under an id the registry already holds.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("provider `{provider_id}` is registered twice")]
pub struct DuplicateProviderId {
    pub provider_id: String,
}

impl ProviderRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `provider` under its kind; one handle per id.
    pub fn with(mut self, provider: ProviderHandle) -> Result<Self, DuplicateProviderId> {
        let provider_id = provider.kind().to_string();
        if self.providers.contains_key(&provider_id) {
            return Err(DuplicateProviderId { provider_id });
        }
        self.providers.insert(provider_id, provider);
        Ok(self)
    }
}

impl RuntimeProviderResolver for ProviderRegistry {
    fn resolve_provider_binding(
        &self,
        provider_id: &str,
    ) -> Result<ProviderBinding, ProviderResolutionError> {
        let requested = provider_id.trim();
        if requested.is_empty() {
            return Err(ProviderResolutionError::MissingProviderId);
        }
        let provider = self.providers.get(requested).ok_or_else(|| {
            ProviderResolutionError::UnknownProvider {
                provider_id: requested.to_string(),
            }
        })?;
        ProviderBinding::new(requested, provider.clone())
    }
}
