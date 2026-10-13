//! Kernel process hosting independently of the session's protocol.

use std::sync::Arc;

use lash_core::plugin::{PluginError, PluginFactory, SessionPlugin};
use lash_vm_client::{RunBounds, service::Service};

/// Registers the kernel process engine and its workflow document provider.
/// A host installs this beside its protocol plugin; it uses the same backend
/// and worker library for publication and execution.
pub struct KernelProcessPluginFactory {
    workers: Service,
    dialect: String,
    bounds: RunBounds,
    functions: Option<Arc<lash_kernel_doc::FunctionRegistry>>,
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
            functions: None,
            #[cfg(feature = "synthetic-next")]
            writes: None,
        }
    }

    /// The library functions the host's custom workers hold.
    #[must_use]
    pub fn with_functions(mut self, functions: Arc<lash_kernel_doc::FunctionRegistry>) -> Self {
        self.functions = Some(functions);
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
    ) -> std::result::Result<lash_core::plugin::PluginRecipe, lash_core::plugin::PluginError> {
        let recipe_factory = Arc::clone(&self);
        let recipe: std::result::Result<
            lash_core::plugin::PluginRecipe,
            lash_core::plugin::PluginError,
        > = {
            let mut reg = lash_core::plugin::PluginRegistrar::<KernelProcessPlugin>::new(
                lash_core::plugin::PluginFactory::id(self.as_ref()),
            );
            KernelProcessPlugin::declare(&mut reg)?;
            let id = lash_core::plugin::PluginFactory::id(self.as_ref());
            Ok(lash_core::plugin::PluginRecipe::new(
                id,
                reg,
                move |_ctx, _state| Ok(Arc::new(KernelProcessPlugin)),
            ))
        };
        let mut recipe = recipe?;
        let engine_factory = Arc::clone(&recipe_factory);
        recipe = recipe.with_process_engine(crate::LASH_VM_ENGINE_KIND, move |context| {
            engine_factory.bind_process_engine(context)
        });
        Ok(recipe)
    }
}
impl KernelProcessPluginFactory {
    pub fn bind_process_engine(
        &self,
        ctx: &lash_core::plugin::ProcessEngineContributionContext<'_>,
    ) -> Result<lash_core::ProcessEngineRegistration, PluginError> {
        let documents = crate::KernelDocuments::new(ctx.backend().module_artifacts());
        let bounds = lash_kernel_vm::Bounds {
            charge: self.bounds.charge,
            memory: self.bounds.memory,
            call_depth: self.bounds.call_depth,
            live_tasks: self.bounds.live_tasks,
            requests_per_park: self.bounds.requests_per_park,
            join_members: self.bounds.join_members,
        };
        let engine = match &self.functions {
            Some(functions) => crate::KernelProcessEngine::with_functions(
                documents,
                &self.dialect,
                Arc::clone(functions),
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
        }
        .with_trace_runtime(ctx.trace_runtime().clone());
        #[cfg(feature = "synthetic-next")]
        let engine = match self.writes {
            Some(writes) => engine.writing(writes),
            None => engine,
        };
        Ok(crate::kernel_process_engine_registration(engine))
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

struct KernelProcessPlugin;

impl SessionPlugin for KernelProcessPlugin {
    fn id(&self) -> &'static str {
        "lash-kernel"
    }
}
impl KernelProcessPlugin {
    fn declare(
        _reg: &mut lash_core::plugin::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash_core::plugin::PluginError> {
        Ok(())
    }
}
