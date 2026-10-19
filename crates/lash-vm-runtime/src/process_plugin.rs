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
    packages: Option<Arc<lash_kernel_package::HeldPackages>>,
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
            packages: None,
            preparations: crate::process::ProcessPreparations::default(),
            #[cfg(feature = "synthetic-next")]
            writes: None,
        }
    }

    /// The package holdings shared by admission and worker pools.
    #[must_use]
    pub fn with_packages(mut self, packages: Arc<lash_kernel_package::HeldPackages>) -> Self {
        self.packages = Some(packages);
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
        let engine = self
            .engine(ctx.backend().module_artifacts())?
            .with_trace_runtime(ctx.trace_runtime().clone());
        #[cfg(feature = "synthetic-next")]
        let engine = match self.writes {
            Some(writes) => engine.writing(writes),
            None => engine,
        };
        Ok(crate::kernel_process_engine_registration(engine))
    }
    /// Constructs one runtime binding without retaining its backend authority.
    /// The contribution boundary supplies trace and format policy separately.
    pub(crate) fn engine(
        &self,
        store: Arc<dyn lash_execution_contracts::module_artifacts::ModuleArtifactStore>,
    ) -> Result<crate::KernelProcessEngine, PluginError> {
        let documents = crate::KernelDocuments::new(store);
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
        let engine = match &self.packages {
            Some(packages) => crate::KernelProcessEngine::with_packages(
                documents,
                &self.dialect,
                Arc::clone(packages),
                self.workers.clone(),
                bounds,
            ),
            None => crate::KernelProcessEngine::new(
                documents,
                &self.dialect,
                self.workers.clone(),
                bounds,
            )
            .map_err(|error| PluginError::Registration(error.to_string()))?,
        };
        Ok(self.preparations.bind(engine))
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
