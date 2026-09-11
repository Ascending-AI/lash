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
#[non_exhaustive]
pub enum ProviderResolutionError {
    #[error("session policy does not specify provider_id")]
    MissingProviderId,
    #[error("provider `{provider_id}` is not registered with the runtime host")]
    UnknownProvider { provider_id: String },
    #[error("provider resolver returned `{actual}` for requested provider `{expected}`")]
    ProviderIdMismatch { expected: String, actual: String },
}

pub trait RuntimeProviderResolver: Send + Sync {
    /// Reports whether this resolver was configured by the host.
    ///
    /// Resolver implementations are configured by default. The runtime's
    /// [`EmptyProviderResolver`] sentinel overrides this to report absence.
    fn is_configured(&self) -> bool {
        true
    }

    fn resolve_provider_binding(
        &self,
        provider_id: &str,
    ) -> Result<ProviderBinding, ProviderResolutionError>;
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
