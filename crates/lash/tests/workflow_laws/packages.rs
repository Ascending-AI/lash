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
async fn dataframe_core() -> lash::LashCore {
    let lash = lash_vm_releases::built().expect("lash's packages");
    let kernel = lash
        .iter()
        .find(|package| package.path().to_string() == "kernel")
        .expect("the kernel package");
    let dataframe = lash::package::testing::dataframe(kernel).expect("the fixture builds");
    let held = Arc::new(
        lash::package::HeldPackages::install(lash.iter().cloned().chain([dataframe]))
            .expect("one registration"),
    );
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores"),
    );
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    let workers = lash::vm::WorkerService::default();
    let bounds = workers.config().run_bounds;
    lash::LashCore::standard_builder(lash_conformance::backend_over(stores))
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
        .build(node_identity("dataframe"))
        .expect("the core builds")
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
    let core = dataframe_core().await;
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
    let selected = artifacts.cell_packages().await.expect("the workflow gate");
    let imported = artifacts
        .import_workflow(
            lash::workflow::document::SourceLanguage::TypeScript,
            ROWS,
            &environment,
            selected,
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
