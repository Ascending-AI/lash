//! Kernel process hosting independently of the session's protocol.

use std::sync::Arc;

use lash_core::plugin::{PluginError, PluginFactory, PluginSessionContext, SessionPlugin};
use lash_vm_client::{RunBounds, service::Service};

/// Registers the kernel process engine and its workflow document provider.
/// A host installs this beside its protocol plugin; it uses the same backend
/// and worker library for publication and execution.
pub struct KernelProcessPluginFactory {
    workers: Service,
    bounds: RunBounds,
    functions: Option<Arc<lash_kernel_doc::FunctionRegistry>>,
    #[cfg(feature = "synthetic-next")]
    writes: Option<lash_kernel_doc::KernelVersion>,
    adopting_helpers: bool,
}

impl KernelProcessPluginFactory {
    pub fn new(workers: Service, bounds: RunBounds) -> Self {
        Self {
            workers,
            bounds,
            functions: None,
            #[cfg(feature = "synthetic-next")]
            writes: None,
            adopting_helpers: false,
        }
    }

    /// The library functions the host's custom workers hold.
    #[must_use]
    pub fn with_functions(mut self, functions: Arc<lash_kernel_doc::FunctionRegistry>) -> Self {
        self.functions = Some(functions);
        self
    }

    /// Adopt processes off the earlier helper releases this build retains.
    #[must_use]
    pub fn adopting_helpers(mut self) -> Self {
        self.adopting_helpers = true;
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

    fn process_engine_contributions(
        &self,
        ctx: &lash_core::plugin::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, PluginError> {
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
                Arc::clone(functions),
                self.workers.clone(),
                bounds,
            ),
            None => crate::KernelProcessEngine::new(documents, self.workers.clone(), bounds)
                .map_err(|error| PluginError::Registration(error.to_string()))?,
        }
        .with_trace_runtime(ctx.trace_runtime().clone());
        #[cfg(feature = "synthetic-next")]
        let engine = match self.writes {
            Some(writes) => engine.writing(writes),
            None => engine,
        };
        let engine = if self.adopting_helpers {
            engine.adopting_helpers(
                crate::retained_earlier_helpers()
                    .map_err(|error| PluginError::Registration(error.to_string()))?,
            )
        } else {
            engine
        };
        Ok(vec![crate::kernel_process_engine_registration(engine)])
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(KernelProcessPlugin))
    }
}

impl lash_core::plugin::PluginDefinition for KernelProcessPluginFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("lash-kernel")
    }
}

struct KernelProcessPlugin;

impl SessionPlugin for KernelProcessPlugin {
    fn id(&self) -> &'static str {
        "lash-kernel"
    }

    fn register(&self, _reg: &mut lash_core::plugin::PluginRegistrar) -> Result<(), PluginError> {
        Ok(())
    }
}
