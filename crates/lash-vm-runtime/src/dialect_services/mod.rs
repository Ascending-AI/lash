//! Ordinary code-only service packages. The same registration serves sessions
//! and sessionless processes; no VM or broker recognizes these names.
use lash_core_execution::{
    PendingCompletion, ToolAttemptOutcome, ToolCall, ToolFailure, ToolOutcomeDone, ToolProvider,
    plugin::{
        OwnerConfigView,
        packages::{AdvertisedRoute, PackageContribution, PackageProvider, operation_publication},
    },
    tool_reads::{ReadCall, ReadError, ToolProviderRegistration, ToolReads},
};
use lash_core_store::tool_surface::{OperationRoute, RouteSource};
use lash_execution_contracts::{PluginError, process::linking::EnginePortError};
use lash_vocabulary::{
    ExecutionPolicy, ParkBound, ToolContract, ToolDeclaration, ToolDefinition, ToolManifest,
    TurnControls, names::*, package::*, providers::ToolProviderHandle,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    TypeScript,
    Python,
}
impl Dialect {
    pub fn package(self) -> &'static str {
        match self {
            Self::TypeScript => "typescript",
            Self::Python => "python",
        }
    }
}
/// Host-selected bounds and randomness. Reads use the activation's injected
/// clock, so no session object or global wall clock is needed by a process.
pub struct Services {
    dialect: Dialect,
    provider: ToolProviderHandle,
    definitions: BTreeMap<String, ToolDefinition>,
    random: Arc<dyn Fn() -> u64 + Send + Sync>,
}
impl Services {
    pub fn new(
        dialect: Dialect,
        provider: ToolProviderHandle,
        execution: Duration,
        random: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Arc<Self>, PluginError> {
        let mut definitions = BTreeMap::new();
        for member in ["clock", "random", "sleep"] {
            let name = format!("{}.{member}", dialect.package());
            let input = if member == "sleep" {
                serde_json::json!({"type":"object","properties":{"milliseconds":{"type":"integer","minimum":0}},"required":["milliseconds"],"additionalProperties":false})
            } else {
                serde_json::json!({"type":"object","properties":{},"additionalProperties":false})
            };
            let output = if member == "sleep" {
                serde_json::json!({"type":"null"})
            } else {
                serde_json::json!({"type":"string","x-lash":{"kind":if member=="clock" {"timestamp"} else {"bytes"}}})
            };
            let mut definition =
                ToolDefinition::raw(name.clone(), name.as_str(), "", input, output)
                    .map_err(|e| PluginError::Registration(e.to_string()))?
                    .declared_contract(
                        lash_kernel_dialect::authoring::service_requirement(
                            &lash_kernel_dialect::service_name(dialect.package(), member),
                        )
                        .expect("service requirement")
                        .contract,
                    )
                    .with_execution(execution)
                    .with_execution_policy(if member == "sleep" {
                        ExecutionPolicy::Once
                    } else {
                        ExecutionPolicy::Read
                    });
            if member == "sleep" {
                definition = definition
                    .with_declaration(ToolDeclaration::deferring(), Some(ParkBound::UntilScopeEnd))
                    .map_err(|e| PluginError::Registration(e.to_string()))?;
            }
            definitions.insert(name, definition);
        }
        Ok(Arc::new(Self {
            dialect,
            provider,
            definitions,
            random,
        }))
    }
    pub fn registration(self: &Arc<Self>) -> ToolProviderRegistration {
        ToolProviderRegistration::new(self.clone()).with_reads(self.clone())
    }
}
#[async_trait::async_trait]
impl PackageProvider for Services {
    fn roots(&self) -> BTreeSet<Segment> {
        BTreeSet::from([Segment::new(self.dialect.package()).expect("canonical")])
    }
    fn advertise(
        &self,
        _config: &OwnerConfigView,
        context: &PublicationContext,
    ) -> Result<Vec<PackageContribution>, PluginError> {
        let path = PackagePath::new(Segment::new(self.dialect.package()).expect("canonical"));
        let mut members = BTreeMap::new();
        let mut routes = Vec::new();
        for (name, definition) in &self.definitions {
            let member = name.rsplit('.').next().expect("qualified");
            let mode = if member == "sleep" {
                OperationMode::Waited
            } else {
                OperationMode::Read
            };
            let offer = OperationOffer {
                requirement: lash_kernel_dialect::authoring::service_requirement(
                    &lash_kernel_dialect::service_name(self.dialect.package(), member),
                )
                .expect("service requirement"),
                intents: BTreeSet::new(),
                controls: TurnControls::none(),
                mode,
            };
            members.insert(
                Segment::new(member).expect("canonical"),
                Member {
                    visibility: Visibility::Public,
                    kind: MemberKind::Operation(offer),
                },
            );
            routes.push(AdvertisedRoute {
                operation: QualifiedOperation {
                    package: path.clone(),
                    name: Segment::new(member).expect("canonical"),
                },
                definition: definition.clone(),
                route: OperationRoute {
                    tool: definition.manifest.id.clone(),
                    provider: self.provider.clone(),
                    policy: definition.manifest.execution_policy.clone(),
                    projection: definition.manifest.argument_projection.clone(),
                    payload: None,
                    source: RouteSource::Advertised,
                },
            });
        }
        let advertisement = PackageAdvertisement::Enumerated(Package { path, members });
        let publication = operation_publication(context, &advertisement)
            .map_err(|error| PluginError::Package(Box::new(error)))?;
        Ok(vec![PackageContribution {
            publication,
            advertisement,
            routes,
        }])
    }
    async fn hold(
        &self,
        _claim: &lash_core_store::artifact_referrer::ReferrerClaim,
        _advertisements: &[&PackageAdvertisement],
    ) -> Result<(), EnginePortError> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl ToolProvider for Services {
    fn tool_manifests(&self, _: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
        Ok(Vec::new())
    }
    fn resolve_manifest_by_id(
        &self,
        id: &lash_vocabulary::ToolId,
        _: &OwnerConfigView,
    ) -> Option<ToolManifest> {
        self.definitions
            .values()
            .find(|d| d.manifest.id == *id)
            .map(|d| d.manifest.clone())
    }
    fn resolve_manifest(&self, name: &str, _: &OwnerConfigView) -> Option<ToolManifest> {
        self.definitions.get(name).map(|d| d.manifest.clone())
    }
    fn resolve_contract(&self, name: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        self.definitions
            .get(name)
            .map(|d| Arc::new(d.contract.clone()))
    }
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        let failure = |message: &str| {
            ToolAttemptOutcome::done_without_intents(ToolOutcomeDone::failure(ToolFailure::new(
                "type_error",
                message,
            )))
        };
        if call.name() != format!("{}.sleep", self.dialect.package()) {
            return failure("a read operation requires a reader");
        }
        let Some(milliseconds) = call
            .args
            .get("milliseconds")
            .and_then(serde_json::Value::as_u64)
        else {
            return failure("sleep takes a nonnegative integer number of milliseconds");
        };
        if let Err(error) = call.context.completion_key() {
            return failure(&error.to_string());
        }
        let now = call.context.admission_now();
        let deadline = lash_durable::DurableInstant(
            now.0
                .saturating_add(i64::try_from(milliseconds).unwrap_or(i64::MAX)),
        );
        ToolAttemptOutcome::pending(
            PendingCompletion::new().resolved_by_timer(deadline, serde_json::Value::Null),
        )
    }
}
#[async_trait::async_trait]
impl ToolReads for Services {
    async fn read(&self, call: ReadCall<'_>) -> Result<serde_json::Value, ReadError> {
        match call.manifest.tool().rsplit('.').next() {
            Some("clock") => {
                let instant = call.clock.timestamp_datetime();
                Ok(serde_json::Value::String(
                    (i128::from(instant.timestamp()) * 1_000_000_000
                        + i128::from(instant.timestamp_subsec_nanos()))
                    .to_string(),
                ))
            }
            Some("random") => {
                let bytes = (self.random)().to_be_bytes();
                Ok(serde_json::Value::String(
                    bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
                ))
            }
            _ => Err(ReadError::new("unknown dialect read")),
        }
    }
}

#[cfg(test)]
mod tests;
