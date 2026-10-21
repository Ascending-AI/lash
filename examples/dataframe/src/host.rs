//! Host choice: derive attachment from typed requirements before publication.
use lash::package::{CellPackages, QualifiedOperation};
use lash::process::{HostArtifactPin, HostArtifacts, ProcessEnv};
use lash::vm::{SurfaceRef, WorkerService};
use lash::workflow::document::{DocumentBytes, SourceLanguage};
use lash::workflow::{WorkflowPublication, WorkflowPublish};
use std::collections::BTreeSet;

pub struct WorkflowCreator {
    pub artifacts: HostArtifacts,
    pub workers: WorkerService,
    pub packages: CellPackages,
    pub environment: ProcessEnv,
}

impl WorkflowCreator {
    pub async fn create(
        &self,
        source: &str,
        language: SourceLanguage,
        entry: &str,
        pin: &HostArtifactPin,
    ) -> Result<WorkflowPublication, String> {
        let environment = self
            .artifacts
            .workflow_environment(&self.environment)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("no workflow engine")?;
        let (document, map, _) = self
            .workers
            .import_workflow(
                language,
                source,
                environment.operations(),
                SurfaceRef::of(self.packages.digest(), self.packages.stored_wire()),
                self.packages.surface().selection(),
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        let required =
            lash::workflow::graph::requirements(&document, self.packages.catalog().as_ref());
        let operations: BTreeSet<_> = required
            .effects
            .iter()
            .chain(required.performs.keys())
            .filter_map(|effect| QualifiedOperation::from_effect(effect).ok())
            .collect();
        let declared = self
            .packages
            .surface()
            .packages()
            .filter_map(|package| {
                let names: BTreeSet<_> = operations
                    .iter()
                    .filter(|op| &op.package == package.path())
                    .map(|op| op.name.clone())
                    .collect();
                (package.ids().any(|id| required.functions.contains_key(&id)) || !names.is_empty())
                    .then(|| (package.path().clone(), names))
            })
            .collect();
        match self
            .artifacts
            .publish_workflow(
                pin,
                &DocumentBytes::from(document),
                &lash::workflow::document::Name::new(entry),
                &self.environment,
                self.packages.declare(declared),
                Some((&map, source)),
            )
            .await
            .map_err(|e| e.to_string())?
        {
            WorkflowPublish::Published(published) => Ok(*published),
            other => Err(format!("{other:?}")),
        }
    }
}

pub const DATAFRAME_TOOLS: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("dataframe-host", lash::plugins::FormatVersion::ONE);

/// The provider's process component and links are host configuration, chosen
/// before publication. The same package holdings go to the workers and engine.
pub fn build(
    backend: lash::Backend,
    owner: lash::persistence::LeaseOwnerIdentity,
) -> Result<(lash::LashCore, WorkflowCreator), String> {
    use std::sync::Arc;
    let packages = crate::packages()?;
    let workers = WorkerService::default().with_packages(packages.clone());
    let component = lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(DATAFRAME_TOOLS.component()),
        lash::plugins::PluginSpec::new().with_tool_provider(
            lash::tool_provider_key!("runner"),
            Arc::new(crate::runner::Runner),
        ),
    )
    .as_component(DATAFRAME_TOOLS.format())
    .map_err(|e| e.to_string())?;
    let provider = DATAFRAME_TOOLS.tool_provider(lash::tool_provider_key!("runner"));
    let environment = crate::runner::definitions()
        .map_err(|e| e.to_string())?
        .iter()
        .fold(
            ProcessEnv::builder(lash::MaxToolCalls::new(64)).component(DATAFRAME_TOOLS, &()),
            |env, tool| env.link_tool(provider.clone(), tool),
        )
        .build()
        .map_err(|e| e.to_string())?;
    let core = lash::LashCore::standard_builder(backend)
        .plugin(Arc::new(
            lash::vm::KernelProcessPluginFactory::new(
                workers.clone(),
                lash::vm::TYPESCRIPT_DIALECT,
                workers.config().run_bounds,
            )
            .with_packages(packages.clone()),
        ))
        .process_component(Arc::new(component), DATAFRAME_TOOLS)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(owner)
        .map_err(|e| e.to_string())?;
    let creator = WorkflowCreator {
        artifacts: core.host_artifacts(),
        workers,
        packages: CellPackages::held(&packages),
        environment,
    };
    Ok((core, creator))
}
