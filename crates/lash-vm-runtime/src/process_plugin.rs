//! Kernel process hosting independently of the session's protocol.

use std::sync::Arc;

use lash_core::plugin::PluginFactory;
use lash_execution_contracts::plugin_error::PluginError;
use lash_vm_client::{RunBounds, service::Service};

/// Registers the kernel process engine and its workflow document provider.
/// A host installs this beside its protocol plugin; it uses the same backend
/// and worker library for publication and execution.
pub struct KernelProcessPluginFactory {
    workers: Service,
    dialect: String,
    bounds: RunBounds,
    /// Content preparation survives per-activation engine construction; it
    /// carries no backend, execution context, recorded settings or authority.
    preparations: crate::process::ProcessPreparations,
    #[cfg(feature = "synthetic-next")]
    writes: Option<lash_kernel_doc::KernelVersion>,
}

impl KernelProcessPluginFactory {
    /// The engine for processes whose documents are written in `dialect`,
    /// run on `workers` and held to `bounds`. The host names the dialect:
    /// each process records it at creation, and a worker whose embedding
    /// lacks it refuses the process's runs (FIG-5990).
    pub fn new(workers: Service, dialect: impl Into<String>, bounds: RunBounds) -> Self {
        Self {
            workers,
            dialect: dialect.into(),
            bounds,
            preparations: crate::process::ProcessPreparations::default(),
            #[cfg(feature = "synthetic-next")]
            writes: None,
        }
    }

    /// The native library shared by worker pools, selected
    /// on this factory's service before any engine shares its pool.
    #[must_use]
    pub fn with_library(mut self, packages: Arc<lash_vm_client::packages::WorkerLibrary>) -> Self {
        self.workers = self.workers.with_library(packages);
        self
    }

    /// The kernel version the previous build writes, for the two-build laws.
    #[cfg(feature = "synthetic-next")]
    #[must_use]
    pub fn writing_kernel(mut self, writes: lash_kernel_doc::KernelVersion) -> Self {
        self.writes = Some(writes);
        self
    }
}

impl PluginFactory for KernelProcessPluginFactory {
    fn id(&self) -> &'static str {
        "lash-kernel"
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let id = self.id();
        Ok(lash_core::plugin::PluginRecipe::from_registrar(
            id,
            lash_core::plugin::PluginRegistrar::new(id),
        )
        .with_process_engine(crate::LASH_VM_ENGINE_KIND, move |context| {
            self.bind_process_engine(context)
        }))
    }
}
impl KernelProcessPluginFactory {
    pub fn bind_process_engine(
        &self,
        ctx: &lash_core::plugin::ProcessEngineContributionContext<'_>,
    ) -> Result<lash_core::ProcessEngineRegistration, PluginError> {
        let mut engine = self
            .engine(
                ctx.backend().module_artifacts(),
                ctx.backend().package_store(),
            )?
            .with_trace_runtime(ctx.trace_runtime().clone());
        engine.carry_stores = Some(ctx.backend().stores());
        #[cfg(feature = "synthetic-next")]
        let engine = match self.writes {
            Some(writes) => engine.writing(writes),
            None => engine,
        };
        Ok(crate::kernel_process_engine_registration(engine))
    }
    /// Bind the current artifact ports to this hosting factory's one pure
    /// document preparation owner. No store or referrer authority is shared.
    pub fn documents(
        &self,
        store: Arc<dyn lash_execution_contracts::module_artifacts::ModuleArtifactStore>,
        packages: Arc<dyn lash_durable::packages::PackageStore>,
    ) -> crate::KernelDocuments {
        let mut documents = crate::KernelDocuments::new(store, packages);
        documents.prepared_from(&self.preparations);
        documents
    }

    /// Constructs one runtime binding without retaining its backend authority.
    /// The contribution boundary supplies trace and format policy separately.
    pub(crate) fn engine(
        &self,
        store: Arc<dyn lash_execution_contracts::module_artifacts::ModuleArtifactStore>,
        packages: Arc<dyn lash_durable::packages::PackageStore>,
    ) -> Result<crate::KernelProcessEngine, PluginError> {
        let documents = self.documents(store, packages);
        let bounds = lash_kernel_vm::Bounds {
            memory_meter: match self.bounds.memory_meter {
                lash_vm_protocol::MemoryMeter::On => lash_kernel_vm::MemoryMeter::On,
                lash_vm_protocol::MemoryMeter::Off => lash_kernel_vm::MemoryMeter::Off,
            },
            charge: match self.bounds.charge {
                lash_vm_client::Charge::Exact => lash_kernel_vm::Charge::Exact,
                lash_vm_client::Charge::Off => lash_kernel_vm::Charge::Off,
            },
            charge_limit: self.bounds.charge_limit,
            memory: self.bounds.memory,
            call_depth: self.bounds.call_depth,
            live_tasks: self.bounds.live_tasks,
            requests_per_park: self.bounds.requests_per_park,
            join_members: self.bounds.join_members,
            // A process is saved at every park and resumed on any worker.
        };
        let engine =
            crate::KernelProcessEngine::new(documents, &self.dialect, self.workers.clone(), bounds)
                .map_err(|error| PluginError::Registration(error.to_string()))?;
        Ok(engine)
    }
}

impl KernelProcessPluginFactory {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("lash-kernel")
    }
}
impl lash_core::plugin::PluginMetadata for KernelProcessPluginFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

/// Explicit session/process hosting for the ordinary dialect operations.
/// The provider advertises packages and keeps its model manifest list empty.
pub struct KernelServicesPluginFactory {
    documents: crate::KernelDocuments,
}
impl KernelServicesPluginFactory {
    pub fn new(documents: crate::KernelDocuments) -> Self {
        Self { documents }
    }
}
pub const KERNEL_SERVICES_OWNER: &str = "lash-kernel-services";
const HELPERS_PROVIDER: lash_core::plugin::ToolProviderKey =
    lash_core::tool_provider_key!("helpers");

/// Whether trusted publication provenance belongs to the compiler helpers.
/// Projects the registration owner/key without a second package inventory.
pub fn is_helper_publication(context: &lash_vocabulary::package::PublicationContext) -> bool {
    context.publisher.0 == format!("{}/{}", KERNEL_SERVICES_OWNER, HELPERS_PROVIDER)
}
pub const KERNEL_SERVICES_CONFIG: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new(KERNEL_SERVICES_OWNER, lash_core::FormatVersion::ONE);
pub const DIALECT_SERVICE_EXECUTION: std::time::Duration = std::time::Duration::from_secs(30);

struct HelperPackages {
    release: Arc<lash_vm_releases::HelperRelease>,
    documents: crate::KernelDocuments,
}
#[async_trait::async_trait]
impl lash_core::plugin::packages::PackageProvider for HelperPackages {
    async fn hold(
        &self,
        claim: &lash_core::ReferrerClaim,
        advertisements: &[&lash_vocabulary::package::PackageAdvertisement],
    ) -> Result<(), lash_execution_contracts::process::linking::EnginePortError> {
        for advertisement in advertisements {
            let lash_vocabulary::package::PackageAdvertisement::Enumerated(package) =
                *advertisement
            else {
                return Err(
                    lash_execution_contracts::process::linking::EnginePortError::Unsupported {
                        operation: "helper release has no deferred packages".into(),
                    },
                );
            };
            if self.release.content.packages.get(&package.identity()) != Some(package) {
                return Err(
                    lash_execution_contracts::process::linking::EnginePortError::Refused(
                        lash_execution_contracts::process::linking::AdmissionRefusal::NotOwner {
                            path: package.path.clone(),
                        },
                    ),
                );
            }
        }
        self.documents
            .hold_helper_release(claim, &self.release.content, &self.release.labels)
            .await
    }
    fn roots(&self) -> std::collections::BTreeSet<lash_vocabulary::Segment> {
        self.release
            .content
            .packages
            .values()
            .map(|package| package.path.as_slice()[0].clone())
            .collect()
    }
    fn advertise(
        &self,
        _: &lash_core::OwnerConfigView,
        context: &lash_vocabulary::package::PublicationContext,
    ) -> Result<Vec<lash_core::plugin::packages::PackageContribution>, PluginError> {
        Ok(self
            .release
            .content
            .packages
            .values()
            .filter(|package| package.path.as_slice()[0] == context.root)
            .cloned()
            .map(|package| {
                let code_links = package
                    .members
                    .values()
                    .filter_map(|member| match &member.kind {
                        lash_vocabulary::package::MemberKind::Export { link, .. } => Some(*link),
                        _ => None,
                    })
                    .collect();
                lash_core::plugin::packages::PackageContribution {
                    publication: lash_vocabulary::package::PublicationRecord {
                        context: context.clone(),
                        path: package.path.clone(),
                        content: lash_vocabulary::package::PublishedContent::Package(
                            package.identity(),
                        ),
                        code_links,
                        imported_publications: Vec::new(),
                    },
                    advertisement: lash_vocabulary::PackageAdvertisement::Enumerated(package),
                    routes: Vec::new(),
                }
            })
            .collect())
    }
}

fn register_services<T: Send + Sync + 'static, R>(
    reg: &mut lash_core::plugin::PluginRegistrar<T, R>,
    documents: crate::KernelDocuments,
) -> Result<(), PluginError>
where
    R: lash_core::plugin::registries::Registry
        + lash_core::plugin::registries::Admits<
            lash_core::plugin::registries::contribution::PackageProvider,
        > + lash_core::plugin::registries::Admits<
            lash_core::plugin::registries::contribution::ToolProvider,
        > + lash_core::plugin::registries::Admits<
            lash_core::plugin::registries::contribution::ToolReads,
        >,
{
    use crate::dialect_services::{Dialect, Services};
    use lash_core::plugin::ToolProviderHandle;
    let owner = reg
        .declared_owner()
        .ok_or_else(|| PluginError::Registration("dialect services require an owner".into()))?
        .to_owned();
    let dialects: &[(Dialect, lash_core::plugin::ToolProviderKey)] = &[
        #[cfg(feature = "dialect-typescript")]
        (
            Dialect::TypeScript,
            lash_core::tool_provider_key!("typescript"),
        ),
        #[cfg(feature = "dialect-python")]
        (Dialect::Python, lash_core::tool_provider_key!("python")),
    ];
    for (dialect, key) in dialects {
        let services = Services::new(
            *dialect,
            ToolProviderHandle::new(R::KIND, owner.clone(), *key),
            DIALECT_SERVICE_EXECUTION,
            Arc::new(|| fastrand::u64(..)),
        )?;
        reg.package_provider(*key, services.clone())?;
        reg.tools()
            .provider_with_reads(*key, move |_, _| Ok(services.registration()))?;
    }
    let release = lash_vm_releases::helper_release()
        .map_err(|error| PluginError::Registration(error.to_string()))?;
    reg.package_provider(
        HELPERS_PROVIDER,
        Arc::new(HelperPackages { release, documents }),
    )?;
    Ok(())
}

impl lash_core::plugin::PluginMetadata for KernelServicesPluginFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(KERNEL_SERVICES_OWNER)
    }
}
impl PluginFactory for KernelServicesPluginFactory {
    fn id(&self) -> &'static str {
        KERNEL_SERVICES_OWNER
    }
    fn recipe(self: Arc<Self>) -> Result<lash_core::plugin::PluginRecipe, PluginError> {
        let mut reg = lash_core::plugin::PluginRegistrar::new(KERNEL_SERVICES_OWNER);
        register_services(&mut reg, self.documents.clone())?;
        Ok(lash_core::plugin::PluginRecipe::from_registrar(
            KERNEL_SERVICES_OWNER,
            reg,
        ))
    }
}

pub struct KernelServicesComponent {
    documents: crate::KernelDocuments,
}
impl KernelServicesComponent {
    pub fn new(documents: crate::KernelDocuments) -> Self {
        Self { documents }
    }
}
impl lash_core::plugin::ProcessComponentFactory for KernelServicesComponent {
    fn declaration(&self) -> lash_core::plugin::ProcessComponentDeclaration {
        lash_core::plugin::ProcessComponentDeclaration::initial(
            KERNEL_SERVICES_OWNER,
            lash_core::FormatVersion::ONE,
        )
        .unwrap_or_else(|_| unreachable!("static services component declaration"))
    }
    fn config_type(&self) -> std::any::TypeId {
        std::any::TypeId::of::<()>()
    }
    fn validate(
        &self,
        entry: &lash_core::ProcessComponentEntry,
        _: &lash_core::ProcessEnv,
    ) -> Result<(), lash_core::ProcessEnvRefusal> {
        entry.decode(KERNEL_SERVICES_CONFIG).map_err(|error| {
            lash_core::ProcessEnvRefusal::ConfigRefused {
                id: KERNEL_SERVICES_OWNER.into(),
                reason: error.to_string(),
            }
        })
    }
    fn recipe(self: Arc<Self>) -> Result<lash_core::plugin::ProcessRecipe, PluginError> {
        let mut reg = lash_core::plugin::PluginRegistrar::for_process(KERNEL_SERVICES_OWNER);
        register_services(&mut reg, self.documents.clone())?;
        Ok(lash_core::plugin::ProcessRecipe::new(
            KERNEL_SERVICES_OWNER,
            reg,
            |_| Ok(Arc::new(())),
        ))
    }
}
