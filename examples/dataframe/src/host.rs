//! The host owns IO, publication provenance and process route selection.
use lash::package::*;
use lash::process::{HostArtifactPin, HostArtifacts, ProcessEnv};
use std::{collections::BTreeMap, sync::Arc};

pub const IO: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("dataframe-io", lash::plugins::FormatVersion::ONE);

pub fn environment_request(
    packages: &RecordedPackages,
    context: PublicationContext,
) -> lash::vm::WorkflowEnvironmentRequest {
    lash::vm::WorkflowEnvironmentRequest {
        snapshot: packages.snapshot.clone(),
        context,
        attachment: Attachment {
            members: Default::default(),
            deferred: Default::default(),
            packages: packages
                .snapshot
                .advertisements
                .iter()
                .filter_map(|accepted| match &accepted.advertisement {
                    PackageAdvertisement::Enumerated(package) => Some(package.path.clone()),
                    _ => None,
                })
                .collect(),
        },
    }
}

pub struct WorkflowCreator {
    pub artifacts: HostArtifacts,
    pub workers: lash::vm::WorkerService,
    pub namespace: RecordedPackages,
    pub environment: ProcessEnv,
    pub context: PublicationContext,
    settings: lash::process::KernelRecordedSettings,
}
impl WorkflowCreator {
    pub async fn create(
        &self,
        source: &str,
        language: lash::workflow::document::SourceLanguage,
        entry: &str,
        pin: &HostArtifactPin,
    ) -> Result<LinkedStart, String> {
        let request = environment_request(&self.namespace, self.context.clone());
        let names = self
            .artifacts
            .workflow_environment(&request)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("workflow engine missing")?;
        let (document, map, _) = self
            .workers
            .import_workflow(language, source, names.namespaces(), None)
            .await
            .map_err(|error| error.to_string())?;
        self.artifacts
            .publish_workflow(
                pin,
                &lash::workflow::document::DocumentBytes::from(document),
                &StartSelector::Export(Name::new(entry)),
                &request,
                Some((&map, source)),
                None,
            )
            .await
            .map_err(|error| error.to_string())
    }
    pub async fn start(
        &self,
        start: LinkedStart,
        path: &str,
        key: &str,
        pin: &HostArtifactPin,
    ) -> Result<lash::persistence::ProcessStartReceipt, String> {
        let env = self
            .artifacts
            .link_process_env(
                &self.environment,
                &lash::process::LinkTarget::Linked(start.clone()),
                &self.namespace,
                &BTreeMap::new(),
            )
            .await
            .map_err(|error| error.to_string())?;
        Box::pin(
            self.artifacts.start_linked(
                pin,
                StartLinked {
                    start,
                    args: [("path".into(), serde_json::Value::String(path.into()))]
                        .into_iter()
                        .collect(),
                    env,
                    settings: Some(self.settings.clone().into_engine_settings()),
                    initial_resolutions: Vec::new(),
                },
                key,
                &environment_request(&self.namespace, self.context.clone()),
            ),
        )
        .await
        .map_err(|error| error.to_string())
    }
}

#[derive(Clone)]
struct CsvFiles(Arc<BTreeMap<String, String>>);
#[lash::async_trait]
impl lash::tools::StaticToolExecute for CsvFiles {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        match call
            .args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .and_then(|path| self.0.get(path))
        {
            Some(text) => {
                lash::tools::ToolOutcome::ok(serde_json::Value::String(text.clone())).into()
            }
            None => lash::tools::ToolOutcome::failure(lash::tools::ToolFailure::invalid_request(
                "missing_csv",
                "the host has no CSV at this path",
            ))
            .into(),
        }
    }
}

/// `pin` owns all bootstrap/config retention before this function's first
/// await. The host releases it when it retires this configured library.
pub async fn build(
    backend: lash::Backend,
    owner: lash::persistence::LeaseOwnerIdentity,
    pin: &HostArtifactPin,
    files: BTreeMap<String, String>,
) -> Result<(lash::LashCore, WorkflowCreator), String> {
    let tool = lash::tools::ToolDefinition::raw("example:read_csv", "io.read_csv", "Read one host-authorized CSV.", serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}), serde_json::json!({"type":"string"})).map_err(|error| error.to_string())?
        .with_execution(std::time::Duration::from_secs(30)).with_execution_policy(lash::tools::ExecutionPolicy::repeatable(std::num::NonZeroU32::MIN, 0, 0));
    let provider: Arc<dyn lash::tools::ToolProvider> = Arc::new(
        lash::tools::StaticToolProvider::new(vec![tool], CsvFiles(Arc::new(files))),
    );
    let package_provider = Arc::new(ToolPackageProvider::new(
        IO.component(),
        lash::tool_provider_key!("io"),
        [Segment::new("io").map_err(|error| error.to_string())?].into(),
        provider.clone(),
    ));
    let spec = lash::plugins::PluginSpec::new()
        .with_tool_provider(lash::tool_provider_key!("io"), provider.clone())
        .with_package_provider(lash::tool_provider_key!("io"), package_provider);
    let session_plugin = Arc::new(lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(IO.component()),
        spec,
    ));
    let component = Arc::new(
        session_plugin
            .as_component(IO.format())
            .map_err(|error| error.to_string())?,
    );
    let workers = lash::vm::WorkerService::default();
    let environment = ProcessEnv::builder(lash::MaxToolCalls::new(64))
        .component(IO, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()
        .map_err(|error| error.to_string())?;
    let build_core = |packages| {
        lash::LashCore::standard_builder(backend.clone())
            .packages(packages)
            .plugin(session_plugin.clone())
            .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
                workers.clone(),
                lash::vm::TYPESCRIPT_DIALECT,
                workers.config().run_bounds,
            )))
            .process_component(component.clone(), IO)
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .build(owner.clone())
            .map_err(|error| error.to_string())
    };
    let bootstrap = build_core(RecordedPackages::default())?;
    let context = PublicationContext {
        domain: PublicationDomain(backend.binding_identity().to_string()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("dataframe-library".into()),
        root: Segment::new("df").map_err(|error| error.to_string())?,
    };
    let mut namespace = bootstrap
        .host_artifacts()
        .process_namespace(pin, &environment, context.clone())
        .await
        .map_err(|error| error.to_string())?;
    let publisher = bootstrap
        .host_artifacts()
        .package_publisher(context.clone(), namespace.snapshot.clone())
        .map_err(|error| error.to_string())?;
    let (package, publication) = crate::publish_library(
        &backend,
        pin,
        &workers,
        &publisher,
        &namespace,
        context.clone(),
    )
    .await?;
    let accepted = AcceptedAdvertisement {
        publication,
        advertisement: PackageAdvertisement::Enumerated(package),
    };
    namespace.snapshot.advertisements.push(accepted.clone());
    let configured = RecordedPackages {
        consumer: None,
        snapshot: NamespaceSnapshot {
            advertisements: vec![accepted],
        },
        routes: Vec::new(),
    };
    drop(bootstrap);
    let core = build_core(configured)?;
    let settings = core
        .processes()
        .kernel_engine_defaults()
        .map_err(|error| error.to_string())?;
    let creator = WorkflowCreator {
        settings,
        artifacts: core.host_artifacts(),
        workers,
        namespace,
        environment,
        context,
    };
    Ok((core, creator))
}
