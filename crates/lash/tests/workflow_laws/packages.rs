//! A host package in a durable workflow (FIG-6988): the `dataframe`
//! fixture installed on the process factory beside lash's packages, its
//! operation served by a fake runner under the package's declared
//! contract.

use super::*;

/// The component that contributes the fake `dataframe` runner.
const DATAFRAME_TOOLS: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("workflow-dataframe", lash::plugins::FormatVersion::ONE);

/// The fake runner's `dataframe.run`: its input is the fixture's plan, the
/// data fields as plain schema and the step's callback as a function slot,
/// and it declares the package's contract.
fn run_definition() -> lash::tools::ToolDefinition {
    lash::tools::ToolDefinition::raw(
        "tool:dataframe_run",
        "dataframe.run",
        "Runs a plan.",
        serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "step": { "type": "string" },
                    "exprs": { "type": "array" },
                    "callback": { "x-lash": { "kind": "function" } },
                    "n": { "type": "integer" }
                },
                "required": ["step", "exprs"],
                "additionalProperties": false
            }
        }),
        serde_json::json!({ "type": "array" }),
    )
    .expect("the runner's schemas")
    .declared_contract(lash_kernel_value::operations::ContractId::new(
        "dataframe.run/1",
    ))
    .with_execution(std::time::Duration::from_secs(120))
}

/// The plans the fake runner ran, in order.
static PLAN_LAW: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static PLANS: std::sync::Mutex<Vec<serde_json::Value>> = std::sync::Mutex::new(Vec::new());

/// Answers two rows for any plan.
struct Runner;

#[lash::async_trait]
impl lash::tools::StaticToolExecute for Runner {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        PLANS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call.args.clone());
        lash::tools::ToolOutcome::ok(serde_json::json!([["a", 2], ["b", 1]])).into()
    }
}

fn runner_component() -> lash::process::StaticProcessComponent {
    lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(DATAFRAME_TOOLS.component()),
        lash::plugins::PluginSpec::new()
            .with_package_provider(
                lash::tool_provider_key!("dataframe"),
                Arc::new(lash::package::ToolPackageProvider::new(
                    DATAFRAME_TOOLS.component(),
                    lash::tool_provider_key!("dataframe"),
                    [lash::package::Segment::new("dataframe").unwrap()].into(),
                    Arc::new(lash::tools::StaticToolProvider::new(
                        vec![run_definition()],
                        Runner,
                    )),
                )),
            )
            .with_tool_provider(
                lash::tool_provider_key!("dataframe"),
                Arc::new(lash::tools::StaticToolProvider::new(
                    vec![run_definition()],
                    Runner,
                )),
            ),
    )
    .as_component(DATAFRAME_TOOLS.format())
    .expect("the runner component")
}

/// One explicitly published code Package beside an operation Package. The
/// fixture's snapshot owns attachment; each code Import retains its own Link.
struct Packages {
    core: lash::LashCore,
    pin: lash::process::HostArtifactPin,
    base: lash::process::ProcessEnv,
    packages: lash::package::RecordedPackages,
    namespace: lash::vm::WorkflowEnvironmentRequest,
}
impl Packages {
    async fn new(stores: Arc<lash_sqlite_store::SqliteStoreSet>, other: i64, boot: &str) -> Self {
        let core = builder_with_process_tools(stores, false, EchoComponent::Installed)
            .await
            .process_component(Arc::new(runner_component()), DATAFRAME_TOOLS)
            .build(node_identity(boot))
            .unwrap();
        let pin = lash::process::HostArtifactPin::mint();
        let base = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
            .component(DATAFRAME_TOOLS, &())
            .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
            .build()
            .unwrap();
        let (packages, namespace) = namespace(&core, &pin, &base).await;
        let mut fixture = Self {
            core,
            pin,
            base,
            packages,
            namespace,
        };
        let (slot, import) = operation_import(lash::package::ImportSlot(0), &run_definition());
        let mut header = Document::new(lash::workflow::document::NumberPolicy::Float, Vec::new());
        header.imports.insert(slot, import);
        let imports = lash_kernel_doc::print_document(&header)
            .lines()
            .filter(|line| line.starts_with("import "))
            .collect::<Vec<_>>()
            .join("\n");
        let collect = lash_kernel_doc::parse_definition(&format!("function collect() -> Json\nkernel 1\nmetering statements\n{imports}\nbody {{ let rows = perform 0([{{step: \"limit\",exprs: [],n: 2}}]) as Any return rows }}")).unwrap();
        fixture
            .code_package("dataframe", function_document(collect, "collect"))
            .await;
        let answer = lash_kernel_doc::parse_definition(&format!(
            "function answer() -> Int\nkernel 1\nmetering statements\nbody {{return {other}}}"
        ))
        .unwrap();
        fixture
            .code_package("other", function_document(answer, "answer"))
            .await;
        fixture
    }
    async fn code_package(&mut self, path: &str, document: Document) {
        use lash::package::PackagePublisher as _;
        use lash::package::*;
        let artifacts = self.core.host_artifacts();
        let documents = lash::vm::KernelDocuments::new(
            self.core.backend().module_artifacts(),
            self.core.backend().package_store(),
            std::sync::Arc::clone(lash::vm::WorkerLibrary::built().unwrap().helpers()),
        );
        let claim = lash::persistence::ReferrerClaim::unguarded(
            lash::persistence::ArtifactReferrer::HostPin(self.pin.clone()),
        )
        .unwrap();
        let id = documents
            .admit_document(&claim, &document.clone().into())
            .await
            .unwrap();
        let context = self
            .packages
            .snapshot
            .advertisements
            .iter()
            .find(|accepted| accepted.publication.path.to_string() == path)
            .map(|accepted| accepted.publication.context.clone())
            .unwrap_or_else(|| PublicationContext {
                root: Segment::new(path).unwrap(),
                ..self.namespace.context.clone()
            });
        let linked = documents
            .fulfil(&lash::package::FulfilRequest {
                document: id,
                selector: lash::package::ReachSelector::AllExports,
                snapshot: self.namespace.snapshot.clone(),
                context: context.clone(),
                purpose: lash::package::LinkPurpose::PackageCode {
                    path: PackagePath::parse(path).unwrap(),
                },
            })
            .await
            .unwrap();
        let mut members = self
            .packages
            .snapshot
            .advertisements
            .iter()
            .find_map(|accepted| match &accepted.advertisement {
                PackageAdvertisement::Enumerated(package) if package.path.to_string() == path => {
                    Some(package.members.clone())
                }
                _ => None,
            })
            .unwrap_or_default();
        members.extend(document.exports.keys().map(|name| {
            (
                Segment::new(name.as_str()).unwrap(),
                Member {
                    visibility: Visibility::Public,
                    kind: MemberKind::Export {
                        link: linked.content.identity(),
                        export: ExportName {
                            name: name.clone(),
                            member: None,
                        },
                    },
                },
            )
        }));
        let package = Package {
            path: PackagePath::parse(path).unwrap(),
            members,
        };
        let publication = artifacts
            .package_publisher(context.clone(), self.namespace.snapshot.clone())
            .unwrap()
            .publish(
                &claim,
                &context,
                lash::package::PublishPackage {
                    package: package.clone(),
                    code: vec![linked],
                },
            )
            .await
            .unwrap();
        self.packages.snapshot.advertisements.retain(|accepted| !matches!(&accepted.advertisement,PackageAdvertisement::Enumerated(old) if old.path == package.path));
        self.packages
            .snapshot
            .advertisements
            .push(AcceptedAdvertisement {
                publication,
                advertisement: PackageAdvertisement::Enumerated(package.clone()),
            });
        self.namespace.snapshot = self.packages.snapshot.clone();
        self.namespace.attachment.packages.insert(package.path);
    }
    async fn import(
        &self,
        source: &str,
        namespace: &lash::vm::WorkflowEnvironmentRequest,
    ) -> lash::workflow::WorkflowImport {
        self.core
            .host_artifacts()
            .import_workflow(
                lash::workflow::document::SourceLanguage::TypeScript,
                source,
                namespace,
                None,
            )
            .await
            .unwrap()
    }
    async fn publish(
        &self,
        document: &lash::workflow::document::DocumentBytes,
        name: &str,
    ) -> (
        lash::package::LinkedStart,
        lash::process::ProcessEnvRef,
        lash::process::ProcessDefinition,
    ) {
        let artifacts = self.core.host_artifacts();
        let start = artifacts
            .publish_workflow(
                &self.pin,
                document,
                &lash::package::StartSelector::Export(Name::new(name)),
                &self.namespace,
                None,
                None,
            )
            .await
            .unwrap();
        let env = artifacts
            .link_process_env(
                &self.base,
                &lash::process::LinkTarget::Linked(start.clone()),
                &self.packages,
                &Default::default(),
            )
            .await
            .unwrap();
        let env = artifacts
            .publish_process_env(&self.pin, &env)
            .await
            .unwrap();
        let draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
            code: lash::vm::KernelInlineRequest::Linked {
                start: start.clone(),
            },
        })
        .unwrap();
        let definition = artifacts
            .publish_definition(&self.pin, &draft)
            .await
            .unwrap();
        (start, env, definition)
    }
}

/// L-SUPERSET/L-DISPATCH: direct and imported-body calls keep separate uses
/// while the host's all-package attachment also retains unused other code.
#[tokio::test(flavor = "multi_thread")]
async fn a_direct_perform_and_a_member_body_state_the_packages_one_requirement() {
    let _guard = PLAN_LAW.lock().await;
    PLANS.lock().unwrap().clear();
    let fixture = Packages::new(
        Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap()),
        7,
        "direct-member",
    )
    .await;
    let source = "export async function rows(): Promise<(string|number)[][][]> { const direct = await dataframe.run([{step: \"sort\",exprs: []}]); const via = dataframe.collect(); return [direct,via]; }";
    let lash::workflow::WorkflowImport::Imported { document, .. } =
        fixture.import(source, &fixture.namespace).await
    else {
        panic!("module imports")
    };
    let (start, env, definition) = fixture.publish(&document, "rows").await;
    let loaded = fixture
        .core
        .host_artifacts()
        .execution_document(&start)
        .await
        .unwrap();
    assert!(matches!(
        loaded,
        lash::workflow::WorkflowDocumentRead::Read(_)
    ));
    let env_value = lash_execution_contracts::process::load_process_env(
        fixture.core.backend().process_env_store().as_ref(),
        &env,
    )
    .await
    .unwrap();
    assert!(
        env_value
            .execution()
            .publications
            .iter()
            .any(|publication| publication.path.to_string() == "other"),
        "all-package attachment is preserved"
    );
    let uses: Vec<_> = env_value.execution().uses.iter().collect();
    assert_eq!(
        uses.len(),
        2,
        "direct and member body retain separate operation uses"
    );
    assert_ne!(uses[0].0.link, uses[1].0.link);
    let process = start_with(
        &fixture.core,
        &env,
        &definition,
        "direct-member",
        Default::default(),
    )
    .await;
    let rows = serde_json::json!([["a", 2], ["b", 1]]);
    assert_eq!(
        finished(&fixture.core, &process).await,
        serde_json::json!([rows, rows])
    );
    assert_eq!(
        *PLANS.lock().unwrap(),
        [
            serde_json::json!([{ "step":"sort","exprs":[] }]),
            serde_json::json!([{ "step":"limit","exprs":[],"n":2 }])
        ]
    );
    fixture.core.shutdown().await.unwrap();
}

/// L-INTERNAL/L-ATTACH: an absent attachment cannot authorize direct or
/// imported code, even when the host retains its published Package.
#[tokio::test(flavor = "multi_thread")]
async fn a_workflow_refuses_each_undeclared_package_member() {
    let fixture = Packages::new(
        Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap()),
        7,
        "attachment",
    )
    .await;
    let mut narrow = fixture.namespace.clone();
    narrow
        .attachment
        .packages
        .remove(&lash::package::PackagePath::parse("other").unwrap());
    let source = "export async function rows(): Promise<number> { return other.answer(); }";
    let import = fixture.import(source, &narrow).await;
    assert!(
        matches!(import, lash::workflow::WorkflowImport::Refused(_)),
        "unattached other.answer is refused: {import:?}"
    );
    for source in [
        "export async function rows(): Promise<(string|number)[][]> { return await dataframe.run([]); }",
        "export async function rows(): Promise<(string|number)[][]> { return dataframe.collect(); }",
    ] {
        let mut narrow = fixture.namespace.clone();
        narrow
            .attachment
            .packages
            .remove(&lash::package::PackagePath::parse("dataframe").unwrap());
        let import = fixture.import(source, &narrow).await;
        assert!(
            matches!(import, lash::workflow::WorkflowImport::Refused(_)),
            "unattached dataframe member is refused: {import:?}"
        );
    }
    let lash::workflow::WorkflowImport::Imported { document, .. } =
        fixture.import(source, &fixture.namespace).await
    else {
        panic!("attached code imports")
    };
    let (_, env, definition) = fixture.publish(&document, "rows").await;
    let process = start_with(
        &fixture.core,
        &env,
        &definition,
        "attached",
        Default::default(),
    )
    .await;
    assert_eq!(
        finished(&fixture.core, &process).await,
        serde_json::json!(7)
    );
    fixture.core.shutdown().await.unwrap();
}

/// L-PIN: changing only an unrelated Package alters a fresh snapshot but
/// leaves the original Link and its cold continuation unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn an_undeclared_package_release_leaves_a_published_workflow_and_its_resume_unchanged() {
    let clock = Arc::new(lash_core::testing::TestClock::new(1_700_000_000_000));
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .unwrap(),
    );
    let first = Packages::new(stores.clone(), 7, "before-other").await;
    let source = "export async function rows(): Promise<(string|number)[][]> { await sleep(60000); return dataframe.collect(); }";
    let lash::workflow::WorkflowImport::Imported { document, .. } =
        first.import(source, &first.namespace).await
    else {
        panic!("sleeping module imports")
    };
    let (start, env, definition) = first.publish(&document, "rows").await;
    let process = start_with(
        &first.core,
        &env,
        &definition,
        "before-change",
        Default::default(),
    )
    .await;
    let due = timer_park_after(&first.core, &process, 0).await;
    first.core.shutdown().await.unwrap();
    let second = Packages::new(stores, 99, "after-other").await;
    let publication = |fixture: &Packages, path: &str| {
        fixture
            .packages
            .snapshot
            .advertisements
            .iter()
            .find(|accepted| accepted.publication.path.to_string() == path)
            .unwrap()
            .publication
            .content
            .clone()
    };
    assert_ne!(publication(&first, "other"), publication(&second, "other"));
    assert_eq!(
        publication(&first, "dataframe"),
        publication(&second, "dataframe")
    );
    let lash::workflow::WorkflowDocumentRead::Read(read) = second
        .core
        .host_artifacts()
        .execution_document(&start)
        .await
        .unwrap()
    else {
        panic!("original Link retained")
    };
    assert_eq!(read.identity(), document.id());
    clock.set(due);
    assert_eq!(
        finished(&second.core, &process).await,
        serde_json::json!([["a", 2], ["b", 1]])
    );
    let again = start_with(
        &second.core,
        &env,
        &definition,
        "after-change",
        Default::default(),
    )
    .await;
    let due = timer_park_after(&second.core, &again, due).await;
    clock.set(due);
    assert_eq!(
        finished(&second.core, &again).await,
        serde_json::json!([["a", 2], ["b", 1]])
    );
    second.core.shutdown().await.unwrap();
}
