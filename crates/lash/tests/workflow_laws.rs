//! A host's workflow on the kernel document (FIG-5715): a document is
//! admitted as a definition, read back as its total view, edited through
//! typed transactions, published again and run, on SQLite stores.
#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "test target: these laws fail by panicking"
)]

use std::sync::Arc;

use futures_util::StreamExt as _;

use lash::workflow::authoring::{DataValue, Operation, ValueTarget};
use lash::workflow::document::{Document, Expr, Literal, Name, Place, Rhs, Stmt, Unit};
use lash::workflow::edit::{Draft, Transaction};
use lash::workflow::{WorkflowDocumentEntry, WorkflowPublication, WorkflowPublish, WorkflowRead};

/// `guarded(name)`: binds `out` inside a `try`, sleeps once in each turn of
/// a nested loop, and returns `out`. Written as a kernel document: no
/// source language is involved anywhere in these laws.
const GUARDED: &str = r#"kernel 1
numbers float
entry guarded(name: Text) -> Any

fn guarded(name) {
  let out = "start"
  try {
    set out = name
  } catch e {
    set out = "caught"
  }
  for outer in [1, 2] {
    for inner in [1] {
      do sleep 5
    }
  }
  return out
}

main {
  finish null
}
"#;

/// `fan(items)`: one task for each of three elements, each performing the
/// one `tools.echo` of the shared closure, joined together.
const FAN: &str = r#"kernel 1
numbers float
effect tools.echo(input: Any) -> Any
entry fan(a: Any, b: Any, c: Any) -> Any

fn fan(a, b, c) {
  let work = fn(item) {
    let input = {value: item}
    let got = perform tools.echo(input) as Any
    return got
  }
  let first = spawn apply work(a)
  let second = spawn apply work(b)
  let third = spawn apply work(c)
  let tasks = [first, second, third]
  let every = join all tasks
  return every
}

main {
  finish null
}
"#;

/// `echoed(name)`: performs `tools.echo` once, sleeps, and returns what
/// the echo answered.
const ECHOED: &str = r#"kernel 1
numbers float
effect tools.echo(input: Any) -> Any
entry echoed(name: Text) -> Any

fn echoed(name) {
  let input = {value: name}
  let got = perform tools.echo(input) as Any
  do sleep 5
  return got
}

main {
  finish null
}
"#;

/// `supervisor(value)` starts the entry `worker` of its own document by
/// function reference, awaits it and returns what it ended with.
const SUPERVISOR: &str = r#"kernel 1
numbers float
effect processes.start(input: Any) -> Any
effect processes.await(input: Any) -> Any
entry supervisor(value: Any) -> Any
entry worker(value: Any) -> Any

fn worker(value) {
  return [value, "worked"]
}

fn supervisor(value) {
  let args = {value: value}
  let request = {definition: &worker, args: args}
  let handle = perform processes.start(request) as Any
  let wait = {handle: handle}
  let done = perform processes.await(wait) as Any
  return done
}

main {
  finish null
}
"#;

struct Echo;

/// The arguments of every `tools.echo` call this binary made, in order.
static ECHO_CALLS: std::sync::Mutex<Vec<serde_json::Value>> = std::sync::Mutex::new(Vec::new());

#[lash::async_trait]
impl lash::tools::StaticToolExecute for Echo {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        ECHO_CALLS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call.args.clone());
        lash::tools::ToolOutcome::ok(call.args["value"].clone()).into()
    }
}

/// The component that contributes `tools.echo` to a workflow process.
const ECHO_TOOLS: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("workflow-echo", lash::plugins::FormatVersion::ONE);

fn echo_definitions() -> Vec<lash::tools::ToolDefinition> {
    use lash::tools::ToolDefinitionBindingExt as _;
    vec![
        lash::tools::ToolDefinition::raw(
            "tool:echo",
            "echo",
            "Answers its value.",
            serde_json::json!({"type": "object"}),
            serde_json::json!({}),
        )
        .expect("tool schema")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(lash::tools::ToolBinding::new(["tools"], "echo")),
    ]
}

fn echo_tool() -> Arc<lash::tools::StaticToolProvider<Echo>> {
    Arc::new(lash::tools::StaticToolProvider::new(
        echo_definitions(),
        Echo,
    ))
}

fn echo_component() -> lash::process::StaticProcessComponent {
    lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(ECHO_TOOLS.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider(lash::tool_provider_key!("tools"), echo_tool()),
    )
    .as_component(ECHO_TOOLS.format())
    .expect("the echo tools are a process component")
}

async fn core() -> lash::LashCore {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    core_over(stores, "boot").await
}

/// A node over `stores`, as the incarnation `boot` of the one deployment.
async fn core_over(stores: Arc<lash_sqlite_store::SqliteStoreSet>, boot: &str) -> lash::LashCore {
    core_with_process_tools(stores, boot, true, EchoComponent::Installed).await
}

/// What a node does with the echo component.
#[derive(Clone, Copy)]
enum EchoComponent {
    Installed,
    /// The deployment no longer installs it and declares it retired
    /// (FIG-6296).
    Retired,
}

async fn core_with_process_tools(
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    boot: &str,
    process_tools: bool,
    echo: EchoComponent,
) -> lash::LashCore {
    builder_with_process_tools(stores, process_tools, echo)
        .await
        .build(node_identity(boot))
        .expect("the core builds")
}

/// The lease identity of the incarnation `boot` of the one deployment.
fn node_identity(boot: &str) -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque(
        lash_core::LeaseOwnerId::new("workflow-laws-worker"),
        lash_core::LeaseIncarnationId::new(format!("workflow-laws-{boot}")),
    )
}

/// A node's builder over `stores`, before it is built.
async fn builder_with_process_tools(
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    process_tools: bool,
    echo: EchoComponent,
) -> lash::LashCoreBuilder {
    builder_on_workers(
        stores,
        process_tools,
        echo,
        lash::vm::WorkerService::default(),
    )
    .await
}

/// A node's builder over `stores` whose processes run on `workers`.
async fn builder_on_workers(
    stores: Arc<dyn lash_core::StoreSet>,
    process_tools: bool,
    echo: EchoComponent,
    workers: lash::vm::WorkerService,
) -> lash::LashCoreBuilder {
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    let backend = lash_conformance::backend_over(stores);
    let bounds = lash::vm::RunBounds {
        charge: 1_000_000,
        memory: 64 * 1024 * 1024,
        ..workers.config().run_bounds
    };
    let builder = lash::LashCore::standard_builder(backend)
        .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
            workers,
            lash::vm::TYPESCRIPT_DIALECT,
            bounds,
        )))
        .tools(lash::tool_provider_key!("tools"), echo_tool());
    let builder = match echo {
        EchoComponent::Installed => {
            builder.process_component(Arc::new(echo_component()), ECHO_TOOLS)
        }
        EchoComponent::Retired => builder.retire_process_component(
            lash::process::ProcessComponentDeclaration::initial(
                ECHO_TOOLS.component(),
                ECHO_TOOLS.format(),
            )
            .expect("the echo component's id"),
        ),
    };
    // A process a workflow starts runs under its starter's environment.
    let builder = if process_tools {
        builder
            .plugin(Arc::new(
                lash::process_controls::SessionProcessAdminPluginFactory::new(
                    lash_core::lifetime::session_or_starter,
                    lash::process::env::policy::from_run(|_| Ok(Vec::new())),
                ),
            ))
            .process_component(
                Arc::new(lash::process_controls::ProcessControlsComponent::new(
                    lash_core::lifetime::session_or_starter,
                    lash::process::env::policy::restrict_or(lash::process::env::policy::refuse(
                        "a workflow process starts only inside a process",
                    )),
                )),
                lash::process_controls::PROCESS_CONTROLS_CONFIG,
            )
    } else {
        builder
    };
    builder
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
}

/// The environment a workflow process runs under: `tools.echo`, and with
/// `process_tools` the process controls, each from its component.
fn environment(process_tools: bool) -> lash::process::ProcessEnv {
    let mut builder = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(ECHO_TOOLS, &());
    for tool in echo_definitions() {
        builder = builder.link_tool(
            ECHO_TOOLS.tool_provider(lash::tool_provider_key!("tools")),
            &tool,
        );
    }
    if process_tools {
        use lash::process_controls::ProcessControlTool;
        builder = builder.component(lash::process_controls::PROCESS_CONTROLS_CONFIG, &());
        for control in [ProcessControlTool::Start, ProcessControlTool::Await] {
            builder = builder.link_tool(
                lash::process_controls::PROCESS_CONTROLS_PROVIDER,
                &lash::process_controls::process_tool_definition(control),
            );
        }
    }
    builder.build().expect("the environment builds")
}

/// `environment` restricted to `effects`: the env a process that performs
/// exactly those effects starts under (LINK-EXACTLY).
fn linking(environment: &lash::process::ProcessEnv, effects: &[&str]) -> lash::process::ProcessEnv {
    let effects: Vec<_> = effects
        .iter()
        .map(|effect| lash::workflow::document::EffectName::new(*effect).expect("an effect name"))
        .collect();
    environment
        .restrict(&effects)
        .expect("the environment links each effect")
}

fn published(publish: WorkflowPublish) -> WorkflowPublication {
    match publish {
        WorkflowPublish::Published(publication) => *publication,
        other => panic!("the workflow publishes: {other:?}"),
    }
}

fn publication_document(publication: &WorkflowPublication) -> lash::workflow::WorkflowDocument {
    lash::workflow::WorkflowDocument::derive(
        publication
            .document
            .to_document()
            .expect("explicit typed read"),
        publication.reference.entry.clone(),
        lash::vm::standard_functions().expect("library").as_ref(),
    )
    .expect("publication view")
}

// The kernel-text example in `lash::workflow::document`'s module docs is
// `src/workflow/document_example.rs`, included into the rustdoc verbatim;
// this test is its compile-and-run proof.
include!("../src/workflow/document_example.rs");

#[test]
fn the_document_module_docs_example_runs() {
    document_example();
}

/// FIG-6124: publication and start staging expose their typed env refusal
/// through the same host accessor, without controller-wrapper knowledge.
#[tokio::test]
async fn a_host_start_with_a_narrow_environment_is_refused_typed() {
    let core = core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let list_env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(lash::process_controls::PROCESS_CONTROLS_CONFIG, &())
        .link_tool(
            lash::process_controls::PROCESS_CONTROLS_CONFIG
                .tool_provider(lash::tool_provider_key!("tools")),
            &lash::process_controls::process_tool_definition(
                lash::process_controls::ProcessControlTool::List,
            ),
        )
        .build()
        .expect("list environment");
    let mut document = lash::workflow::document::parse_document(
        r#"kernel 1
numbers float
effect processes.list(input: Any) -> Any
entry list() -> Any
fn list() {
  let input = {}
  let rows = perform processes.list(input) as Any
  return rows
}

main { finish null }
"#,
    )
    .expect("list document");
    let checked = artifacts
        .workflow_environment(&list_env)
        .await
        .expect("environment")
        .expect("kernel environment");
    let effect = lash::workflow::document::EffectName::new("processes.list").expect("effect");
    document
        .manifest
        .effects
        .insert(effect.clone(), checked.effects()[&effect].clone());
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &Name::new("list"),
                &list_env,
                None,
            )
            .await
            .expect("publish list"),
    );
    let narrow = list_env.restrict(std::iter::empty()).expect("narrow env");
    let env_ref = artifacts
        .publish_process_env(&pin, &narrow)
        .await
        .expect("publish narrow env");
    let refused = core
        .processes()
        .start(
            lash::process::ProcessStartRequest::definition(
                publication.definition.id,
                Some(publication.definition.signature),
                Default::default(),
                lash::process::EngineLaunch::new(
                    env_ref,
                    core.processes()
                        .engine_defaults(
                            lash::process::LASH_VM_ENGINE_KIND,
                            &serde_json::Value::Null,
                        )
                        .expect("settings"),
                ),
                lash::process::ProcessOriginator::host(),
                lash::process::LifetimeDecision::Detached,
            )
            .with_host_start_key("narrow-env"),
            core.effect_host(),
        )
        .await
        .expect_err("the missing link refuses the start");
    assert_eq!(
        refused.process_env_refusal(),
        Some(&lash::process::ProcessEnvRefusal::EffectNotLinked {
            effect: "processes.list".into(),
        })
    );

    let missing =
        lash::process::ProcessConfigKey::<()>::new("missing", lash::plugins::FormatVersion::ONE);
    let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(missing, &())
        .build()
        .expect("candidate env");
    let refused = artifacts
        .publish_process_env(&pin, &env)
        .await
        .expect_err("uninstalled component");
    assert_eq!(
        refused.process_env_refusal(),
        Some(&lash::process::ProcessEnvRefusal::ComponentNotInstalled {
            id: "missing".into(),
        })
    );
    core.shutdown().await.expect("shutdown");
}

/// FIG-6124 / LINK-EXACTLY: host capture projects a candidate to the entry's
/// certified closure, keeping each recorded link and every other env field.
/// Checking and starting the broader candidate still refuses extra links.
#[tokio::test]
async fn a_host_captures_exactly_the_published_entrys_environment() {
    let core = core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let candidate = environment(true);
    let checked = artifacts
        .workflow_environment(&candidate)
        .await
        .expect("environment")
        .expect("kernel environment");
    let mut document = lash::workflow::document::parse_document(SUPERVISOR).expect("document");
    for effect in document
        .manifest
        .effects
        .keys()
        .cloned()
        .collect::<Vec<_>>()
    {
        document
            .manifest
            .effects
            .insert(effect.clone(), checked.effects()[&effect].clone());
    }
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &Name::new("supervisor"),
                &candidate,
                None,
            )
            .await
            .expect("publish"),
    );
    let captured = core
        .processes()
        .capture(&publication.definition.id, &candidate)
        .await
        .expect("capture");
    assert_eq!(
        captured,
        linking(&candidate, &["processes.start", "processes.await"])
    );
    let WorkflowRead::Inspected(inspection) = artifacts
        .definition_graph(&publication.definition.id)
        .await
        .expect("read")
    else {
        panic!("the published workflow is readable");
    };
    // The definition's engine payload is available through its recorded document.
    let payload = serde_json::json!({
        "document": inspection.document.reference().document,
        "entry": "supervisor",
        "args": {},
    });
    core.processes()
        .check_link(
            lash::process::LASH_VM_ENGINE_KIND,
            &payload,
            &captured,
            &Default::default(),
        )
        .await
        .expect("captured links exactly");
    let refused = core
        .processes()
        .check_link(
            lash::process::LASH_VM_ENGINE_KIND,
            &payload,
            &candidate,
            &Default::default(),
        )
        .await
        .expect_err("the extra echo link is refused");
    assert_eq!(
        refused.process_env_refusal(),
        Some(&lash::process::ProcessEnvRefusal::EffectNotPerformed {
            effect: "tools.echo".into()
        })
    );
    let refused = core
        .processes()
        .capture(&publication.definition.id, &linking(&candidate, &[]))
        .await
        .expect_err("missing performed effects refuse capture");
    assert!(matches!(
        refused.process_env_refusal(),
        Some(lash::process::ProcessEnvRefusal::EffectNotLinked { .. })
    ));
    let mut links = candidate.effects().clone();
    let await_effect =
        lash::workflow::document::EffectName::new("processes.await").expect("effect");
    let link = links.get_mut(&await_effect).expect("await link");
    let tool = link.tool().to_string();
    link.provider = lash::plugins::ToolProviderHandle::new(
        lash::plugins::RegistryKind::Process,
        link.provider.owner(),
        lash::tool_provider_key!("unserved"),
    );
    let unserved = lash::process::ProcessEnv::builder(candidate.max_tool_calls())
        .effects(links)
        .component(ECHO_TOOLS, &())
        .component(lash::process_controls::PROCESS_CONTROLS_CONFIG, &())
        .build()
        .expect("unserved candidate");
    let refused = core
        .processes()
        .capture(&publication.definition.id, &unserved)
        .await
        .expect_err("unserved recorded link refuses capture");
    assert_eq!(
        refused.process_env_refusal(),
        Some(&lash::process::ProcessEnvRefusal::CaptureMissing {
            effect: await_effect.to_string(),
            tool,
        })
    );
    // The captured pair is also accepted at the start hold point.
    let env_ref = artifacts
        .publish_process_env(&pin, &captured)
        .await
        .expect("publish captured env");
    let process = start_with(
        &core,
        &env_ref,
        &publication.definition,
        "captured",
        serde_json::Map::from_iter([("value".into(), serde_json::json!("captured"))]),
    )
    .await;
    let _ = finished(&core, &process).await;
    core.shutdown().await.expect("shutdown");
}

/// Starts `definition` as `guarded(name)` and answers the process.
async fn start(
    core: &lash::LashCore,
    env_ref: &lash_core::ProcessEnvRef,
    definition: &lash_core::ProcessDefinition,
    key: &str,
    name: &str,
) -> lash_core::ProcessId {
    let mut args = serde_json::Map::new();
    args.insert("name".to_owned(), serde_json::json!(name));
    start_with(core, env_ref, definition, key, args).await
}

async fn start_with(
    core: &lash::LashCore,
    env_ref: &lash_core::ProcessEnvRef,
    definition: &lash_core::ProcessDefinition,
    key: &str,
    args: serde_json::Map<String, serde_json::Value>,
) -> lash_core::ProcessId {
    core.processes()
        .start(
            lash_core::ProcessStartRequest::definition(
                definition.id.clone(),
                Some(definition.signature.clone()),
                args,
                lash_core::EngineLaunch::new(
                    env_ref.clone(),
                    core.processes()
                        .engine_defaults(
                            lash::process::LASH_VM_ENGINE_KIND,
                            &serde_json::Value::Null,
                        )
                        .expect("the kernel states its settings"),
                ),
                lash_core::ProcessOriginator::host(),
                lash_core::LifetimeDecision::Detached,
            )
            .with_host_start_key(key),
            core.effect_host(),
        )
        .await
        .expect("the process starts")
        .process_id
}

/// What the process finished with.
async fn finished(core: &lash::LashCore, process_id: &lash_core::ProcessId) -> serde_json::Value {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        core.processes().await_output(process_id),
    )
    .await
    .expect("the process settles")
    .expect("the output reads");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process settles with an output: {output:?}");
    };
    assert!(output.is_success(), "the process finishes: {output:?}");
    output.value_for_projection()
}

/// A document is admitted as a definition, read back with its derived
/// view, edited inside its `try` region through a typed transaction and
/// published as a second definition. Each definition's process runs the
/// document it was admitted under, through every park of the nested loop,
/// and a read of the earlier process still answers the earlier document.
#[tokio::test]
async fn a_document_is_published_read_edited_in_a_try_region_republished_and_run() {
    let core = core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let checked = artifacts
        .workflow_environment(&environment)
        .await
        .expect("the environment reads")
        .expect("the kernel engine reads workflow documents");
    let entry = Name::new("guarded");

    let document: Document = lash::workflow::document::parse_document(
        &GUARDED.replace("set out = name", "set out = \"operator\""),
    )
    .expect("the document parses");
    let first = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &entry,
                &environment,
                None,
            )
            .await
            .expect("the publication answers"),
    );
    assert_eq!(
        &first.document.to_document().expect("explicit typed read"),
        &document
    );
    assert_eq!(
        first.reference.entry,
        WorkflowDocumentEntry::Entry {
            function: entry.clone()
        }
    );

    let WorkflowRead::Inspected(inspection) = artifacts
        .definition_graph(&first.definition.id)
        .await
        .expect("the definition reads")
    else {
        panic!("a published workflow has a document");
    };
    assert_eq!(inspection.definition, first.definition);
    assert_eq!(
        inspection.document.document(),
        &first.document.to_document().expect("read bytes")
    );

    // The view names the one wait of the body inside both loops.
    let graph = inspection.document.graph();
    let unit = Unit::Function(entry.clone());
    let [sleep] = graph.execution_sites() else {
        panic!(
            "the body has one execution site: {:?}",
            graph.execution_sites()
        );
    };
    assert_eq!(sleep.site.unit, unit);
    assert_eq!(sleep.loops.len(), 2, "the wait sits in the inner loop");

    // Change the existing status literal while preserving all control and action sites.
    let guarded = graph.nodes().iter().find(|node| node.site.unit == unit && matches!(&node.kind, lash::workflow::graph::NodeKind::Expr(lash::workflow::graph::ExprNode::Literal(Literal::Text(text))) if text == "operator")).map(|node| node.site.clone()).expect("the status literal");
    let mut draft =
        Draft::open(inspection.document.document().clone()).expect("the document opens");
    let applied = draft
        .apply(
            &Transaction {
                base: draft.identity(),
                edits: vec![Operation::SetValue {
                    base: draft.identity(),
                    target: ValueTarget::Literal {
                        expression: guarded.clone(),
                    },
                    value: DataValue::Literal(Literal::Text("edited".to_owned())),
                }],
            },
            &checked.checker(),
        )
        .expect("the edit applies");
    assert_ne!(applied.admitted.identity, first.document.id());
    let second = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(draft.document().clone()),
                &entry,
                &environment,
                None,
            )
            .await
            .expect("the publication answers"),
    );
    assert_ne!(second.definition.id, first.definition.id);
    assert_eq!(second.document.id(), applied.admitted.identity);
    let survivor = applied
        .correspondence
        .survivor(&sleep.site)
        .expect("the wait survives the edit");
    assert_eq!((&survivor.to, survivor.edited), (&sleep.site, false));

    // The first definition is immutable: a process of it runs the document
    // it was admitted with, beside a process of the edited one.
    let env_ref = artifacts
        .publish_process_env(&pin, &linking(&environment, &[]))
        .await
        .expect("publish the process environment");
    let kept = start(&core, &env_ref, &first.definition, "first", "operator").await;
    let edited = start(&core, &env_ref, &second.definition, "second", "operator").await;
    assert_eq!(finished(&core, &kept).await, serde_json::json!("operator"));
    assert_eq!(finished(&core, &edited).await, serde_json::json!("edited"));

    let WorkflowRead::Inspected(kept_read) = core
        .processes()
        .graph(&kept)
        .await
        .expect("the process reads")
    else {
        panic!("a kernel process has a document");
    };
    assert_eq!(
        kept_read.document.document(),
        &first.document.to_document().expect("read bytes")
    );
    assert_eq!(kept_read.definition, first.definition);
}

/// FIG-6323: every sleep in a loop names its own execution occurrence in
/// the committed process log, so a host can highlight it without tracing.
#[tokio::test]
async fn stored_sleep_waits_name_the_sleep_site_and_each_loop_occurrence() {
    let core = core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let source = GUARDED
        .replace(
            "entry guarded",
            "effect tools.echo(input: Any) -> Any\nentry guarded",
        )
        .replace(
            "do sleep 5",
            "do sleep 6000\n      let input = {value: name}\n      do perform tools.echo(input) as Any",
        );
    let mut document = lash::workflow::document::parse_document(&source).expect("document");
    let checked = artifacts
        .workflow_environment(&environment)
        .await
        .expect("environment")
        .expect("kernel");
    let echo = lash::workflow::document::EffectName::new("tools.echo").expect("effect");
    document
        .manifest
        .effects
        .insert(echo.clone(), checked.effects()[&echo].clone());
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &Name::new("guarded"),
                &environment,
                None,
            )
            .await
            .expect("publication"),
    );
    let publication_view = publication_document(&publication);
    let sites = publication_view.graph().execution_sites();
    let sleep = sites
        .iter()
        .find(|site| site.kind == lash::workflow::graph::SiteKind::Sleep)
        .expect("sleep");
    let tool = sites
        .iter()
        .find(|site| site.kind == lash::workflow::graph::SiteKind::Perform)
        .expect("tool");
    let env_ref = artifacts
        .publish_process_env(&pin, &linking(&environment, &["tools.echo"]))
        .await
        .expect("environment");
    let process = start(
        &core,
        &env_ref,
        &publication.definition,
        "sleep-sites",
        "done",
    )
    .await;
    assert_eq!(finished(&core, &process).await, serde_json::json!("done"));
    let page = core
        .processes()
        .events(
            lash::process::ProcessHistoryContinuation::start(process.clone()),
            std::num::NonZeroUsize::new(100).expect("limit"),
            lash::process::ProcessEventQueryMode::Full,
        )
        .await
        .expect("stored events");
    let lash::process::ProcessEventReadOutcome::Retained(page) = page.outcome else {
        panic!("retained events");
    };
    let lash::process::ProcessEventPageEvents::Full(events) = page.events else {
        panic!("full events");
    };
    let waits: Vec<lash::process::WaitState> = events
        .iter()
        .filter(|event| event.fact.event_type() == "process.waiting")
        .map(|event| serde_json::from_value(event.fact.payload()["wait"].clone()).expect("wait"))
        .collect();
    assert_eq!(waits.len(), 2, "both loop sleeps enter a wait");
    for (occurrence, wait) in waits.iter().enumerate() {
        let at = wait
            .site
            .as_ref()
            .expect("a sleeping workflow names its site");
        assert_eq!(at.site, sleep.site);
        assert_eq!(at.occurrence, occurrence as u64);
    }
    let mut overlay = lash::workflow::ProcessWorkflowOverlayAccumulator::new(process.clone());
    for event in &events {
        overlay.committed(event);
        if let lash::process::ProcessLifecycleFact::Waiting { wait } = &event.fact {
            let at = wait.execution_site().expect("workflow coordinate");
            let state = overlay.snapshot();
            let row = state
                .sites
                .iter()
                .find(|row| row.at == *at)
                .expect("waiting row");
            assert_eq!(row.occurrences, at.occurrence + 1);
            assert_eq!(
                row.state,
                lash::workflow::ProcessWorkflowSiteState::Waiting {
                    kind: lash::workflow::ProcessWorkflowWaitKind::Sleep,
                }
            );
            assert!(state.current_sites.contains(at));
        }
    }
    let overlay = overlay.snapshot();
    for site in [&sleep.site, &tool.site] {
        let row = overlay
            .sites
            .iter()
            .find(|row| &row.at.site == site)
            .expect("site in stored fold");
        assert_eq!(row.occurrences, 2);
        let entered = events
            .iter()
            .find(|event| match &event.fact {
                lash::process::ProcessLifecycleFact::Waiting { wait }
                | lash::process::ProcessLifecycleFact::Resumed { wait } => {
                    wait.execution_site().is_ok_and(|at| &at.site == site)
                }
                lash::process::ProcessLifecycleFact::EffectOutcome(outcome) => {
                    &outcome.at.site == site
                }
                _ => false,
            })
            .expect("committed entry");
        let settled = events
            .iter()
            .rev()
            .find(|event| match &event.fact {
                lash::process::ProcessLifecycleFact::Resumed { wait } => {
                    matches!(wait.kind, lash::process::WaitKind::Sleep { .. })
                        && wait.execution_site().is_ok_and(|at| &at.site == site)
                }
                lash::process::ProcessLifecycleFact::EffectOutcome(outcome) => {
                    &outcome.at.site == site
                }
                _ => false,
            })
            .expect("committed settlement");
        assert_eq!(row.first_entered, Some(entered.sequence));
        assert_eq!(row.last_settled, Some(settled.sequence));
        assert_eq!(
            row.state,
            lash::workflow::ProcessWorkflowSiteState::Completed
        );
    }
    let snapshot = core
        .processes()
        .observe(&process)
        .snapshot()
        .await
        .expect("snapshot");
    let lash::process::ProcessReadView::Retained(view) = snapshot.read_view else {
        panic!("retained");
    };
    let snapshot_overlay = view.workflow.expect("workflow snapshot");
    assert!(
        snapshot_overlay.current_sites.is_empty(),
        "terminal clears current sites"
    );
    for row in overlay.sites {
        assert!(
            snapshot_overlay.sites.contains(&row),
            "snapshot uses the stored-event fold"
        );
    }
}

/// FIG-6323: a stretch of pure compute names the last document call it
/// entered, taken from the run's next frame (FIG-6343: no exchange of its
/// own). The process reducer keeps that last entry and counts every call.
#[tokio::test]
async fn a_process_feed_names_pure_compute_calls_and_counts_their_entries() {
    use lash::process::{ProcessObservationEventPayload, ProcessObservationStreamItem};
    use lash::workflow::{ProcessWorkflowOverlayAccumulator, ProcessWorkflowSiteState};

    let core = core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let document = lash::workflow::document::parse_document(
        r#"kernel 1
numbers float
entry computed(name: Text) -> Any
fn identity(value) { return value }
fn computed(name) {
  for n in [1, 2] { let value = call identity(name) }
  return name
}
main { finish null }
"#,
    )
    .expect("pure document");
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &Name::new("computed"),
                &environment,
                None,
            )
            .await
            .expect("publication"),
    );
    let publication_view = publication_document(&publication);
    let [call] = publication_view.graph().execution_sites() else {
        panic!("one call site");
    };
    let env_ref = artifacts
        .publish_process_env(&pin, &linking(&environment, &[]))
        .await
        .expect("environment");
    let process = start(
        &core,
        &env_ref,
        &publication.definition,
        "compute-sites",
        "done",
    )
    .await;
    let observed = core.processes().observe(&process);
    let snapshot = observed.snapshot().await.expect("snapshot");
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    assert_eq!(finished(&core, &process).await, serde_json::json!("done"));
    let mut overlay = ProcessWorkflowOverlayAccumulator::new(process);
    let mut entries = std::collections::BTreeSet::new();
    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(60), feed.next())
            .await
            .expect("feed wakes")
            .expect("feed continues")
            .expect("feed reads");
        let ProcessObservationStreamItem::Event(event) = item else {
            panic!("no gap in the fixture");
        };
        // Reconstruct each running stretch from the provisional process
        // observations, before the later committed terminal settles them.
        if let ProcessObservationEventPayload::LanguageExecution(observation) = &event.payload {
            overlay
                .observe(&publication.reference, &event.payload)
                .expect("same execution");
            if let lash_trace::TraceLanguageExecutionPayload::Node {
                at,
                fact: lash_trace::TraceNodeFact::Started { .. },
            } = &observation.execution.payload
                && at.site == call.site
            {
                entries.insert(at.occurrence);
                let state = overlay.snapshot();
                assert!(
                    state.current_sites.contains(at),
                    "the pure call stays current"
                );
                let [row] = state.sites.as_slice() else {
                    panic!("one site");
                };
                assert_eq!(row.occurrences, at.occurrence + 1);
                assert_eq!(row.state, ProcessWorkflowSiteState::Running);
            }
            if entries.contains(&1) {
                break;
            }
        }
    }
    assert_eq!(entries.last(), Some(&1), "the last entry is named");
    core.shutdown().await.expect("shutdown");
}

/// A fan-out's overlay tells its elements apart (`K-EFF-008`). Three tasks
/// perform the one `tools.echo` of a shared closure, so each has occurrence
/// 0 of that site. The process's feed reports each at the machine's own
/// identity, and the overlay folded from it holds one completed row for
/// each task at the one site, every site being the document's.
#[tokio::test]
async fn a_fan_out_overlay_holds_one_row_for_each_elements_effect() {
    use lash::process::{ProcessObservationEventPayload, ProcessObservationStreamItem};
    use lash::workflow::{WorkflowExecutionOverlayAccumulator, WorkflowOverlayOccurrence};

    let core = core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let checked = artifacts
        .workflow_environment(&environment)
        .await
        .expect("the environment reads")
        .expect("the kernel engine reads workflow documents");
    let echo = lash::workflow::document::EffectName::new("tools.echo").expect("an effect name");
    let mut document = lash::workflow::document::parse_document(FAN).expect("the document parses");
    // The document states the signature the host offers the effect under.
    document.manifest.effects.insert(
        echo.clone(),
        checked
            .effects()
            .get(&echo)
            .expect("the host offers the tool")
            .clone(),
    );
    let entry = Name::new("fan");
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &entry,
                &environment,
                None,
            )
            .await
            .expect("the publication answers"),
    );
    let publication_view = publication_document(&publication);
    let performs: Vec<_> = publication_view
        .graph()
        .execution_sites()
        .iter()
        .filter(|site| site.kind == lash::workflow::graph::SiteKind::Perform)
        .collect();
    let [perform] = performs.as_slice() else {
        panic!("the document performs at one site: {performs:?}");
    };

    let env_ref = artifacts
        .publish_process_env(&pin, &linking(&environment, &["tools.echo"]))
        .await
        .expect("publish the process environment");
    let args = serde_json::json!({"a": "x", "b": "y", "c": "z"});
    let serde_json::Value::Object(args) = args else {
        unreachable!()
    };
    let process = start_with(&core, &env_ref, &publication.definition, "fan", args).await;
    let observed = core.processes().observe(&process);
    let snapshot = observed.snapshot().await.expect("the durable snapshot");
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    assert_eq!(
        finished(&core, &process).await,
        serde_json::json!(["x", "y", "z"])
    );

    let mut overlay = WorkflowExecutionOverlayAccumulator::default();
    overlay.set_document(lash::workflow::WorkflowOverlayDocument::of(
        &publication_document(&publication),
    ));
    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(60), feed.next())
            .await
            .expect("the feed wakes")
            .expect("the feed continues")
            .expect("the observation reads");
        match item {
            ProcessObservationStreamItem::Event(event) => match &event.payload {
                ProcessObservationEventPayload::LanguageExecution(observation) => {
                    overlay.observe(observation).expect("the observation folds");
                }
                ProcessObservationEventPayload::StepBodyStarted(observation) => {
                    overlay
                        .step_body_started(observation)
                        .expect("the step body start folds");
                }
                _ => {}
            },
            ProcessObservationStreamItem::Gap { .. } => overlay.reset_live(),
        }
        let done = overlay.snapshot().is_some_and(|overlay| {
            overlay
                .sites
                .iter()
                .filter(|row| {
                    row.site.site == perform.site
                        && matches!(
                            row.state.occurrence,
                            WorkflowOverlayOccurrence::Completed { .. }
                        )
                })
                .count()
                == 3
        });
        if done {
            break;
        }
    }
    let overlay = overlay.snapshot().expect("the overlay");
    assert!(overlay.mismatches.is_empty(), "{:?}", overlay.mismatches);
    let rows: Vec<_> = overlay
        .sites
        .iter()
        .filter(|row| row.site.site == perform.site)
        .collect();
    assert_eq!(rows.len(), 3, "one row for each element: {rows:#?}");
    let tasks: std::collections::BTreeSet<_> = rows.iter().map(|row| &row.site.task).collect();
    assert_eq!(tasks.len(), 3, "each row is another task's: {rows:#?}");
    for row in rows {
        assert!(
            matches!(
                row.state.occurrence,
                WorkflowOverlayOccurrence::Completed { occurrence: 0, .. }
            ),
            "each element ran the site once, as its occurrence 0: {row:#?}"
        );
        assert!(
            matches!(
                row.site.task,
                lash::workflow::document::TaskIdentity::Spawned(_)
            ),
            "an element's effect belongs to its spawned task: {row:#?}"
        );
    }
}

/// Starting a process is an effect that takes a function reference
/// (kernel spec §1). A document lists two entries; one publication holds a
/// definition for each. The supervisor's `&worker` crosses the effect
/// boundary as the definition of that entry of its own document, the child
/// runs `worker`, and the supervisor's await of the handle answers the
/// child's result.
#[tokio::test]
async fn a_process_starts_another_entry_of_its_document_by_function_reference_and_awaits_it() {
    let core = core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let checked = artifacts
        .workflow_environment(&environment)
        .await
        .expect("the environment reads")
        .expect("the kernel engine reads workflow documents");
    let mut document =
        lash::workflow::document::parse_document(SUPERVISOR).expect("the document parses");
    for effect in ["processes.start", "processes.await"] {
        let name = lash::workflow::document::EffectName::new(effect).expect("an effect name");
        let offered = checked
            .effects()
            .get(&name)
            .unwrap_or_else(|| panic!("the host offers `{effect}`"));
        document.manifest.effects.insert(name, offered.clone());
    }
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &Name::new("supervisor"),
                &environment,
                None,
            )
            .await
            .expect("the publication answers"),
    );
    let env_ref = artifacts
        .publish_process_env(
            &pin,
            &linking(&environment, &["processes.start", "processes.await"]),
        )
        .await
        .expect("publish the process environment");
    let mut args = serde_json::Map::new();
    args.insert("value".to_owned(), serde_json::json!("x"));
    let supervisor = start_with(&core, &env_ref, &publication.definition, "supervisor", args).await;
    let ended = finished(&core, &supervisor).await;
    assert!(
        ended.to_string().contains(r#"["x","worked"]"#),
        "the supervisor returns what its child ended with: {ended}"
    );
}

/// Wait for a new durable timer park, reading only after process-feed events.
/// The injected store clock holds the timer until the host observed its park.
async fn timer_park_after(
    core: &lash::LashCore,
    process: &lash_core::ProcessId,
    after: u64,
) -> u64 {
    let mut trace = Vec::new();
    let parked = async {
        let observable = core.processes().observe(process);
        let snapshot = observable
            .snapshot()
            .await
            .expect("the process snapshot reads");
        let mut feed = observable.subscribe_and_recover(snapshot.cursor);
        loop {
            let observed = core
                .processes()
                .get(process)
                .await
                .expect("the process reads")
                .expect("the process is retained");
            trace.push(format!("{observed:?}"));
            for wait in observed.lifecycle.waits() {
                if let lash_core::WaitKind::Sleep { until_ms } = wait.kind {
                    let until = u64::try_from(until_ms).expect("a nonnegative timer instant");
                    if until > after {
                        return until;
                    }
                }
            }
            assert!(
                observed.lifecycle.terminal().is_none(),
                "the process ends before the expected timer park: {observed:?}"
            );
            let event = feed
                .next()
                .await
                .expect("the process feed remains open")
                .expect("the process feed reads");
            trace.push(format!("{event:?}"));
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(120), parked)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timer park hang guard expired for {process}; trace:\n{}",
                trace.join("\n")
            )
        })
}

/// A process parked inside its inner loop outlives its node (kernel spec
/// §9). The node that started it stops while the run stands on a `sleep`
/// in the first turn of the nested loop; a second node over the same
/// SQLite file resumes the saved run from that park, takes it through the
/// remaining turns and finishes it with the value bound before the park.
#[tokio::test]
async fn a_process_parked_in_its_inner_loop_resumes_on_another_node_and_finishes() {
    let files = tempfile::tempdir().expect("a SQLite store directory");
    let clock = Arc::new(lash_core::testing::TestClock::new(1_700_000_000_000));
    let open = || async {
        Arc::new(
            lash_sqlite_store::SqliteStoreSet::open_with_clock(
                files.path().join("lash.db"),
                lash_sqlite_store::SqliteSynchronous::Normal,
                clock.clone(),
            )
            .await
            .expect("open the SQLite file stores"),
        )
    };
    let first = core_over(open().await, "first").await;
    let artifacts = first.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let document = lash::workflow::document::parse_document(GUARDED).expect("the document parses");
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &Name::new("guarded"),
                &environment,
                None,
            )
            .await
            .expect("the publication answers"),
    );
    let env_ref = artifacts
        .publish_process_env(&pin, &linking(&environment, &[]))
        .await
        .expect("publish the process environment");
    let process = start(
        &first,
        &env_ref,
        &publication.definition,
        "parked",
        "operator",
    )
    .await;

    // FIG-5976: even a delayed observer cannot miss this park: durable time
    // stands still until the first node stopped with its saved inner loop.
    let first_due = timer_park_after(&first, &process, 0).await;
    first.shutdown().await.expect("the first node stops");
    drop(first);

    clock.set(first_due);
    let second = core_over(open().await, "second").await;
    let second_due = timer_park_after(&second, &process, first_due).await;
    clock.set(second_due);
    assert_eq!(
        finished(&second, &process).await,
        serde_json::json!("operator")
    );
    let WorkflowRead::Inspected(read) = second
        .processes()
        .graph(&process)
        .await
        .expect("the process reads")
    else {
        panic!("a kernel process has a document");
    };
    assert_eq!(read.document.identity(), publication.document.id());
}

/// SQLite file stores under a test clock, opened anew by each node of a
/// law.
struct Files {
    dir: tempfile::TempDir,
    clock: Arc<lash_core::testing::TestClock>,
}

impl Files {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("a SQLite store directory"),
            clock: Arc::new(lash_core::testing::TestClock::new(1_700_000_000_000)),
        }
    }

    async fn open(&self) -> Arc<lash_sqlite_store::SqliteStoreSet> {
        Arc::new(
            lash_sqlite_store::SqliteStoreSet::open_with_clock(
                self.dir.path().join("lash.db"),
                lash_sqlite_store::SqliteSynchronous::Normal,
                self.clock.clone(),
            )
            .await
            .expect("open the SQLite file stores"),
        )
    }
}

/// The echo component's id, as a refusal names it.
fn echo_id() -> String {
    ECHO_TOOLS.component().to_owned()
}

/// RETIRED-SEALED (FIG-6296): a process sealed under an env that names
/// the echo component, which contributed none of its effects, parks on a
/// node that installs it. A node that declares the component retired
/// claims the parked process, builds its env without the component, says
/// so on the env view, and resumes the run to its answer.
#[tokio::test]
async fn a_sealed_env_naming_a_retired_component_builds_without_it_and_resumes() {
    let files = Files::new();
    let first = core_over(files.open().await, "first").await;
    let artifacts = first.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let document = lash::workflow::document::parse_document(GUARDED).expect("the document parses");
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &Name::new("guarded"),
                &environment,
                None,
            )
            .await
            .expect("the publication answers"),
    );
    let sealed = linking(&environment, &[]);
    assert!(
        sealed
            .components()
            .iter()
            .any(|entry| entry.id().as_str() == ECHO_TOOLS.component()),
        "the sealed env names the echo component"
    );
    let env_ref = artifacts
        .publish_process_env(&pin, &sealed)
        .await
        .expect("publish the process environment");
    let process = start(
        &first,
        &env_ref,
        &publication.definition,
        "retired",
        "operator",
    )
    .await;
    let first_due = timer_park_after(&first, &process, 0).await;
    first.shutdown().await.expect("the first node stops");
    drop(first);

    files.clock.set(first_due);
    let second =
        core_with_process_tools(files.open().await, "second", true, EchoComponent::Retired).await;
    let second_due = timer_park_after(&second, &process, first_due).await;
    files.clock.set(second_due);
    assert_eq!(
        finished(&second, &process).await,
        serde_json::json!("operator")
    );
    let view = second
        .processes()
        .environment(&process)
        .await
        .expect("the env view reads")
        .expect("the process is retained");
    let lash::process::ProcessAuthority::Engine { retired, .. } = view.authority else {
        panic!("a kernel process has an env");
    };
    assert_eq!(
        retired
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect::<Vec<_>>(),
        [echo_id()],
        "the env view names the component the process built without"
    );
    second.shutdown().await.expect("the second node stops");
}

/// RETIRED-NEW (FIG-6296): a new env may not name a retired component.
#[tokio::test]
async fn a_new_env_naming_a_retired_component_is_refused_typed() {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    let core = core_with_process_tools(stores, "retired", true, EchoComponent::Retired).await;
    let refused = core
        .host_artifacts()
        .publish_process_env(
            &lash::process::HostArtifactPin::mint(),
            &linking(&environment(true), &[]),
        )
        .await
        .expect_err("the retired component refuses the new env");
    assert_eq!(
        refused.process_env_refusal(),
        Some(&lash::process::ProcessEnvRefusal::ComponentRetired { id: echo_id() })
    );
    core.shutdown().await.expect("shutdown");
}

/// RETIRED-USED (FIG-6296): a process whose sealed env links `tools.echo`
/// to the echo component performs it and parks. On a node that declares
/// the component retired, the effect would have no body, so the process
/// parks with the typed refusal before any step resumes, and nothing
/// settles.
#[tokio::test]
async fn a_sealed_env_linking_a_retired_components_effect_parks_with_the_typed_refusal() {
    let files = Files::new();
    let first = core_over(files.open().await, "first").await;
    let artifacts = first.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let document = lash::workflow::document::parse_document(ECHOED).expect("the document parses");
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &Name::new("echoed"),
                &environment,
                None,
            )
            .await
            .expect("the publication answers"),
    );
    let env_ref = artifacts
        .publish_process_env(&pin, &linking(&environment, &["tools.echo"]))
        .await
        .expect("publish the process environment");
    let process = start(&first, &env_ref, &publication.definition, "used", "echoed").await;
    let due = timer_park_after(&first, &process, 0).await;
    first.shutdown().await.expect("the first node stops");
    drop(first);

    files.clock.set(due);
    let second =
        core_with_process_tools(files.open().await, "second", true, EchoComponent::Retired).await;
    let observed = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            let observed = second
                .processes()
                .get(&process)
                .await
                .expect("the process reads")
                .expect("the process is retained");
            if matches!(observed.park, lash_core::ProcessParkState::Parked(_)) {
                break observed;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the process parks within two minutes");
    assert_eq!(
        observed.park,
        lash_core::ProcessParkState::Parked(lash_core::ProcessParkReason::EnvRefused {
            refusal: lash::process::ProcessEnvRefusal::RetiredComponentServes {
                id: echo_id(),
                effect: "tools.echo".to_owned(),
            },
        })
    );
    assert!(
        observed.lifecycle.terminal().is_none(),
        "nothing settled: {observed:?}"
    );
    second.shutdown().await.expect("the second node stops");
}

/// FIG-6215: a multithreaded host can await workflow import in a Send future.
#[test]
fn workflow_import_future_is_send() {
    fn assert_send<T: Send>(_: T) {}

    let _: fn(&lash::process::HostArtifacts, &lash::process::ProcessEnv) =
        |artifacts, environment| {
            assert_send(artifacts.import_workflow(
                lash::workflow::document::SourceLanguage::Python,
                "async def main():\n    return None\n",
                environment,
                Some(lash::dialect::workflow::WorkflowSourceLimits {
                    max_bytes: 1024,
                    max_nesting: 16,
                }),
            ));
        };
}

/// MODULE-CONTRACT (FIG-6380): TypeScript and Python modules import through
/// one host call under one contract. Twin sources declare the same entries,
/// at the top level only and with no process tools offered, and break each
/// rule with the same refusal code; a successful import answers its source
/// map, and a front-end defect keeps its byte span. TypeScript's
/// `process(...)` wrapper, which main imported as an entry, is not workflow
/// syntax.
#[tokio::test]
async fn typescript_and_python_modules_share_one_entry_contract() {
    use lash::dialect::workflow::ModuleRefusal;
    use lash::workflow::WorkflowImport;
    use lash::workflow::document::SourceLanguage;
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores"),
    );
    let core = core_with_process_tools(stores, "import", false, EchoComponent::Installed).await;
    let artifacts = core.host_artifacts();
    let environment = environment(false);
    let import = |language, source: &'static str| {
        let artifacts = artifacts.clone();
        let environment = environment.clone();
        async move {
            artifacts
                .import_workflow(language, source, &environment, None)
                .await
                .expect("import answers")
        }
    };
    let entries = |imported: WorkflowImport, source: &str| {
        let WorkflowImport::Imported {
            document,
            source_map,
            source: text,
        } = imported
        else {
            panic!("the module imports: {imported:?}\n{source}");
        };
        assert_eq!(text, source);
        let document = document.to_document().expect("explicit typed import read");
        source_map
            .validate(&document, &text)
            .expect("the import maps its source");
        document
            .entries
            .keys()
            .map(|name| name.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    let code = |imported: WorkflowImport, source: &str| match imported {
        WorkflowImport::Refused(diagnostics) => diagnostics[0].code.clone(),
        other => panic!("the module is refused: {other:?}\n{source}"),
    };

    let typescript = "export async function first(inbox: string) { return inbox; }\nconst second = async (n: number) => n * 2;\nif (second) {\n  async function inBlock() { return 1; }\n}\n";
    let python = "async def first(inbox: str):\n    return inbox\n\n\nasync def second(n: int):\n    return n * 2\n\n\nif True:\n    async def in_block():\n        return 1\n";
    assert_eq!(
        entries(
            import(SourceLanguage::TypeScript, typescript).await,
            typescript
        ),
        ["first", "second"]
    );
    assert_eq!(
        entries(import(SourceLanguage::Python, python).await, python),
        ["first", "second"]
    );

    for (typescript, python, refusal) in [
        (
            "export async function a() { return 1; }\nexport async function a() { return 2; }",
            "async def a():\n    return 1\n\n\nasync def a():\n    return 2\n",
            ModuleRefusal::DuplicateEntry,
        ),
        (
            "if (true) {\n  const a = async () => 1;\n}\n",
            "if True:\n    async def a():\n        return 1\n",
            ModuleRefusal::EmptyModule,
        ),
        (
            "const limit = 3;\nexport async function a() { return limit; }",
            "limit = 3\n\n\nasync def a():\n    return limit\n",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "function helper() { return 1; }\nexport async function a() { return helper(); }",
            "def helper():\n    return 1\n\n\nasync def a():\n    return helper()\n",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "export async function b() { return 1; }\nexport async function a() { return await b(); }",
            "async def b():\n    return 1\n\n\nasync def a():\n    return await b()\n",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "export async function a(f: () => number) { return 1; }",
            "async def a(f: Callable[[], int]):\n    return 1\n",
            ModuleRefusal::UnmappableEntryAnnotation,
        ),
        (
            "export async function a(limit: number = 3) { return limit; }",
            "async def a(limit: int = 3):\n    return limit\n",
            ModuleRefusal::EntryDefault,
        ),
    ] {
        assert_eq!(
            code(
                import(SourceLanguage::TypeScript, typescript).await,
                typescript
            ),
            refusal.code(),
            "{typescript}"
        );
        assert_eq!(
            code(import(SourceLanguage::Python, python).await, python),
            refusal.code(),
            "{python}"
        );
    }

    let wrapped = "export const digest = process(async (inbox: string) => inbox);";
    assert_eq!(
        code(import(SourceLanguage::TypeScript, wrapped).await, wrapped),
        "TS_UNKNOWN_BINDING"
    );

    for (language, source) in [
        (SourceLanguage::TypeScript, "export const broken = ;"),
        (SourceLanguage::Python, "async def broken(:\n    return 1\n"),
    ] {
        let WorkflowImport::Refused(diagnostics) = import(language, source).await else {
            panic!("a syntax error is refused: {source}");
        };
        let span = diagnostics[0].span.expect("the syntax error has a span");
        assert!(
            span.start <= span.end && span.end <= source.len(),
            "{span:?}"
        );
    }
    core.shutdown().await.expect("the node stops");
}

/// FIG-6009: printed source retains its stated effects; a missing tool is
/// a checker admission refusal rather than a parsing diagnostic.
#[cfg(feature = "typescript")]
#[tokio::test]
async fn typescript_import_reports_missing_tools_as_admission_refusals() {
    use lash::workflow::{WorkflowAdmissionRefusal, WorkflowImport};
    let core = core().await;
    let document = lash::workflow::document::parse_document(
        "kernel 1\nnumbers float\neffect missing.tool(input: Any) -> Any\nentry job() -> Any\nfn job() { return null }\nmain { finish null }\n",
    ).expect("document parses");
    let source = lash::typescript::print(&document, Default::default())
        .expect("the printer accepts the document");
    let decoded = lash::typescript::read(&source.source, &document, Default::default())
        .expect("exported source reads back before admission");
    assert_eq!(document.canonical_bytes(), decoded.canonical_bytes());
    let WorkflowImport::Admission(WorkflowAdmissionRefusal::Document(refusal)) = core
        .host_artifacts()
        .import_exported_workflow(
            &source.source,
            lash::dialect::SourceFormat::TypeScriptV1,
            &document,
            &environment(true),
        )
        .await
        .expect("import answers")
    else {
        panic!("the unavailable effect is refused by admission");
    };
    assert!(refusal.errors.iter().any(|error| matches!(
        error.reason,
        lash::workflow::graph::RefusalReason::MissingEffect { .. }
    )));
    core.shutdown().await.expect("the node stops");
}

#[cfg(all(feature = "typescript", feature = "python"))]
#[path = "workflow_laws/generated_source.rs"]
mod generated_source;
#[path = "workflow_laws/node_spans.rs"]
#[cfg(all(feature = "testing", feature = "otel-trace"))]
mod node_spans;

#[path = "workflow_laws/documents.rs"]
mod documents;

#[path = "workflow_laws/coarse.rs"]
mod coarse;

#[cfg(feature = "typescript")]
#[path = "workflow_laws/crossing.rs"]
mod crossing;

#[path = "workflow_laws/inline_starts.rs"]
mod inline_starts;
