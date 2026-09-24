//! The service catalog: what the endpoint serves, read from its own discovery
//! document the way `restate-server` reads it at deployment registration.

use std::collections::BTreeMap;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use restate_sdk::endpoint::Endpoint;
use serde::Deserialize;

/// The Restate service kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ServiceKind {
    Service,
    VirtualObject,
    Workflow,
}

/// How a handler is scheduled against its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerKind {
    /// A plain service handler: no key, no state, no lock.
    Service,
    /// A virtual-object handler that holds its key's lock and may write state.
    Exclusive,
    /// A handler that runs beside the lock and reads a state snapshot.
    Shared,
    /// A workflow's `run` handler: once per key, holding the key's lock.
    WorkflowRun,
}

impl HandlerKind {
    pub fn is_keyed(self) -> bool {
        !matches!(self, Self::Service)
    }

    /// Whether an invocation of this handler queues behind its key's lock.
    pub fn takes_lock(self) -> bool {
        matches!(self, Self::Exclusive | Self::WorkflowRun)
    }
}

/// A handler's retry settings as the deployment declares them (discovery
/// manifest v4). Unset fields fall back to the service's, then to the
/// server's default policy.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RetryOverrides {
    pub initial_interval_ms: Option<u64>,
    pub max_interval_ms: Option<u64>,
    pub max_attempts: Option<u32>,
    pub exponentiation_factor: Option<f64>,
    pub on_max_attempts: Option<OnMaxAttempts>,
}

impl RetryOverrides {
    fn or(self, fallback: Self) -> Self {
        Self {
            initial_interval_ms: self.initial_interval_ms.or(fallback.initial_interval_ms),
            max_interval_ms: self.max_interval_ms.or(fallback.max_interval_ms),
            max_attempts: self.max_attempts.or(fallback.max_attempts),
            exponentiation_factor: self
                .exponentiation_factor
                .or(fallback.exponentiation_factor),
            on_max_attempts: self.on_max_attempts.or(fallback.on_max_attempts),
        }
    }
}

/// What the invoker does once a handler exhausts its attempts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OnMaxAttempts {
    /// Stop retrying and park the invocation until an operator resumes it.
    Pause,
    /// Fail the invocation terminally.
    Kill,
}

/// One served handler.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HandlerSpec {
    pub service_kind: ServiceKind,
    pub kind: HandlerKind,
    pub retry: RetryOverrides,
    /// The deployment's inactivity timeout for this handler, if it set one.
    pub inactivity_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ServiceEntry {
    pub kind: ServiceKind,
    pub handlers: BTreeMap<String, HandlerSpec>,
}

/// Every service the endpoint binds, by name.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    services: BTreeMap<String, ServiceEntry>,
}

/// Why a target is not served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unresolved {
    Service(String),
    Handler { service: String, handler: String },
}

impl Catalog {
    pub fn service(&self, name: &str) -> Option<&ServiceEntry> {
        self.services.get(name)
    }

    pub fn resolve(&self, service: &str, handler: &str) -> Result<HandlerSpec, Unresolved> {
        let entry = self
            .services
            .get(service)
            .ok_or_else(|| Unresolved::Service(service.to_owned()))?;
        entry
            .handlers
            .get(handler)
            .copied()
            .ok_or_else(|| Unresolved::Handler {
                service: service.to_owned(),
                handler: handler.to_owned(),
            })
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.services.keys().map(String::as_str)
    }

    /// Read the catalog from `endpoint`'s discovery document.
    pub async fn discover(endpoint: &Endpoint) -> Result<Self, String> {
        let request = http::Request::builder()
            .uri("/discover")
            .header("accept", "application/vnd.restate.endpointmanifest.v4+json")
            .body(Full::new(Bytes::new()))
            .map_err(|error| format!("discovery request: {error}"))?;
        let response = endpoint.handle(request);
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|error| format!("discovery body: {error}"))?
            .to_bytes();
        if !status.is_success() {
            return Err(format!(
                "discovery answered {status}: {}",
                String::from_utf8_lossy(&body)
            ));
        }
        Self::from_manifest(&body)
    }

    fn from_manifest(body: &[u8]) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Manifest {
            #[serde(default)]
            services: Vec<Service>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Service {
            name: String,
            ty: ServiceKind,
            #[serde(default)]
            handlers: Vec<Handler>,
            #[serde(flatten)]
            retry: Retry,
            #[serde(default)]
            inactivity_timeout: Option<u64>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Handler {
            name: String,
            #[serde(default)]
            ty: Option<String>,
            #[serde(flatten)]
            retry: Retry,
            #[serde(default)]
            inactivity_timeout: Option<u64>,
        }
        #[derive(Default, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Retry {
            #[serde(default)]
            retry_policy_initial_interval: Option<u64>,
            #[serde(default)]
            retry_policy_max_interval: Option<u64>,
            #[serde(default)]
            retry_policy_max_attempts: Option<u32>,
            #[serde(default)]
            retry_policy_exponentiation_factor: Option<f64>,
            #[serde(default)]
            retry_policy_on_max_attempts: Option<OnMaxAttempts>,
        }
        impl Retry {
            fn overrides(&self) -> RetryOverrides {
                RetryOverrides {
                    initial_interval_ms: self.retry_policy_initial_interval,
                    max_interval_ms: self.retry_policy_max_interval,
                    max_attempts: self.retry_policy_max_attempts,
                    exponentiation_factor: self.retry_policy_exponentiation_factor,
                    on_max_attempts: self.retry_policy_on_max_attempts,
                }
            }
        }
        let manifest: Manifest =
            serde_json::from_slice(body).map_err(|error| format!("discovery document: {error}"))?;
        let mut services = BTreeMap::new();
        for service in manifest.services {
            let service_retry = service.retry.overrides();
            let mut handlers = BTreeMap::new();
            for handler in service.handlers {
                let kind = match (service.ty, handler.ty.as_deref()) {
                    (ServiceKind::Service, _) => HandlerKind::Service,
                    (_, Some("SHARED")) => HandlerKind::Shared,
                    (ServiceKind::Workflow, None | Some("WORKFLOW")) => HandlerKind::WorkflowRun,
                    (ServiceKind::VirtualObject, None | Some("EXCLUSIVE")) => {
                        HandlerKind::Exclusive
                    }
                    (kind, Some(other)) => {
                        return Err(format!(
                            "handler {}/{} has type {other} on a {kind:?}",
                            service.name, handler.name
                        ));
                    }
                };
                handlers.insert(
                    handler.name,
                    HandlerSpec {
                        service_kind: service.ty,
                        kind,
                        retry: handler.retry.overrides().or(service_retry),
                        inactivity_timeout_ms: handler
                            .inactivity_timeout
                            .or(service.inactivity_timeout),
                    },
                );
            }
            services.insert(
                service.name,
                ServiceEntry {
                    kind: service.ty,
                    handlers,
                },
            );
        }
        Ok(Self { services })
    }
}
