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
/// Immutable ordinary service declarations. Their identity is independent of routes and host bounds.
pub static DIALECT_SERVICE_PACKAGES: std::sync::LazyLock<[Package; 2]> =
    std::sync::LazyLock::new(|| {
        [Dialect::TypeScript, Dialect::Python].map(|dialect| Package {
            path: PackagePath::new(
                Segment::new(dialect.package())
                    .unwrap_or_else(|error| panic!("declared dialect package: {error}")),
            ),
            members: ["clock", "random", "sleep"]
                .into_iter()
                .map(|member| {
                    (
                        Segment::new(member)
                            .unwrap_or_else(|error| panic!("declared dialect service: {error}")),
                        Member {
                            visibility: Visibility::Public,
                            kind: MemberKind::Operation(OperationOffer {
                                requirement: lash_kernel_dialect::authoring::service_requirement(
                                    &lash_kernel_dialect::service_name(dialect.package(), member),
                                )
                                .unwrap_or_else(|| {
                                    panic!("declared dialect service requirement is absent")
                                }),
                                intents: BTreeSet::new(),
                                controls: TurnControls::none(),
                                mode: if member == "sleep" {
                                    OperationMode::Waited
                                } else {
                                    OperationMode::Read
                                },
                            }),
                        },
                    )
                })
                .collect(),
        })
    });
/// Well-known timer symbols for display. Consumers compare the exact pinned Package and member.
pub static TIMER_OPERATIONS: std::sync::LazyLock<[(PackageId, Segment); 2]> =
    std::sync::LazyLock::new(|| {
        DIALECT_SERVICE_PACKAGES.each_ref().map(|package| {
            (
                package.identity(),
                Segment::new("sleep")
                    .unwrap_or_else(|error| panic!("declared sleep member: {error}")),
            )
        })
    });
impl Dialect {
    fn declaration(self) -> &'static Package {
        &DIALECT_SERVICE_PACKAGES[match self {
            Self::TypeScript => 0,
            Self::Python => 1,
        }]
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
        for member in dialect.declaration().members.keys().map(Segment::as_str) {
            let name = format!("{}.{member}", dialect.package());
            let input = if member == "sleep" {
                serde_json::json!({"type":"integer","minimum":0})
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
                        .unwrap_or_else(|| panic!("declared dialect service requirement is absent"))
                        .contract,
                    )
                    .with_execution(execution)
                    .with_execution_policy(if member == "sleep" {
                        // Timer admission only records a deadline from the original
                        // admitted instant. Retrying an uncommitted admission is safe.
                        ExecutionPolicy::repeatable(
                            std::num::NonZeroU32::MIN.saturating_add(2),
                            0,
                            0,
                        )
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
    fn roots(&self, _: &OwnerConfigView) -> BTreeSet<Segment> {
        self.dialect
            .declaration()
            .path
            .first()
            .cloned()
            .into_iter()
            .collect()
    }
    fn advertise(
        &self,
        _config: &OwnerConfigView,
        context: &PublicationContext,
    ) -> Result<Vec<PackageContribution>, PluginError> {
        let package = self.dialect.declaration().clone();
        let mut routes = Vec::new();
        for definition in self.definitions.values() {
            routes.push(AdvertisedRoute {
                operation: definition.manifest.operation.clone(),
                definition: definition.clone(),
                route: OperationRoute {
                    tool: definition.manifest.id.clone(),
                    provider: self.provider.clone(),
                    policy: definition.manifest.execution_policy,
                    projection: definition.manifest.argument_projection.clone(),
                    payload: None,
                    source: RouteSource::Advertised,
                },
            });
        }
        let advertisement = PackageAdvertisement::Enumerated(package);
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
        let Some(milliseconds) = call.args.as_u64() else {
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
