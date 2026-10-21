//! Trusted package advertisement, publication, starts and deferred discovery.
//! Package visibility controls Imports; only explicit tool registration exposes
//! model tools. Resolution outcomes are committed before target execution.
use super::{OwnerConfigView, PluginRegistrar};
use crate::deferred_provider::DeferredResolveContext;
use lash_core_store::artifact_referrer::ReferrerClaim;
use lash_core_store::{process_env::ProcessEnv, tool_surface::OperationRoute};
use lash_execution_contracts::{
    PluginError,
    process::{EngineSettings, linking::*},
};
use lash_vocabulary::{
    ToolDefinition, identity::ProcessId, names::*, package::*, providers::ToolProviderKey,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Package data plus route candidates. Package membership never automatically enters the Tool Catalog.
pub struct PackageContribution {
    pub advertisement: PackageAdvertisement,
    pub routes: Vec<AdvertisedRoute>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// One qualified operation's definition/route candidate. Selection also keys by contract, provider and tool ID.
pub struct AdvertisedRoute {
    pub operation: QualifiedOperation,
    pub definition: ToolDefinition,
    pub route: OperationRoute,
}
#[async_trait::async_trait]
/// Trusted package advertisement from recorded owner config; independent of model-tool registration.
pub trait PackageProvider: Send + Sync {
    fn roots(&self) -> BTreeSet<Segment>;
    fn advertise(&self, config: &OwnerConfigView) -> Result<Vec<PackageContribution>, PluginError>;
}
#[derive(Debug)]
/// Prepared namespace and code. Publication validates ownership, Exports, Brands and full provenance holds.
pub struct PublishPackage {
    pub package: Package,
    pub code: Vec<Fulfilled>,
}
#[async_trait::async_trait]
/// Trusted immutable publication with complete content/provenance retention before references are exposed.
pub trait PackagePublisher: Send + Sync {
    async fn publish(
        &self,
        claim: &ReferrerClaim,
        context: &PublicationContext,
        request: PublishPackage,
    ) -> Result<PublicationRecord, EnginePortError>;
}
#[derive(Debug)]
/// Trusted host-selected start, arguments, complete environment/settings and explicit initial pins.
pub struct StartLinked {
    pub start: LinkedStart,
    pub args: serde_json::Map<String, serde_json::Value>,
    pub env: ProcessEnv,
    pub settings: Option<EngineSettings>,
    pub initial_resolutions: Vec<lash_core_store::resolutions::ResolutionSeed>,
}
#[async_trait::async_trait]
/// Host starts after linked reach, routes, hosting, arguments and initial pins validate.
pub trait LinkedProcessStarts: Send + Sync {
    async fn start(&self, request: StartLinked) -> Result<ProcessId, EnginePortError>;
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Discovery outcome. Only validated recorded winners execute; a miss/refusal is pinned before guest delivery.
pub enum DeferredAnswer {
    Resolved {
        definition: Box<ToolDefinition>,
        route: Box<OperationRoute>,
    },
    Refused {
        note: String,
    },
    Unavailable {
        note: Option<String>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
/// Retryable infrastructure discovery failure under the grant's resolution policy; no negative resolution is stored.
pub struct ResolverFault {
    pub message: String,
}
#[async_trait::async_trait]
/// Read-only repeatable discovery over pinned grant scope/config, never a live-session fallback.
pub trait DeferredPackageProvider: Send + Sync {
    async fn resolve(
        &self,
        cx: &DeferredResolveContext<'_>,
        grant: &DeferredGrant,
        requested: &QualifiedName,
    ) -> Result<DeferredAnswer, ResolverFault>;
}

impl std::fmt::Display for ResolverFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ResolverFault {}
/// Registration helper over the existing process component registry.
/// Storage and construction belong to pkg-host.
pub struct ProcessComponentRegistrar {
    _private: (),
}
impl<T, R> PluginRegistrar<T, R> {
    pub fn package_provider(
        &mut self,
        _key: ToolProviderKey,
        _provider: Arc<dyn PackageProvider>,
    ) -> Result<(), PluginError> {
        // pkg-host owns registration storage and validation.
        todo!("PkgHost (FIG-7107): package provider registration")
    }
    pub fn deferred_package_provider(
        &mut self,
        _key: ToolProviderKey,
        _provider: Arc<dyn DeferredPackageProvider>,
    ) -> Result<(), PluginError> {
        // pkg-host owns registration storage and validation.
        todo!("PkgHost (FIG-7107): deferred package provider registration")
    }
}
impl ProcessComponentRegistrar {
    pub fn deferred_package_provider(
        &mut self,
        _key: ToolProviderKey,
        _provider: Arc<dyn DeferredPackageProvider>,
    ) -> Result<(), PluginError> {
        // pkg-host owns the existing process component registry adapter.
        todo!("PkgHost (FIG-7107): process deferred provider registration")
    }
}
