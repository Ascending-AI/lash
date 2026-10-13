//! The link laws (P1, P2, R5–R7, D4): a process is the pair of a template
//! and an env, and a start links the pair only when the env maps exactly
//! the effects the template's entry performs, each to a tool that fits the
//! effect and that one of the env's components serves.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash::plugins::{FormatVersion, PluginError};
use lash::process::{ProcessConfigKey, ProcessEnv, ProcessEnvRefusal};
use lash::tools::ToolDefinitionBindingExt as _;

/// Serves `search_a` at `search.run`.
const SEARCH_A: ProcessConfigKey<()> = ProcessConfigKey::new("link-search-a", FormatVersion::ONE);
/// Serves `search_b` at `search.run`, and `search_wide`.
const SEARCH_B: ProcessConfigKey<()> = ProcessConfigKey::new("link-search-b", FormatVersion::ONE);

const KIND: &str = lash_vm_runtime::LASH_VM_ENGINE_KIND;

fn object(fields: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": fields,
    })
}

/// The search input every tool takes: a query.
fn query() -> serde_json::Value {
    object(serde_json::json!({ "q": { "type": "string" } }), &["q"])
}

/// The search result: its hits.
fn hits() -> serde_json::Value {
    object(
        serde_json::json!({ "hits": { "type": "string" } }),
        &["hits"],
    )
}

/// A search tool `name` bound at `search.run`, taking `input` and giving
/// `output`.
fn search_tool(
    name: &str,
    input: serde_json::Value,
    output: serde_json::Value,
) -> lash::tools::ToolDefinition {
    lash::tools::ToolDefinition::raw(format!("tool:{name}"), name, "Searches.", input, output)
        .expect("the tool's schemas")
        .with_execution(std::time::Duration::from_secs(60))
        .with_tool_binding(lash::tools::ToolBinding::new(["search"], "run"))
}

fn search_a() -> lash::tools::ToolDefinition {
    search_tool("search_a", query(), hits())
}

fn search_b() -> lash::tools::ToolDefinition {
    search_tool("search_b", query(), hits())
}

/// A search that also takes an optional limit, and adds a field to its
/// result: it serves an effect typed from [`search_a`], not identically.
fn search_wide() -> lash::tools::ToolDefinition {
    search_tool(
        "search_wide",
        object(
            serde_json::json!({
                "q": { "type": "string" },
                "limit": { "type": "integer" },
            }),
            &["q"],
        ),
        object(
            serde_json::json!({
                "hits": { "type": "string" },
                "took_ms": { "type": "integer" },
            }),
            &["hits"],
        ),
    )
}

/// Each search tool answers its own name as its hits.
struct Answer;

#[lash::async_trait]
impl lash::tools::StaticToolExecute for Answer {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        lash::tools::ToolOutcome::ok(serde_json::json!({ "hits": call.name(), "took_ms": 1 }))
            .into()
    }
}

fn component(
    key: ProcessConfigKey<()>,
    tools: Vec<lash::tools::ToolDefinition>,
) -> Arc<lash::process::StaticProcessComponent> {
    Arc::new(
        lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial(key.component()),
            lash::plugins::PluginSpec::new().with_tool_provider(
                lash::hook_key!("tools"),
                Arc::new(lash::tools::StaticToolProvider::new(tools, Answer)),
            ),
        )
        .as_component(key.format())
        .expect("a search component"),
    )
}

async fn core() -> (lash::Backend, lash::LashCore) {
    let stores: Arc<dyn lash::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores"),
    );
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .build()
        .expect("the backend assembles");
    let workers = lash::vm::WorkerService::default();
    let bounds = lash::vm::RunBounds {
        charge: 1_000_000,
        memory: 64 * 1024 * 1024,
        ..workers.config().run_bounds
    };
    let core = lash::LashCore::standard_builder(backend.clone())
        .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
            workers,
            lash::vm::TYPESCRIPT_DIALECT,
            bounds,
        )))
        .process_component(component(SEARCH_A, vec![search_a()]), SEARCH_A)
        .process_component(
            component(SEARCH_B, vec![search_b(), search_wide()]),
            SEARCH_B,
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("link-laws"),
            lash::persistence::LeaseIncarnationId::new("link-laws-boot"),
        ))
        .expect("the core builds");
    (backend, core)
}

/// An env naming `component` and linking each of `tools` to its provider.
fn env(component: ProcessConfigKey<()>, tools: &[lash::tools::ToolDefinition]) -> ProcessEnv {
    tools
        .iter()
        .fold(
            ProcessEnv::builder(lash::MaxToolCalls::new(4)).component(component, &()),
            |builder, tool| builder.link_tool(component.tool_provider("tools"), tool),
        )
        .build()
        .expect("the env builds")
}

/// `search(q)` performs `search.run` once, typed as [`search_a`] projects.
const SEARCH: &str = r#"kernel 1
numbers float
effect search.run(input: Any) -> Any
entry search() -> Any

fn search() {
  let request = {q: "x"}
  let got = perform search.run(request) as Any
  return got
}

main {
  finish null
}
"#;

/// The start payload of `entry` of `text`, its effects typed as `typed`
/// declares them, published under a host pin.
async fn payload(
    backend: &lash::Backend,
    text: &str,
    entry: &str,
    typed: &[(&str, &lash::tools::ToolDefinition)],
) -> serde_json::Value {
    let mut document = lash::workflow::document::parse_document(text).expect("the document parses");
    for (effect, tool) in typed {
        document.manifest.effects.insert(
            lash::workflow::document::EffectName::new(*effect).expect("an effect name"),
            lash_kernel_check::tool_signature(
                tool.contract.input_schema.canonical(),
                tool.contract.output_schema.canonical(),
            ),
        );
    }
    let identity = lash_vm_runtime::KernelDocuments::new(backend.module_artifacts())
        .publish(
            &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .expect("a host pin is unguarded"),
            &document,
        )
        .await
        .expect("the document publishes");
    serde_json::to_value(lash_vm_runtime::KernelProcessInput {
        document: identity,
        entry: lash::workflow::document::Name::new(entry),
        args: serde_json::Map::new(),
    })
    .expect("the input encodes")
}

/// Start `payload` under `env`, answering the start's typed link refusal.
async fn start(
    core: &lash::LashCore,
    env: &ProcessEnv,
    payload: serde_json::Value,
) -> Result<lash_core::ProcessId, ProcessEnvRefusal> {
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), env)
        .await
        .expect("the env is published");
    let settings = core
        .processes()
        .engine_defaults(KIND, &payload)
        .expect("the kernel's settings");
    core.processes()
        .start(
            lash_core::ProcessStartRequest::engine(
                KIND.to_owned(),
                payload,
                lash_core::EngineLaunch::new(env_ref, settings),
                lash_core::ProcessOriginator::host(),
                lash_core::LifetimeDecision::Detached,
            ),
            core.effect_host(),
        )
        .await
        .map(|receipt| receipt.process_id)
        .map_err(|error| refusal(&error))
}

/// The typed link refusal `error` carries.
fn refusal(error: &lash::EmbedError) -> ProcessEnvRefusal {
    let spelled = format!("{error:?}");
    match error {
        lash::EmbedError::Plugin(PluginError::ProcessEnvRefused { refusal }) => refusal.clone(),
        lash::EmbedError::Plugin(PluginError::RuntimeEffectController(controller)) => {
            match controller.cause.as_ref() {
                Some(lash_core::RuntimeErrorCause::ProcessEnvRefused { refusal }) => {
                    refusal.as_ref().clone()
                }
                _ => panic!("the start is refused as a link: {spelled}"),
            }
        }
        _ => panic!("the start is refused as a link: {spelled}"),
    }
}

async fn answer(core: &lash::LashCore, process: &lash_core::ProcessId) -> serde_json::Value {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        core.processes().await_output(process),
    )
    .await
    .expect("the process ends within a minute")
    .expect("its end is read");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process ended without an answer: {output:?}");
    };
    assert!(output.is_success(), "{output:?}");
    output.value_for_projection()
}

/// TEMPLATE-SUBSTITUTES (law 5; R5, R6, P1): one document performing
/// `search.run` runs under two envs that link it to two tools, each served
/// by its own component. An env whose component does not serve the linked
/// tool refuses the link, naming the effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_template_links_under_two_envs_and_an_unserved_tool_refuses() {
    let (backend, core) = core().await;
    let payload = payload(&backend, SEARCH, "search", &[("search.run", &search_a())]).await;
    for (component, tool) in [(SEARCH_A, search_a()), (SEARCH_B, search_b())] {
        let process = start(&core, &env(component, &[tool.clone()]), payload.clone())
            .await
            .expect("the pair links");
        assert_eq!(
            answer(&core, &process).await["hits"],
            serde_json::json!(tool.manifest.name),
            "the run performed the effect on its env's tool"
        );
    }
    let unserved = env(SEARCH_A, &[search_b()]);
    assert_eq!(
        start(&core, &unserved, payload).await,
        Err(ProcessEnvRefusal::ToolNotServed {
            effect: "search.run".to_owned(),
            tool: "tool:search_b".to_owned(),
            provider: format!("{}/tool_provider:tools", SEARCH_A.component()),
        })
    );
}

/// FIT-EXACT-OPT-IN (law 6; R7) and OPEN-RESULT-FITS (law 18, the link
/// case; D4): a tool that takes an extra optional input and adds a result
/// field serves the effect typed from the narrower tool, so the pair links
/// and runs under `Compatible`; a host that opts into `Exact` is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compatible_tool_links_unless_the_host_asks_for_an_exact_fit() {
    let (backend, core) = core().await;
    let payload = payload(&backend, SEARCH, "search", &[("search.run", &search_a())]).await;
    let wide = env(SEARCH_B, &[search_wide()]);
    let effect = lash::workflow::document::EffectName::new("search.run").unwrap();
    core.processes()
        .check_link(KIND, &payload, &wide, &BTreeMap::new())
        .await
        .expect("a compatible tool links");
    let exact = [(effect, lash_core::FitPolicy::Exact)]
        .into_iter()
        .collect();
    let refused = core
        .processes()
        .check_link(KIND, &payload, &wide, &exact)
        .await
        .expect_err("an exact fit refuses it");
    assert!(
        matches!(refusal(&refused), ProcessEnvRefusal::EffectUnfit { ref effect, ref tool, .. }
            if effect == "search.run" && tool == "tool:search_wide"),
        "{refused:?}"
    );
    let process = start(&core, &wide, payload).await.expect("the pair links");
    assert_eq!(
        answer(&core, &process).await["hits"],
        serde_json::json!("search_wide")
    );
}

/// `orchestrate()` starts `worker` by reference; only `worker` searches.
const ORCHESTRATE: &str = r#"kernel 1
numbers float
effect processes.start(input: Any) -> Any
effect search.run(input: Any) -> Any
entry orchestrate() -> Any
entry worker() -> Any

fn worker() {
  let request = {q: "x"}
  let got = perform search.run(request) as Any
  return got
}

fn orchestrate() {
  let request = {definition: &worker}
  let handle = perform processes.start(request) as Any
  return handle
}

main {
  finish null
}
"#;

/// `relay(f)` calls through a value while it holds `worker`.
const RELAY: &str = r#"kernel 1
numbers float
effect search.run(input: Any) -> Any
entry relay(f: Any) -> Any

fn worker() {
  let request = {q: "x"}
  let got = perform search.run(request) as Any
  return got
}

fn relay(f) {
  let held = {definition: &worker}
  let got = apply f(held)
  return got
}

main {
  finish null
}
"#;

/// LINK-EXACTLY (law 7; P1, P2): an env that misses a performed effect, or
/// links one the entry does not perform, refuses. An orchestrator that only
/// passes a child's reference does not link the child's effects, and an
/// entry whose value call cannot be certified is refused, typed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_env_links_exactly_what_its_entry_performs() {
    let (backend, core) = core().await;
    let search = payload(&backend, SEARCH, "search", &[("search.run", &search_a())]).await;
    assert_eq!(
        start(&core, &env(SEARCH_A, &[]), search).await,
        Err(ProcessEnvRefusal::EffectNotLinked {
            effect: "search.run".to_owned()
        })
    );
    let start_tool = lash::process_controls::process_tool_definition(
        lash::process_controls::ProcessControlTool::Start,
    );
    let orchestrate = payload(
        &backend,
        ORCHESTRATE,
        "orchestrate",
        &[
            ("search.run", &search_a()),
            ("processes.start", &start_tool),
        ],
    )
    .await;
    assert_eq!(
        start(&core, &env(SEARCH_A, &[search_a()]), orchestrate).await,
        Err(ProcessEnvRefusal::EffectNotLinked {
            effect: "processes.start".to_owned()
        }),
        "the orchestrator performs the start, not its child's search"
    );
    let worker_only = payload(
        &backend,
        ORCHESTRATE,
        "worker",
        &[
            ("search.run", &search_a()),
            ("processes.start", &start_tool),
        ],
    )
    .await;
    let process = start(&core, &env(SEARCH_A, &[search_a()]), worker_only)
        .await
        .expect("the worker links its own search alone");
    answer(&core, &process).await;
    let relay = payload(&backend, RELAY, "relay", &[("search.run", &search_a())]).await;
    assert!(
        matches!(
            start(&core, &env(SEARCH_A, &[search_a()]), relay).await,
            Err(ProcessEnvRefusal::Uncertified { ref reason }) if reason.contains("worker")
        ),
        "an uncertifiable value call is refused"
    );
    let nothing = payload(
        &backend,
        "kernel 1\nnumbers float\nentry idle() -> Any\n\nfn idle() {\n  return null\n}\n\nmain {\n  finish null\n}\n",
        "idle",
        &[],
    )
    .await;
    assert_eq!(
        start(&core, &env(SEARCH_A, &[search_a()]), nothing).await,
        Err(ProcessEnvRefusal::EffectNotPerformed {
            effect: "search.run".to_owned()
        })
    );
}
