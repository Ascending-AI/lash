use std::collections::BTreeMap;

use super::ProviderHandle;

#[derive(Clone, Debug)]
pub struct ProviderBinding {
    pub provider_id: String,
    pub provider: ProviderHandle,
}

impl Default for ProviderBinding {
    fn default() -> Self {
        Self {
            provider_id: String::new(),
            provider: ProviderHandle::unconfigured(),
        }
    }
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
        Ok(Self {
            provider_id: requested.to_string(),
            provider,
        })
    }

    pub fn from_provider(provider: ProviderHandle) -> Self {
        Self {
            provider_id: provider.kind().to_string(),
            provider,
        }
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

pub trait RuntimeProviderResolver: Send + Sync {
    fn resolve_provider_binding(
        &self,
        provider_id: &str,
    ) -> Result<ProviderBinding, ProviderResolutionError>;
}

#[derive(Clone, Debug, Default)]
pub struct EmptyProviderResolver;

impl RuntimeProviderResolver for EmptyProviderResolver {
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

#[derive(Clone, Debug, Default)]
pub struct MapProviderResolver {
    providers: BTreeMap<String, ProviderHandle>,
}

impl MapProviderResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_provider(mut self, provider: ProviderHandle) -> Self {
        self.providers.insert(provider.kind().to_string(), provider);
        self
    }

    pub fn with_provider_id(
        mut self,
        provider_id: impl Into<String>,
        provider: ProviderHandle,
    ) -> Self {
        self.providers.insert(provider_id.into(), provider);
        self
    }
}

impl RuntimeProviderResolver for MapProviderResolver {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(kind: &'static str) -> ProviderHandle {
        crate::testing::TestProvider::builder()
            .kind(kind)
            .build()
            .into_handle()
    }

    #[test]
    fn map_provider_resolver_returns_registered_provider() {
        let resolver = MapProviderResolver::new().with_provider(provider("mock"));

        let resolved = resolver
            .resolve_provider_binding("mock")
            .expect("registered provider resolves");

        assert_eq!(resolved.provider_id, "mock");
        assert_eq!(resolved.provider.kind(), "mock");
    }

    #[test]
    fn map_provider_resolver_reports_missing_provider_id() {
        let resolver = MapProviderResolver::new().with_provider(provider("mock"));

        let err = resolver
            .resolve_provider_binding("  ")
            .expect_err("empty provider id is rejected");

        assert_eq!(err, ProviderResolutionError::MissingProviderId);
    }

    #[test]
    fn map_provider_resolver_reports_unknown_provider_id() {
        let resolver = MapProviderResolver::new().with_provider(provider("mock"));

        let err = resolver
            .resolve_provider_binding("other")
            .expect_err("unknown provider id is rejected");

        assert_eq!(
            err,
            ProviderResolutionError::UnknownProvider {
                provider_id: "other".to_string()
            }
        );
    }

    #[test]
    fn map_provider_resolver_reports_provider_id_mismatch() {
        let resolver = MapProviderResolver::new().with_provider_id("recorded", provider("actual"));

        let err = resolver
            .resolve_provider_binding("recorded")
            .expect_err("mismatched live provider is rejected");

        assert_eq!(
            err,
            ProviderResolutionError::ProviderIdMismatch {
                expected: "recorded".to_string(),
                actual: "actual".to_string()
            }
        );
    }
}
