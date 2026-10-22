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

use lash::workflow::WorkflowRead;
use lash::workflow::authoring::{DataValue, Operation, ValueTarget};
use lash::workflow::document::{Document, Expr, Literal, Name, Place, Rhs, Stmt, Unit};
use lash::workflow::edit::{Draft, Transaction};
#[path = "workflow_laws/support.rs"]
mod support;
use support::*;

/// `guarded(name)`: binds `out` inside a `try`, sleeps once in each turn of
/// a nested loop, and returns `out`. Written as a kernel document: no
/// source language is involved anywhere in these laws.
const GUARDED: &str = r#"function guarded(name: Text) -> Text
kernel 1
metering statements
import 0 operation typescript.sleep(milliseconds: Int) -> Null
body {
  let out = "start"
  try { set out = name } catch e { set out = "caught" }
  for outer in [1, 2] {
    for inner in [1] { do perform 0(5) as Null }
  }
  return out
}
"#;

fn guarded(source: &str) -> Document {
    let definition = lash_kernel_doc::parse_definition(source).unwrap();
    let mut document = Document::new(lash::workflow::document::NumberPolicy::Float, Vec::new());
    document.imports = definition.body().unwrap().imports.clone();
    document.exports.insert(
        Name::new("guarded"),
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

fn function_unit(document: &Document, name: &str) -> Unit {
    let lash::workflow::document::Export::Function(export) = &document.exports[&Name::new(name)]
    else {
        panic!("function export")
    };
    Unit::Library(export.function)
}

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
    vec![
        lash::tools::ToolDefinition::raw(
            "tool:echo",
            "tools.echo",
            "Answers its value.",
            serde_json::json!({"type": "object"}),
            serde_json::json!({}),
        )
        .expect("tool schema")
        // The contract the laws' kernel texts pin.
        .declared_contract(lash_vocabulary::operations::ContractId::new("1"))
        .with_execution(std::time::Duration::from_secs(120)),
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
            .with_tool_provider(lash::tool_provider_key!("tools"), echo_tool())
            .with_package_provider(
                lash::tool_provider_key!("tools"),
                Arc::new(lash::package::ToolPackageProvider::new(
                    ECHO_TOOLS.component(),
                    lash::tool_provider_key!("tools"),
                    [lash::package::Segment::new("tools").unwrap()].into(),
                    echo_tool(),
                )),
            ),
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
    stores: Arc<dyn lash_execution_contracts::backend::StoreSet>,
    process_tools: bool,
    echo: EchoComponent,
    workers: lash::vm::WorkerService,
) -> lash::LashCoreBuilder {
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    let backend = lash_conformance::backend_over(stores);
    let bounds = lash::vm::RunBounds {
        charge_limit: 1_000_000,
        memory: 64 * 1024 * 1024,
        ..workers.config().run_bounds
    };
    let builder = lash::LashCore::standard_builder(backend)
        .process_component(Arc::new(workflow_services()), WORKFLOW_SERVICES)
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
                    lash_execution_contracts::process::lifetime::session_or_starter,
                    lash::process::env::policy::from_run(|_| Ok(Vec::new())),
                ),
            ))
            .process_component(
                Arc::new(lash::process_controls::ProcessControlsComponent::new(
                    lash_execution_contracts::process::lifetime::session_or_starter,
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
        .component(ECHO_TOOLS, &())
        .component(WORKFLOW_SERVICES, &());

    if process_tools {
        builder = builder.component(lash::process_controls::PROCESS_CONTROLS_CONFIG, &());
    }
    builder.build().expect("the environment builds")
}

/// `environment` restricted to `effects`: the env a process that performs
/// exactly those effects starts under (LINK-EXACTLY).
async fn publication_document(
    core: &lash::LashCore,
    publication: &Publication,
) -> lash::workflow::WorkflowDocument {
    let lash::workflow::WorkflowDocumentRead::Read(read) = core
        .host_artifacts()
        .execution_document(&publication.start)
        .await
        .expect("read published document")
    else {
        panic!("the publication pin retains the document");
    };
    (*read).clone()
}

// The kernel-text example in `lash::workflow::document`'s module docs is
// `src/workflow/document_example.rs`, included into the rustdoc verbatim;
// this test is its compile-and-run proof.
include!("../src/workflow/document_example.rs");

#[test]
fn the_document_module_docs_example_runs() {
    document_example();
}

/// Starts `definition` as `guarded(name)` and answers the process.
async fn start(
    core: &lash::LashCore,
    env_ref: &lash_core::ProcessEnvRef,
    definition: &lash_execution_contracts::process::ProcessDefinition,
    key: &str,
    name: &str,
) -> lash_vocabulary::identity::ProcessId {
    let mut args = serde_json::Map::new();
    args.insert("name".to_owned(), serde_json::json!(name));
    start_with(core, env_ref, definition, key, args).await
}

async fn start_with(
    core: &lash::LashCore,
    env_ref: &lash_core::ProcessEnvRef,
    definition: &lash_execution_contracts::process::ProcessDefinition,
    key: &str,
    args: serde_json::Map<String, serde_json::Value>,
) -> lash_vocabulary::identity::ProcessId {
    core.processes()
        .start(
            lash_execution_contracts::process::ProcessStartRequest::definition(
                definition.id.clone(),
                Some(definition.signature.clone()),
                args,
                lash_execution_contracts::process::EngineLaunch::new(
                    env_ref.clone(),
                    core.processes()
                        .engine_defaults(
                            lash::process::LASH_VM_ENGINE_KIND,
                            &serde_json::Value::Null,
                        )
                        .expect("the kernel states its settings"),
                ),
                lash_execution_contracts::process::ProcessOriginator::host(),
                lash_execution_contracts::process::LifetimeDecision::Detached,
            )
            .with_host_start_key(key),
            core.effect_host(),
        )
        .await
        .expect("the process starts")
        .process_id
}

/// What the process finished with.
async fn finished(
    core: &lash::LashCore,
    process_id: &lash_vocabulary::identity::ProcessId,
) -> serde_json::Value {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        core.processes().await_output(process_id),
    )
    .await
    .expect("the process settles")
    .expect("the output reads");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
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
        .workflow_environment(&namespace(&core, &pin, &environment).await.1)
        .await
        .expect("the environment reads")
        .expect("the kernel engine reads workflow documents");
    let entry = Name::new("guarded");

    let document = guarded(&GUARDED.replace("set out = name", "set out = \"operator\""));
    let first = publish(
        &core,
        &pin,
        &lash::workflow::document::DocumentBytes::from(document.clone()),
        &entry,
        &environment,
        None,
    )
    .await
    .expect("the publication answers");
    let first_document = publication_document(&core, &first).await;
    assert_eq!(first_document.document(), &document);
    assert_eq!(
        first.start.selector,
        lash::package::StartSelector::Export(entry.clone())
    );

    let WorkflowRead::Inspected(inspection) = artifacts
        .definition_graph(&first.definition.id)
        .await
        .expect("the definition reads")
    else {
        panic!("a published workflow has a document");
    };
    assert_eq!(inspection.definition, first.definition);
    assert_eq!(inspection.document.document(), first_document.document());

    // The view names the one wait of the body inside both loops.
    let graph = inspection.document.graph();
    let unit = function_unit(&document, "guarded");
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
    assert_ne!(applied.admitted.identity, first.document);
    let second = publish(
        &core,
        &pin,
        &lash::workflow::document::DocumentBytes::from(draft.document().clone()),
        &entry,
        &environment,
        None,
    )
    .await
    .expect("the publication answers");
    assert_ne!(second.definition.id, first.definition.id);
    assert_eq!(second.document, applied.admitted.identity);
    let survivor = applied
        .correspondence
        .survivor(&sleep.site)
        .expect("the wait survives the edit");
    assert!(!survivor.edited);
    assert_eq!(survivor.to.path, sleep.site.path);

    // The first definition is immutable: a process of it runs the document
    // it was admitted with, beside a process of the edited one.
    let env_ref = first.env_ref.clone();
    let kept = start(&core, &env_ref, &first.definition, "first", "operator").await;
    let edited = start(
        &core,
        &second.env_ref,
        &second.definition,
        "second",
        "operator",
    )
    .await;
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
    assert_eq!(kept_read.document.document(), first_document.document());
    assert_eq!(kept_read.definition, first.definition);
}

fn is_perform(node: lash::workflow::document::Node<'_>, slot: u32) -> bool {
    use lash::workflow::document::{ActKind, Action, Expr, Node};
    match node {
        Node::Action(Action::Perform { slot: found, .. }) => found.0 == slot,
        Node::Expr(Expr::Act(act)) => {
            matches!(act.kind,ActKind::Perform {slot:found,..} if found.0 == slot)
        }
        _ => false,
    }
}

/// FIG-6323: every sleep in a loop names its own execution occurrence in
/// the committed process log, so a host can highlight it without tracing.
#[tokio::test]
async fn stored_sleep_waits_name_the_sleep_site_and_each_loop_occurrence() {
    let core = core().await;

    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let source = GUARDED.replace("do perform 0(5) as Null",
        "do perform 0(6000) as Null\n      let input = {value: name}\n      do perform 1(input) as Any");
    let document =
        guarded(&source.replace("body {", &format!("{}\nbody {{", echo_import_header(1))));
    let publication = publish(
        &core,
        &pin,
        &lash::workflow::document::DocumentBytes::from(document.clone()),
        &Name::new("guarded"),
        &environment,
        None,
    )
    .await
    .expect("publication");
    let publication_view = publication_document(&core, &publication).await;
    let sites = publication_view.graph().execution_sites();
    let sleep = sites
        .iter()
        .find(|site| {
            document
                .node(&site.site)
                .is_some_and(|node| is_perform(node, 0))
        })
        .expect("sleep");
    let tool = sites
        .iter()
        .find(|site| {
            document
                .node(&site.site)
                .is_some_and(|node| is_perform(node, 1))
        })
        .expect("tool");
    let env_ref = publication.env_ref.clone();
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
        assert_eq!(at.at.position(), sleep.site);
        assert_eq!(
            at.at.visit(),
            lash::workflow::document::testkit::visit(occurrence as u64)
        );
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
            assert_eq!(
                row.occurrences,
                lash_vocabulary::sites::ObservationReader::counts(at).starts
            );
            let lash::process::WaitKind::Call {
                resume_at_ms: Some(until_ms),
                ..
            } = wait.kind
            else {
                panic!("the loop waits on its sleep: {wait:?}");
            };
            assert_eq!(
                row.state,
                lash::workflow::ProcessWorkflowSiteState::Waiting {
                    kind: lash::workflow::ProcessWorkflowWaitKind::Effect,
                    resume_at_ms: Some(until_ms),
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
            .find(|row| &row.at.position() == site)
            .expect("site in stored fold");
        assert_eq!(row.occurrences, 2);
        let entered = events
            .iter()
            .find(|event| match &event.fact {
                lash::process::ProcessLifecycleFact::Waiting { wait } => {
                    wait.execution_site().is_ok_and(|at| &at.position() == site)
                }
                lash::process::ProcessLifecycleFact::EffectOutcome(outcome) => {
                    &outcome.at.position() == site
                }
                _ => false,
            })
            .expect("the occurrence has a committed entry");
        let settled = events
            .iter()
            .rev()
            .find(|event| match &event.fact {
                lash::process::ProcessLifecycleFact::EffectOutcome(outcome) => {
                    &outcome.at.position() == site
                }
                _ => false,
            })
            .expect("the ordinary operation's outcome settles its occurrence");
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
        let snapshot_row = snapshot_overlay
            .sites
            .iter()
            .find(|snapshot_row| snapshot_row.at == row.at)
            .expect("the snapshot retains the committed occurrence");
        assert_eq!(
            snapshot_row, &row,
            "snapshot retains the stored-event fold and the perform's authored origin"
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

    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let identity = lash_kernel_doc::parse_definition("function identity(value: Text) -> Text\nkernel 1\nmetering statements\nbody { return value }").unwrap();
    let computed = lash_kernel_doc::parse_definition(&format!(
        "function computed(name: Text) -> Text\nkernel 1\nmetering statements\nuse identity = @{}\nbody {{ for n in [1, 2] {{ let value = call identity(name) }} return name }}", identity.identity(),
    )).unwrap();
    let mut document = function_document(computed, "computed");
    document.functions.insert(identity.identity(), identity);
    let publication = publish(
        &core,
        &pin,
        &lash::workflow::document::DocumentBytes::from(document.clone()),
        &Name::new("computed"),
        &environment,
        None,
    )
    .await
    .expect("publication");
    let publication_view = publication_document(&core, &publication).await;
    let [call] = publication_view.graph().execution_sites() else {
        panic!("one call site");
    };
    let env_ref = publication.env_ref.clone();
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
                .observe(&publication.start, &event.payload)
                .expect("same execution");
            if let lash_trace::TraceLanguageExecutionPayload::Node {
                at,
                fact: lash_trace::TraceNodeFact::Started { .. },
                ..
            } = &observation.execution.payload
                && at.position() == call.site
            {
                entries.insert(at.visit());
                let state = overlay.snapshot();
                assert!(
                    state.current_sites.contains(at),
                    "the pure call stays current"
                );
                let [row] = state.sites.as_slice() else {
                    panic!("one site");
                };
                assert_eq!(
                    row.occurrences,
                    lash_vocabulary::sites::ObservationReader::counts(at).starts
                );
                assert_eq!(row.state, ProcessWorkflowSiteState::Running);
            }
            if entries.contains(&lash::workflow::document::testkit::visit(1)) {
                break;
            }
        }
    }
    assert_eq!(
        entries.last(),
        Some(&lash::workflow::document::testkit::visit(1)),
        "the last entry is named"
    );
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

    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let text = echo_import_header(0);
    let mut document = lash_kernel_doc::parse_document(&format!("kernel 1\nnumbers float\n{text}\nmain {{ let fan = fn(a,b,c) {{ let work = fn(item) {{ let input = {{value: item}} let got = perform 0(input) as Any return got }} let first = spawn apply work(a) let second = spawn apply work(b) let third = spawn apply work(c) let tasks = [first,second,third] let every = join all tasks return every }} }}")).unwrap();
    export_closure(
        &mut document,
        "fan",
        &["a", "b", "c"],
        lash::workflow::document::Type::Json,
    );
    let publication = publish(
        &core,
        &pin,
        &lash::workflow::document::DocumentBytes::from(document.clone()),
        &Name::new("fan"),
        &environment,
        None,
    )
    .await
    .expect("the publication answers");
    let publication_view = publication_document(&core, &publication).await;
    let performs: Vec<_> = publication_view
        .graph()
        .execution_sites()
        .iter()
        .filter(|site| site.kind == lash::workflow::graph::SiteKind::Perform)
        .collect();
    let [perform] = performs.as_slice() else {
        panic!("the document performs at one site: {performs:?}");
    };

    let env_ref = publication.env_ref.clone();
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
    overlay.set_document(lash::workflow::workflow_overlay_document(
        &publication_document(&core, &publication).await,
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
                WorkflowOverlayOccurrence::Completed { occurrence, .. } if occurrence == lash::workflow::document::testkit::visit(0)
            ),
            "each element ran the site once, as its occurrence 0: {row:#?}"
        );
        assert!(
            !lash::workflow::document::ObservationReader::task_is_root(&row.site.task),
            "an element's effect belongs to its spawned task: {row:#?}"
        );
    }
}

/// START-TEMPLATE: a reference to worker retains its defining Link and gets
/// its own launch after the original publication pin is gone.
#[tokio::test]
async fn a_process_starts_another_entry_of_its_document_by_function_reference_and_awaits_it() {
    let clock = Arc::new(lash_core::testing::TestClock::new(1_700_000_000_000));
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .expect("SQLite"),
    );
    let core = builder_with_process_tools(stores.clone(), false, EchoComponent::Installed)
        .await
        .process_component(
            Arc::new(lash::process_controls::ProcessControlsComponent::new(
                lash_execution_contracts::process::lifetime::session_or_starter,
                lash::process::env::policy::restrict_or(lash::process::env::policy::refuse(
                    "the child inherits its retained parent",
                )),
            )),
            lash::process_controls::PROCESS_CONTROLS_CONFIG,
        )
        .build(node_identity("start-template"))
        .expect("core");
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let mut imports = lash_kernel_doc::parse_document("kernel 1\nnumbers float\nimport 0 operation typescript.sleep(milliseconds: Int) -> Null\nmain {finish null}").unwrap();
    for (slot, tool) in [
        (1, lash::process_controls::ProcessControlTool::Start),
        (2, lash::process_controls::ProcessControlTool::Await),
    ] {
        let (slot, import) = operation_import(
            lash::package::ImportSlot(slot),
            &lash::process_controls::process_tool_definition(tool),
        );
        imports.imports.insert(slot, import);
    }
    let (slot, import) = operation_import(lash::package::ImportSlot(3), &echo_definitions()[0]);
    imports.imports.insert(slot, import);
    let header = lash_kernel_doc::print_document(&imports)
        .lines()
        .filter(|line| line.starts_with("import "))
        .collect::<Vec<_>>()
        .join("\n");
    let worker = lash_kernel_doc::parse_definition(&format!("function worker(value: Json) -> Json\nkernel 1\nmetering statements\n{header}\nbody {{ let request = {{value: value}} let echoed = perform 3(request) as Any return [echoed, \"worked\"] }}")).unwrap();
    let supervisor = lash_kernel_doc::parse_definition(&format!("function supervisor(value: Json) -> Json\nkernel 1\nmetering statements\n{header}\nuse worker = @{}\nbody {{ do perform 0(1000) as Null let args = {{value: value}} let request = {{definition: &worker,args: args}} let handle = perform 1(request) as Any let wait = {{handle: handle}} let done = perform 2(wait) as Any return done }}",worker.identity())).unwrap();
    let mut document = function_document(supervisor, "supervisor");
    document
        .exports
        .extend(function_document(worker.clone(), "worker").exports);
    document.functions.insert(worker.identity(), worker);
    let publication = publish(
        &core,
        &pin,
        &document.clone().into(),
        &Name::new("supervisor"),
        &environment,
        None,
    )
    .await
    .unwrap();
    let worker_draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
        code: lash::vm::KernelInlineRequest::Linked {
            start: lash::package::LinkedStart {
                link: publication.start.link,
                selector: lash::package::StartSelector::Export(Name::new("worker")),
            },
        },
    })
    .unwrap();
    let worker_id = worker_draft.id();
    assert!(
        artifacts
            .get_definition(&worker_id)
            .await
            .expect("read worker")
            .is_none(),
        "publication creates only the selected descriptor"
    );
    let env_ref = publication.env_ref.clone();
    let mut args = serde_json::Map::new();
    args.insert("value".to_owned(), serde_json::json!("x"));
    let supervisor = start_with(&core, &env_ref, &publication.definition, "supervisor", args).await;
    let until = timer_park_after(&core, &supervisor, 0).await;
    artifacts.release(pin).await.expect("release original pin");
    lash::testing::quiesce_artifact_cleanup(&core)
        .await
        .settle()
        .await
        .expect("pin cleanup");
    clock.set(until);
    let ended = finished(&core, &supervisor).await;
    assert!(
        ended.to_string().contains(r#"["x","worked"]"#),
        "the supervisor returns its child's result: {ended}"
    );
    assert!(
        artifacts
            .get_definition(&worker_id)
            .await
            .expect("read worker")
            .is_none(),
        "starting the extracted child publishes no sibling descriptor"
    );
    use lash_execution_contracts::process::ProcessQuery as _;
    let rows = stores
        .process_registry()
        .list_processes_page(
            &lash_execution_contracts::process::ProcessListFilter {
                status: lash_execution_contracts::process::ProcessStatusFilter::Any,
                ..Default::default()
            },
            std::num::NonZeroUsize::new(10).expect("limit"),
            None,
        )
        .await
        .expect("processes")
        .records;
    let child = rows.iter().find(|row| row.id != supervisor).expect("child");
    assert_eq!(
        finished(&core, &child.id).await,
        serde_json::json!(["x", "worked"])
    );
    let lash::process::ProcessInput::Engine { payload, .. } = child.input.as_ref() else {
        panic!("the child is an engine execution")
    };
    let code: lash::vm::KernelInlineRequest =
        serde_json::from_value(payload["code"].clone()).unwrap();
    let lash::vm::KernelInlineRequest::Callable { link, function, .. } = code else {
        panic!("a captured callable")
    };
    assert_eq!(
        link, publication.start.link,
        "the child retains its defining Link"
    );
    let content = lash::vm::KernelDocuments::new(
        core.backend().module_artifacts(),
        core.backend().package_store(),
    )
    .load_linked(link)
    .await
    .unwrap();
    assert_eq!(
        &content.documents[&content.links[&link].document],
        &document
    );
    assert!(document.functions.contains_key(&function.callable.function));
    core.shutdown().await.expect("stop the node");
}

/// Wait for a new durable timer park, reading only after process-feed events.
/// The injected store clock holds the timer until the host observed its park.
async fn timer_park_after(
    core: &lash::LashCore,
    process: &lash_vocabulary::identity::ProcessId,
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
                if let lash_execution_contracts::process::WaitKind::Call {
                    resume_at_ms: Some(until_ms),
                    ..
                } = wait.kind
                {
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

    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(true);
    let document = guarded(GUARDED);
    let publication = publish(
        &first,
        &pin,
        &lash::workflow::document::DocumentBytes::from(document.clone()),
        &Name::new("guarded"),
        &environment,
        None,
    )
    .await
    .expect("the publication answers");
    let env_ref = publication.env_ref.clone();
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
    assert_eq!(read.document.identity(), publication.document);
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
    let document = guarded(GUARDED);
    let publication = publish(
        &first,
        &pin,
        &lash::workflow::document::DocumentBytes::from(document.clone()),
        &Name::new("guarded"),
        &environment,
        None,
    )
    .await
    .expect("the publication answers");
    let sealed = publication.environment.clone();
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
        .publish_process_env(&lash::process::HostArtifactPin::mint(), &environment(true))
        .await
        .expect_err("the retired component refuses the new env");
    assert_eq!(
        refused.process_env_refusal(),
        Some(&lash::process::ProcessEnvRefusal::ComponentRetired { id: echo_id() })
    );
    core.shutdown().await.expect("shutdown");
}

/// FIG-6215: a multithreaded host can await workflow import in a Send future.
#[test]
fn workflow_import_future_is_send() {
    fn assert_send<T: Send>(_: T) {}

    let _: fn(&lash::process::HostArtifacts, &lash::vm::WorkflowEnvironmentRequest) =
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
    let environment = source_environment(false);
    let (_, namespace) =
        namespace(&core, &lash::process::HostArtifactPin::mint(), &environment).await;
    let import = |language, source: &'static str| {
        let artifacts = artifacts.clone();
        let namespace = namespace.clone();
        async move {
            artifacts
                .import_workflow(language, source, &namespace, None)
                .await
                .expect("import answers")
        }
    };
    let entries = |imported: WorkflowImport, source: &str| {
        let WorkflowImport::Imported {
            document,
            source_map,
            source: text,
            ..
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
            .exports
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
            "const value = 3;",
            "value = 3\n",
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

    for (language, source) in [
        (
            SourceLanguage::TypeScript,
            "export async function entry() { return 0; }\nif (true) { const a = async () => 1; }",
        ),
        (
            SourceLanguage::Python,
            "async def entry():\n    return 0\n\nif True:\n    async def a():\n        return 1\n",
        ),
    ] {
        assert!(
            entries(import(language, source).await, source) == ["entry"],
            "only the top-level function creates a public Export"
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
async fn a_missing_operation_is_refused_at_workflow_publication() {
    let core = core().await;
    let pin = lash::process::HostArtifactPin::mint();
    let document = lash_kernel_doc::parse_document("kernel 1\nnumbers float\nimport 0 operation missing.tool(input: Json) -> Json\nmain { do perform 0(null) as Json return null }").unwrap();
    let (_, request) = namespace(&core, &pin, &source_environment(false)).await;
    let error = core
        .host_artifacts()
        .publish_workflow(
            &pin,
            &document.into(),
            &lash::package::StartSelector::Main,
            &request,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error,lash::EmbedError::Plugin(lash_execution_contracts::PluginError::Package(cause)) if matches!(*cause,lash_execution_contracts::process::linking::EnginePortError::Refused(lash_execution_contracts::process::linking::AdmissionRefusal::MissingImport {slot:lash::package::ImportSlot(0),..})))
    );
    core.shutdown().await.unwrap();
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

#[cfg(feature = "typescript")]
#[path = "workflow_laws/packages.rs"]
mod packages;

#[cfg(all(feature = "typescript", feature = "python", feature = "workflow-view"))]
#[path = "workflow_laws/failures.rs"]
mod failures;

#[cfg(all(feature = "typescript", feature = "python", feature = "workflow-view"))]
#[path = "workflow_laws/returned_lists.rs"]
mod returned_lists;

#[cfg(all(feature = "typescript", feature = "python", feature = "workflow-view"))]
#[path = "workflow_laws/cancelled_sleep.rs"]
mod cancelled_sleep;

#[cfg(all(feature = "typescript", feature = "python", feature = "workflow-view"))]
#[path = "workflow_laws/sleep_resume_at.rs"]
mod sleep_resume_at;
