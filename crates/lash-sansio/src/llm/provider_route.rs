use super::types::ProviderRouteIdentity;

impl ProviderRouteIdentity {
    pub fn new(
        provider: impl Into<String>,
        endpoint: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into().into_boxed_str(),
            endpoint: endpoint.into().into_boxed_str(),
            model: model.into().into_boxed_str(),
        }
    }

    /// Construct a route from an HTTP(S) endpoint, normalizing the scheme and
    /// authority case and removing trailing slashes from the path. Query and
    /// fragment bytes are preserved verbatim.
    pub fn for_endpoint(
        provider: impl Into<String>,
        endpoint: &str,
        model: impl Into<String>,
    ) -> Self {
        Self::new(provider, normalize_provider_endpoint(endpoint), model)
    }

    /// Validate security-sensitive invariants of an HTTP(S) route endpoint.
    /// Opaque host-supplied route ids are accepted unchanged.
    pub fn validate_endpoint(&self) -> Result<(), ProviderEndpointError> {
        let Some((_, remainder)) = self.endpoint.split_once("://") else {
            return Ok(());
        };
        let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
        if remainder[..authority_end].contains('@') {
            return Err(ProviderEndpointError::UserinfoNotAllowed);
        }
        Ok(())
    }
}

fn normalize_provider_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim();
    let Some((scheme, remainder)) = trimmed.split_once("://") else {
        return trimmed.trim_end_matches('/').to_string();
    };
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let (authority, suffix) = remainder.split_at(authority_end);
    // Keep invalid userinfo byte-for-byte distinct until validation rejects
    // it; lowercasing credentials would merge two invalid configured routes.
    let authority = if authority.contains('@') {
        authority.to_string()
    } else {
        authority.to_ascii_lowercase()
    };
    let suffix = if suffix.starts_with('/') {
        let path_end = suffix.find(['?', '#']).unwrap_or(suffix.len());
        let (path, query_or_fragment) = suffix.split_at(path_end);
        format!("{}{}", path.trim_end_matches('/'), query_or_fragment)
    } else {
        suffix.to_string()
    };
    format!("{}://{}{}", scheme.to_ascii_lowercase(), authority, suffix)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderEndpointError {
    UserinfoNotAllowed,
}

impl std::fmt::Display for ProviderEndpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserinfoNotAllowed => formatter.write_str(
                "LLM Provider endpoint must not contain userinfo; configure credentials separately",
            ),
        }
    }
}

impl std::error::Error for ProviderEndpointError {}
