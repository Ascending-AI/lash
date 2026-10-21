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
    .declared_contract(lash_core::ContractId::new("dataframe.run/1"))
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
        lash::plugins::PluginSpec::new().with_tool_provider(
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

/// A node over SQLite whose kernel processes hold lash's packages and the
/// `dataframe` fixture, on a single-binary worker.
async fn dataframe_core() -> (lash::LashCore, lash::package::CellPackages) {
    let stores = Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    dataframe_core_on(stores, 7, "dataframe").await
}

async fn dataframe_core_on(
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    other: i64,
    boot: &str,
) -> (lash::LashCore, lash::package::CellPackages) {
    let lash = lash_vm_releases::built().expect("lash's packages");
    let kernel = lash
        .iter()
        .find(|package| package.path().to_string() == "kernel")
        .expect("the kernel package");
    let dataframe = lash::package::testing::dataframe(kernel).expect("the fixture builds");
    let other = lash::package::Package::build(
        "other",
        "1",
        &lash::package::PackageSources {
            kernel: &format!(
                "function other.answer() -> Int\nkernel 1\ncharge 1\nbody {{\n return {other}\n}}\n"
            ),
            surface: "function answer() -> Int = other.answer",
            labels: "",
        },
        &[kernel],
    )
    .expect("the second fixture builds");
    let held = Arc::new(
        lash::package::HeldPackages::install(lash.iter().cloned().chain([dataframe, other]))
            .expect("one registration"),
    );
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    let workers = lash::vm::WorkerService::default();
    let bounds = workers.config().run_bounds;
    let selected = lash::package::CellPackages::held(&held);
    let core = lash::LashCore::standard_builder(lash_conformance::backend_over(stores))
        .plugin(Arc::new(
            lash::vm::KernelProcessPluginFactory::new(
                workers,
                lash::vm::TYPESCRIPT_DIALECT,
                bounds,
            )
            .with_packages(held),
        ))
        .process_component(Arc::new(runner_component()), DATAFRAME_TOOLS)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(node_identity(boot))
        .expect("the core builds");
    (core, selected)
}

/// A workflow entry that performs the package's public `dataframe.run`
/// directly and through `collectRows()`'s body, each on a plan that holds
/// no package value.
const ROWS: &str = r#"export async function rows(text: string) {
  const direct = await dataframe.run([{ step: "sort", exprs: [] }]);
  const via = await dataframe.DataQuanta.readText(text).limit(2).collectRows();
  return [direct, via];
}
"#;

/// OP-ONE-REQUIREMENT (FIG-6988): the runner's tool states its own
/// signature, and the workflow is offered `dataframe.run` under the
/// package's declared requirement. So its direct perform and the member
/// body's state that one requirement: the module is admitted and
/// published, and its process runs both performs through the runner.
#[tokio::test(flavor = "multi_thread")]
async fn a_direct_perform_and_a_member_body_state_the_packages_one_requirement() {
    let _plan_law = PLAN_LAW.lock().await;
    PLANS.lock().unwrap().clear();
    let (core, selected) = dataframe_core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(DATAFRAME_TOOLS, &())
        .link_tool(
            DATAFRAME_TOOLS.tool_provider(lash::tool_provider_key!("dataframe")),
            &run_definition(),
        )
        .build()
        .expect("the runner environment");
    let imported = artifacts
        .import_workflow(
            lash::workflow::document::SourceLanguage::TypeScript,
            ROWS,
            &environment,
            selected.declare_all(),
            None,
        )
        .await
        .expect("import answers");
    let lash::workflow::WorkflowImport::Imported {
        document, packages, ..
    } = imported
    else {
        panic!("the workflow imports: {imported:?}");
    };
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &document,
                &Name::new("rows"),
                &environment,
                packages,
                None,
            )
            .await
            .expect("publish"),
    );
    let bytes = core
        .backend()
        .definition_store()
        .get_process_definition(&publication.definition.id)
        .await
        .unwrap()
        .unwrap();
    let descriptor =
        lash::process::ProcessDefinitionDraft::from_store_bytes(&publication.definition.id, &bytes)
            .unwrap();
    assert!(
        descriptor.value().as_json()["declared_packages"]
            .get("other")
            .is_some(),
        "core preserves the host's unused attachment"
    );
    let env_ref = artifacts
        .publish_process_env(&pin, &environment)
        .await
        .expect("publish the process environment");
    let mut args = serde_json::Map::new();
    args.insert("text".to_owned(), serde_json::json!("a b a"));
    let process = start_with(&core, &env_ref, &publication.definition, "rows", args).await;
    let rows = serde_json::json!([["a", 2], ["b", 1]]);
    assert_eq!(
        finished(&core, &process).await,
        serde_json::json!([rows, rows])
    );
    assert_eq!(
        *PLANS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        [
            serde_json::json!([{ "step": "sort", "exprs": [] }]),
            serde_json::json!([{ "step": "limit", "exprs": [], "n": 2 }]),
        ]
    );
    core.shutdown().await.expect("the node stops");
}

/// WF-DECLARATION: reaching a package outside the host's declaration is
/// refused before publication, with the offending member named.
#[tokio::test(flavor = "multi_thread")]
async fn a_workflow_refuses_each_undeclared_package_member() {
    let _plan_law = PLAN_LAW.lock().await;
    PLANS.lock().unwrap().clear();
    let (core, selected) = dataframe_core().await;
    let artifacts = core.host_artifacts();
    let environment = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(DATAFRAME_TOOLS, &())
        .link_tool(
            DATAFRAME_TOOLS.tool_provider(lash::tool_provider_key!("dataframe")),
            &run_definition(),
        )
        .build()
        .expect("runner environment");
    let mut declared = selected.surface().selection();
    declared.remove(&lash::package::PackagePath::parse("other").unwrap());
    let packages = selected.declare(declared);
    // The host declares dataframe, but the code also reaches other.answer.
    let source = "export async function rows() { const n = other.answer(); return await dataframe.run([]); }";
    let imported = artifacts
        .import_workflow(
            lash::workflow::document::SourceLanguage::TypeScript,
            source,
            &environment,
            packages,
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(&imported, lash::workflow::WorkflowImport::Admission(refusal)
        if refusal.code() == "undeclared_members" && refusal.to_string().contains("other.answer")),
        "undeclared other.answer must be refused: {imported:?}"
    );
    // Selecting a package's code does not attach its operations. This
    // applies to a direct perform and to one reached through a member body.
    for code in [
        "export async function rows() { return await dataframe.run([]); }",
        "export async function rows() { return await dataframe.DataQuanta.readText(\"a\").collectRows(); }",
    ] {
        let mut declared = selected.surface().selection();
        declared
            .get_mut(&lash::package::PackagePath::parse("dataframe").unwrap())
            .unwrap()
            .clear();
        let refused = artifacts
            .import_workflow(
                lash::workflow::document::SourceLanguage::TypeScript,
                code,
                &environment,
                selected.declare(declared),
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(refused, lash::workflow::WorkflowImport::Admission(lash::workflow::WorkflowAdmissionRefusal::UndeclaredMembers { members })
            if members.iter().any(|member| member == "dataframe.run")),
            "the performed operation needs its own attachment"
        );
    }
    let admitted = artifacts
        .import_workflow(
            lash::workflow::document::SourceLanguage::TypeScript,
            source,
            &environment,
            selected.declare_all(),
            None,
        )
        .await
        .unwrap();
    let lash::workflow::WorkflowImport::Imported {
        document, packages, ..
    } = admitted
    else {
        panic!("both packages admit: {admitted:?}");
    };
    let pin = lash::process::HostArtifactPin::mint();
    let refused = artifacts
        .publish_workflow(
            &pin,
            &document,
            &Name::new("rows"),
            &environment,
            selected.declare({
                let mut declaration = selected.surface().selection();
                declaration.remove(&lash::package::PackagePath::parse("other").unwrap());
                declaration
            }),
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(refused, lash::workflow::WorkflowPublish::Refused(lash::workflow::WorkflowAdmissionRefusal::UndeclaredMembers { members })
        if members.iter().any(|member| member.contains("other.answer")))
    );
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &document,
                &Name::new("rows"),
                &environment,
                packages,
                None,
            )
            .await
            .unwrap(),
    );
    let env_ref = artifacts
        .publish_process_env(&pin, &environment)
        .await
        .unwrap();
    let process = start_with(
        &core,
        &env_ref,
        &publication.definition,
        "declared-both",
        serde_json::Map::new(),
    )
    .await;
    assert_eq!(
        finished(&core, &process).await,
        serde_json::json!([["a", 2], ["b", 1]])
    );
    core.shutdown().await.unwrap();
}

/// WF-DECLARATION-RELEASE: an install changing only an undeclared package
/// changes neither the pinned workflow nor its parked continuation.
#[tokio::test(flavor = "multi_thread")]
async fn an_undeclared_package_release_leaves_a_published_workflow_and_its_resume_unchanged() {
    let _plan_law = PLAN_LAW.lock().await;
    let clock = Arc::new(lash_core::testing::TestClock::new(1_700_000_000_000));
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .unwrap(),
    );
    let (first, packages) = dataframe_core_on(stores.clone(), 7, "before-b-change").await;
    let pin = lash::process::HostArtifactPin::mint();
    let environment = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(DATAFRAME_TOOLS, &())
        .link_tool(
            DATAFRAME_TOOLS.tool_provider(lash::tool_provider_key!("dataframe")),
            &run_definition(),
        )
        .build()
        .unwrap();
    let mut declaration = packages.surface().selection();
    declaration.remove(&lash::package::PackagePath::parse("other").unwrap());
    let selected = packages.declare(declaration.clone());
    let imported = first
        .host_artifacts()
        .import_workflow(
            lash::workflow::document::SourceLanguage::TypeScript,
            "export async function rows() { await sleep(60000); return await dataframe.run([]); }",
            &environment,
            selected,
            None,
        )
        .await
        .unwrap();
    let lash::workflow::WorkflowImport::Imported {
        document,
        packages: selected,
        ..
    } = imported
    else {
        panic!("{imported:?}");
    };
    let publication = published(
        first
            .host_artifacts()
            .publish_workflow(
                &pin,
                &document,
                &Name::new("rows"),
                &environment,
                selected,
                None,
            )
            .await
            .unwrap(),
    );
    let env_ref = first
        .host_artifacts()
        .publish_process_env(&pin, &environment)
        .await
        .unwrap();
    let process = start_with(
        &first,
        &env_ref,
        &publication.definition,
        "before-b-change",
        serde_json::Map::new(),
    )
    .await;
    let due = timer_park_after(&first, &process, 0).await;
    first.shutdown().await.unwrap();
    let (second, next) = dataframe_core_on(stores, 99, "after-b-change").await;
    assert_ne!(packages.digest(), next.digest());
    let key = lash::package::PackagePath::parse("dataframe").unwrap();
    assert_eq!(
        packages.surface().package(&key).unwrap().digest(),
        next.surface().package(&key).unwrap().digest()
    );
    let bytes = second
        .backend()
        .definition_store()
        .get_process_definition(&publication.definition.id)
        .await
        .unwrap()
        .unwrap();
    let descriptor =
        lash::process::ProcessDefinitionDraft::from_store_bytes(&publication.definition.id, &bytes)
            .unwrap();
    assert_eq!(
        descriptor.value().as_json()["declared_packages"],
        serde_json::to_value(&declaration).unwrap()
    );
    let lash::workflow::WorkflowRead::Inspected(read) = second
        .host_artifacts()
        .definition_graph(&publication.definition.id)
        .await
        .unwrap()
    else {
        panic!("the old workflow still links");
    };
    assert_eq!(read.document.identity(), document.id());
    clock.set(due);
    assert_eq!(
        finished(&second, &process).await,
        serde_json::json!([["a", 2], ["b", 1]])
    );
    let again = start_with(
        &second,
        &env_ref,
        &publication.definition,
        "after-b-change",
        serde_json::Map::new(),
    )
    .await;
    let due = timer_park_after(&second, &again, due).await;
    clock.set(due);
    assert_eq!(
        finished(&second, &again).await,
        serde_json::json!([["a", 2], ["b", 1]])
    );
    second.shutdown().await.unwrap();
}
