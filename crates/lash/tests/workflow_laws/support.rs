//! The fixture owns publication, explicit namespace and launch retention.
use super::*;
use lash::package::*;

#[derive(Debug)]
pub(super) struct Publication {
    pub start: LinkedStart,
    pub document: lash::workflow::document::DocumentId,
    pub definition: lash::process::ProcessDefinition,
    pub environment: lash::process::ProcessEnv,
    pub env_ref: lash::process::ProcessEnvRef,
    pub source_map: Option<lash::workflow::document::SourceMapId>,
}

pub(super) async fn namespace(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    environment: &lash::process::ProcessEnv,
) -> (RecordedPackages, lash::vm::WorkflowEnvironmentRequest) {
    let context = PublicationContext {
        domain: PublicationDomain(core.backend().binding_identity().to_string()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("workflow-laws".into()),
        root: Segment::new("workflow").unwrap(),
    };
    let packages = core
        .host_artifacts()
        .process_namespace(pin, environment, context.clone())
        .await
        .unwrap();
    let request = lash::vm::WorkflowEnvironmentRequest {
        snapshot: packages.snapshot.clone(),
        context,
        attachment: Attachment {
            packages: packages
                .snapshot
                .advertisements
                .iter()
                .filter_map(|accepted| match &accepted.advertisement {
                    PackageAdvertisement::Enumerated(package) => Some(package.path.clone()),
                    _ => None,
                })
                .collect(),
            members: Default::default(),
            deferred: Default::default(),
        },
    };
    (packages, request)
}

pub(super) async fn publish(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    document: &lash::workflow::document::DocumentBytes,
    name: &Name,
    base: &lash::process::ProcessEnv,
    source: Option<(&lash::workflow::document::SourceMap, &str)>,
) -> lash::Result<Publication> {
    let (packages, request) = namespace(core, pin, base).await;
    publish_recorded(
        core,
        pin,
        document,
        name,
        base,
        source,
        (&packages, &request),
    )
    .await
}

pub(super) async fn publish_recorded(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    document: &lash::workflow::document::DocumentBytes,
    name: &Name,
    base: &lash::process::ProcessEnv,
    source: Option<(&lash::workflow::document::SourceMap, &str)>,
    recorded: (&RecordedPackages, &lash::vm::WorkflowEnvironmentRequest),
) -> lash::Result<Publication> {
    let (packages, request) = recorded;
    let artifacts = core.host_artifacts();
    let start = artifacts
        .publish_workflow(
            pin,
            document,
            &StartSelector::Export(name.clone()),
            request,
            source,
            None,
        )
        .await?;
    let environment = artifacts
        .link_process_env(
            base,
            &lash::process::LinkTarget::Linked(start.clone()),
            packages,
            &Default::default(),
        )
        .await?;
    let env_ref = artifacts.publish_process_env(pin, &environment).await?;
    let draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
        code: lash::vm::KernelInlineRequest::Linked {
            start: start.clone(),
        },
    })
    .map_err(|error| {
        lash::EmbedError::from(lash_execution_contracts::PluginError::Invoke(
            error.to_string(),
        ))
    })?;
    let definition = artifacts.publish_definition(pin, &draft).await?;
    Ok(Publication {
        start,
        document: document.id(),
        definition,
        environment,
        env_ref,
        source_map: source.map(|(map, _)| map.identity()),
    })
}

pub(super) fn operation_import(
    slot: ImportSlot,
    definition: &lash::tools::ToolDefinition,
) -> (ImportSlot, Import) {
    (
        slot,
        Import {
            name: QualifiedName {
                package: definition.manifest.operation.package.clone(),
                member: definition.manifest.operation.name.clone(),
                type_member: None,
            },
            kind: ImportKind::Operation {
                signature: lash_vocabulary::json_schema::tool_signature(
                    &definition.contract.input_schema.canonical,
                    &definition.contract.output_schema.canonical,
                ),
            },
        },
    )
}
pub(super) fn echo_import_header(slot: u32) -> String {
    let mut document = Document::new(lash::workflow::document::NumberPolicy::Float, Vec::new());
    document.imports.insert(
        ImportSlot(slot),
        operation_import(ImportSlot(slot), &echo_definitions()[0]).1,
    );
    lash_kernel_doc::print_document(&document)
        .lines()
        .filter(|line| line.starts_with("import "))
        .collect::<Vec<_>>()
        .join("\n")
}
pub(super) fn function_document(
    definition: lash::workflow::document::FunctionDefinition,
    name: &str,
) -> Document {
    let mut document = Document::new(lash::workflow::document::NumberPolicy::Float, Vec::new());
    document.imports = definition.body().unwrap().imports.clone();
    document.exports.insert(
        Name::new(name),
        lash::workflow::document::Export::Function(lash::workflow::document::FunctionExport {
            function: definition.identity(),
            signature: definition.signature.clone(),
            convention: lash::workflow::document::StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    document.functions.insert(definition.identity(), definition);
    document
}
pub(super) fn export_closure(
    document: &mut Document,
    name: &str,
    params: &[&str],
    result: lash::schema::Type,
) {
    let Stmt::Let {
        value: Rhs::Expr(Expr::Closure(closure)),
        ..
    } = &document.main.as_ref().unwrap()[0]
    else {
        panic!("authored closure")
    };
    document.exports.insert(
        Name::new(name),
        lash::workflow::document::Export::Function(lash::workflow::document::FunctionExport {
            function: closure.function,
            signature: lash::schema::Signature {
                params: params
                    .iter()
                    .map(|name| lash::schema::Param {
                        name: Name::new(*name),
                        ty: lash::schema::Type::Json,
                        optional: false,
                    })
                    .collect(),
                result,
            },
            convention: lash::workflow::document::StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
}

pub(super) const WORKFLOW_SERVICES: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("workflow-services", lash::plugins::FormatVersion::ONE);
pub(super) fn workflow_services() -> lash::process::StaticProcessComponent {
    let provider = lash_vm_runtime::dialect_services::Services::new(
        lash_vm_runtime::dialect_services::Dialect::TypeScript,
        WORKFLOW_SERVICES.tool_provider(lash::tool_provider_key!("timer")),
        std::time::Duration::from_secs(30),
        Arc::new(|| 0),
    )
    .unwrap();
    lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(WORKFLOW_SERVICES.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider_registration(
                lash::tool_provider_key!("timer"),
                provider.registration(),
            )
            .with_package_provider(lash::tool_provider_key!("timer"), provider),
    )
    .as_component(WORKFLOW_SERVICES.format())
    .unwrap()
}
pub(super) fn source_environment(process_tools: bool) -> lash::process::ProcessEnv {
    let mut builder = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(ECHO_TOOLS, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &());
    if process_tools {
        builder = builder.component(lash::process_controls::PROCESS_CONTROLS_CONFIG, &());
    }
    builder.build().unwrap()
}
