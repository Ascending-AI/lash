//! Cenote R14 cells must publish through the real frozen-callable boundary.
use super::*;

const R14_TOOLS: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("cenote-r14-printer", lash::plugins::FormatVersion::ONE);

struct CaptureRun(Arc<std::sync::Mutex<Vec<serde_json::Value>>>);

#[lash::async_trait]
impl lash::tools::StaticToolExecute for CaptureRun {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        assert!(
            call.args.get("run").is_some(),
            "only the outer creation cell runs"
        );
        self.0.lock().unwrap().push(call.args.clone());
        lash::tools::ToolOutcome::ok(serde_json::json!({"ok":true,"data":{"id":"saved"}})).into()
    }
}

async fn prints(cell: &str) {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let definitions = [
        ("cenote.read_document", serde_json::json!({"type":"object"})),
        ("cenote.discuss", serde_json::json!({"type":"object"})),
        (
            "workflows.create",
            serde_json::json!({
                "type":"object",
                "properties": {
                    "name":{"type":"string"},
                    "description":{"type":"string"},
                    "space_id":{"type":"string"},
                    "run":{"x-lash":{"kind":"function"}}
                },
                "required":["name","run"],
                "additionalProperties":false
            }),
        ),
    ]
    .into_iter()
    .map(|(name, input)| {
        lash::tools::ToolDefinition::raw(
            name,
            name,
            "R14 publication fixture",
            input,
            lash::tools::ToolDefinition::any_output(),
        )
        .unwrap()
        .with_execution(std::time::Duration::from_secs(120))
    })
    .collect();
    let provider = Arc::new(lash::tools::StaticToolProvider::new(
        definitions,
        CaptureRun(captured.clone()),
    ));
    let component = lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(R14_TOOLS.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider(lash::tool_provider_key!("r14"), provider.clone())
            .with_package_provider(
                lash::tool_provider_key!("r14"),
                Arc::new(lash::package::ToolPackageProvider::new(
                    R14_TOOLS.component(),
                    lash::tool_provider_key!("r14"),
                    ["cenote", "workflows"]
                        .into_iter()
                        .map(|name| lash::package::Segment::new(name).unwrap())
                        .collect(),
                    provider,
                )),
            ),
    )
    .as_component(R14_TOOLS.format())
    .unwrap();
    let stores = Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let core = builder_with_process_tools(stores, false, EchoComponent::Installed)
        .await
        .process_component(Arc::new(component), R14_TOOLS)
        .build(node_identity("r14-print"))
        .unwrap();
    let base = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(R14_TOOLS, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()
        .unwrap();
    let pin = lash::process::HostArtifactPin::mint();
    let (core, pin, base, packages, request) =
        super::super::packages::capturing_views_on(core, pin, base).await;
    let source = format!(
        r#"
const r={{data:{{document:{{doc_id:'01900000-0000-7000-8000-000000000001'}}}}}};
const rooms={{data:{{rooms:[{{name:'Signup counts',direct:false,space_id:'01900000-0000-7000-8000-000000000002'}}]}}}};
{cell}
"#
    );
    let format = SourceFormat::TypeScriptV1;
    let checked = core
        .host_artifacts()
        .workflow_environment(&request)
        .await
        .unwrap()
        .unwrap();
    let release = lash_kernel_releases::helper_release().unwrap();
    let roots = release.resolver_roots();
    let library =
        lash::dialect::NamedLibrary::resolving(&release.functions, |id| roots.contains(id))
            .unwrap();
    let bound = lash::dialect::Environment::new(&library, checked.namespaces());
    let lowered = lash::typescript::lower(
        &source,
        &bound,
        lash::dialect::Mode::Cell(lash::dialect::CellEnvironment::first()),
    )
    .unwrap();
    let start = core
        .host_artifacts()
        .publish_workflow(
            &pin,
            &lowered.document.clone().into(),
            &lash::package::StartSelector::Main,
            &request,
            None,
            None,
        )
        .await
        .unwrap();
    let environment = core
        .host_artifacts()
        .link_process_env(
            &base,
            &lash::process::LinkTarget::Linked(start.clone()),
            &packages,
            &Default::default(),
        )
        .await
        .unwrap();
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&pin, &environment)
        .await
        .unwrap();
    let draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
        code: lash::vm::KernelInlineRequest::Linked { start },
    })
    .unwrap();
    let definition = core
        .host_artifacts()
        .publish_definition(&pin, &draft)
        .await
        .unwrap();
    let run = start_with(
        &core,
        &env_ref,
        &definition,
        "r14-capture",
        Default::default(),
    )
    .await;
    finished(&core, &run).await;
    let shipped = captured.lock().unwrap().last().unwrap()["run"].clone();
    let function = FunctionValue::from_transport_json(
        &shipped,
        &lash::workflow::document::FunctionRegistry::default(),
        Default::default(),
        lash_kernel_value::data_limits::KernelDataLimits::standard(),
    )
    .unwrap();
    let origin = lash::package::CallableOrigin {
        authored: environment.execution().authored.clone(),
        publications: environment
            .execution()
            .publications
            .iter()
            .map(lash::package::PublicationRecord::key)
            .collect(),
    };
    let definition = core
        .host_artifacts()
        .publish_callable(&pin, &function, &request, &origin, None)
        .await;
    let definition = match definition {
        Ok(definition) => definition,
        Err(error) => {
            core.shutdown().await.unwrap();
            panic!("R14 publication: {error:?}");
        }
    };
    let printed = core
        .host_artifacts()
        .print_workflow(&definition.id, None, format, Default::default())
        .await
        .unwrap();
    let opened = core
        .host_artifacts()
        .open_workflow(&definition.id, None)
        .await
        .unwrap();
    let (read, _) = lash::vm::WorkerService::default()
        .read_exported(
            &printed.source,
            opened.draft.document(),
            format,
            Default::default(),
            None,
        )
        .await
        .unwrap();
    eprintln!("R14 PRINTED:\n{}", printed.source);
    assert!(
        read.canonical_bytes() == opened.draft.document().canonical_bytes(),
        "R14 canonical readback"
    );
    publish_source(&core, &pin, &printed.source, format, &request, "run").await;
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn numeric_label_publishes_and_reads_back() {
    prints(include_str!("r14/numeric_label.ts")).await;
}

#[tokio::test]
async fn method_argument_publishes_and_reads_back() {
    prints(include_str!("r14/method_argument.ts")).await;
}

#[tokio::test]
async fn attempt_1_publishes_and_reads_back() {
    prints(include_str!("r14/attempt_1.ts")).await;
}

#[tokio::test]
async fn attempt_2_publishes_and_reads_back() {
    prints(include_str!("r14/attempt_2.ts")).await;
}

#[tokio::test]
async fn attempt_3_publishes_and_reads_back() {
    prints(include_str!("r14/attempt_3.ts")).await;
}
