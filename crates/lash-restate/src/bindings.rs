//! Wiring-time binding validation for Restate endpoints.
//!
//! One responsibility: ask a built [`Endpoint`] which services it bound — the
//! same discovery document it serves the Restate runtime — and check that
//! answer against the service names a Lash deployment requires. A host that
//! forgets a `.bind(..)` learns it here, at startup, instead of at the first
//! call that 404s into a terminal (`FIG-1579` made that runtime backstop
//! typed; this is the wiring-time check in front of it).
//!
//! The endpoint itself is the source of truth — there is no lash-side mirror
//! of registrations to drift. One caveat the substrate imposes: an endpoint
//! configured with request-identity keys answers `/discover` only for signed
//! requests, so introspecting it from inside the process reports
//! [`RestateBindingCheckError::Discovery`] rather than a service list.

use std::collections::BTreeSet;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use restate_sdk::endpoint::Endpoint;

/// The endpoint's own discovery document could not be produced or decoded.
///
/// The discovery request runs through [`Endpoint::handle`], so a rejection the
/// endpoint would give the Restate runtime — most commonly a request-identity
/// challenge on an endpoint configured with `identity_key` — lands here as the
/// refusal reason rather than as a guessed binding set.
#[derive(Debug)]
pub struct RestateEndpointDiscoveryError(String);

impl RestateEndpointDiscoveryError {
    fn new(detail: String) -> Self {
        Self(detail)
    }
}

impl std::fmt::Display for RestateEndpointDiscoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "endpoint discovery failed: {}", self.0)
    }
}

impl std::error::Error for RestateEndpointDiscoveryError {}

/// Why [`assert_services_bound`] refused an endpoint.
#[derive(Debug)]
pub enum RestateBindingCheckError {
    /// The endpoint would not report what it bound. Nothing about the required
    /// set was verified — treat this as "check inconclusive", not "bound".
    Discovery(RestateEndpointDiscoveryError),
    /// The endpoint answered discovery, and these required names are not in
    /// the answer. Sorted, deduplicated, exactly the missing names.
    UnboundServices(Vec<String>),
}

impl std::fmt::Display for RestateBindingCheckError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discovery(error) => write!(formatter, "{error}"),
            Self::UnboundServices(missing) => write!(
                formatter,
                "endpoint does not bind required services: {}",
                missing.join(", ")
            ),
        }
    }
}

impl From<RestateEndpointDiscoveryError> for RestateBindingCheckError {
    fn from(error: RestateEndpointDiscoveryError) -> Self {
        Self::Discovery(error)
    }
}

impl std::error::Error for RestateBindingCheckError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Discovery(error) => Some(error),
            Self::UnboundServices(_) => None,
        }
    }
}

/// The service names `endpoint` reports bound in its own discovery document.
///
/// This is the endpoint's word, not a lash-side record of what was passed to
/// `.bind(..)`: names arrive through the same discovery response the Restate
/// runtime reads at deployment registration.
pub async fn bound_service_names(
    endpoint: &Endpoint,
) -> Result<BTreeSet<String>, RestateEndpointDiscoveryError> {
    let request = http::Request::builder()
        .uri("/discover")
        .body(Full::new(Bytes::new()))
        .map_err(|error| {
            RestateEndpointDiscoveryError::new(format!(
                "could not build the discovery request: {error}"
            ))
        })?;
    let response = endpoint.handle(request);
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|error| {
            RestateEndpointDiscoveryError::new(format!(
                "could not read discovery response body: {error}"
            ))
        })?
        .to_bytes();
    if !status.is_success() {
        return Err(RestateEndpointDiscoveryError::new(format!(
            "endpoint answered discovery with {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    #[derive(serde::Deserialize)]
    struct DiscoveryDocument {
        #[serde(default)]
        services: Vec<DiscoveryService>,
    }
    #[derive(serde::Deserialize)]
    struct DiscoveryService {
        name: String,
    }
    let document: DiscoveryDocument = serde_json::from_slice(&body).map_err(|error| {
        RestateEndpointDiscoveryError::new(format!("could not decode discovery document: {error}"))
    })?;
    Ok(document
        .services
        .into_iter()
        .map(|service| service.name)
        .collect())
}

/// Assert that `endpoint` binds every name in `required`.
///
/// `required` is the caller's contract, not something the endpoint infers —
/// Lash deployment wiring exposes its required surface through
/// `required_service_names()` (for example
/// [`crate::RestateProcessDeployment::required_service_names`]), and a host
/// may append names of its own services it wants the same check to cover.
pub async fn assert_services_bound(
    endpoint: &Endpoint,
    required: &[&str],
) -> Result<(), RestateBindingCheckError> {
    let bound = bound_service_names(endpoint).await?;
    let mut missing: Vec<String> = required
        .iter()
        .filter(|name| !bound.contains(**name))
        .map(|name| (*name).to_string())
        .collect();
    missing.sort();
    missing.dedup();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(RestateBindingCheckError::UnboundServices(missing))
    }
}
