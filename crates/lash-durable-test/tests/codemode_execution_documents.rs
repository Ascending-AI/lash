//! Law L1 on the production paths (FIG-3571, put in terms of the workflow
//! document by FIG-5576, on kernel documents by FIG-5715), on the durable
//! engine.
//!
//! A turn's cell runs on a served core with a trace sink, and the process
//! the cell starts runs on that core's node. In both, `ExecutionStarted`
//! names a document a host reads back by that reference alone, and every
//! site the run reports is an execution site of that document. Cell functions
//! cross into a process as a separate document when a start is declared.

#![allow(clippy::expect_used, clippy::unwrap_used)]
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;
#[path = "support/web_fetch.rs"]
mod web_fetch;

#[path = "support/packages.rs"]
mod packages;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lash::tracing::{TraceLanguageExecution, TraceLanguageExecutionPayload};

use lash::workflow::{WorkflowDocument, WorkflowDocumentRead};
use lash_core::facade_support::{TraceRecord, TraceSink, TraceSinkError};
use lash_protocol_types::sync::MutexExt as _;

use served::{Tier, World};
use web_fetch::Fetch;

/// Every language-execution and step-body record the core traced.
#[derive(Default)]
struct RecordingSink(Mutex<Vec<TraceRecord>>);

impl TraceSink for RecordingSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        if matches!(
            &record.event,
            lash_core::TraceEvent::LanguageExecution { .. }
                | lash_core::TraceEvent::StepBodyStarted { .. }
        ) {
            self.0.lock_recover().push(record.clone());
        }
        Ok(())
    }
}

impl RecordingSink {
    fn records(&self) -> Vec<TraceRecord> {
        self.0.lock_recover().clone()
    }
}

/// The component that serves the law's tools to a process body.
const TOOLS_COMPONENT: lash_core::ProcessConfigKey<()> = lash_core::ProcessConfigKey::new(
    "codemode-document-tools",
    lash_vocabulary::providers::FormatVersion::ONE,
);

/// A core running RLM turns on `workers`, with the session process
/// controls, `tools`, and `sink` tracing every record. A process a cell
/// starts runs under the turn's run with `tools`' component.
async fn world_with_tools(
    tier: Tier,
    workers: lash::vm::WorkerService,
    sink: &Arc<RecordingSink>,
    tools: Arc<dyn lash_core::ToolProvider>,
) -> Option<World> {
    let sink = Arc::clone(sink);
    World::new(tier, move |backend| {
        lash::LashCore::rlm_builder(backend.clone(), served::rlm(backend, workers))
            .plugin(Arc::new(
                lash::process_controls::SessionProcessAdminPluginFactory::new(
                    lash_execution_contracts::process::lifetime::session_or_starter,
                    packages::run_policy_naming(&[TOOLS_COMPONENT]),
                ),
            ))
            .process_component(
                lash_core::testing::tool_component(TOOLS_COMPONENT, Arc::clone(&tools)),
                TOOLS_COMPONENT,
            )
            .plugin(lash_core::testing::tool_plugin(TOOLS_COMPONENT, tools))
            .trace_sink(sink)
    })
    .await
}

fn language_events(records: &[TraceRecord]) -> Vec<&TraceLanguageExecution> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            lash_core::TraceEvent::LanguageExecution { event, .. } => Some(event),
            _ => None,
        })
        .collect()
}

/// Each execution's start, with the document it names.
fn execution_starts<'r>(
    events: &[&'r TraceLanguageExecution],
) -> Vec<(&'r TraceLanguageExecution, &'r lash::package::LinkedStart)> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            TraceLanguageExecutionPayload::ExecutionStarted { .. } => {
                match &event.identity.document {
                    lash::tracing::TraceExecutionCode::Start(start) => Some((*event, start)),
                    lash::tracing::TraceExecutionCode::Callable(_) => None,
                }
            }
            _ => None,
        })
        .collect()
}

/// The exact sites the execution `identity` names reported on.
fn emitted_sites(
    events: &[&TraceLanguageExecution],
    started: &TraceLanguageExecution,
) -> BTreeSet<lash::workflow::document::PositionLabel> {
    events
        .iter()
        .filter(|event| event.identity == started.identity && event.subject() == started.subject())
        .filter_map(|event| event.payload.at())
        .map(|at| at.position())
        .collect()
}

/// Whether the execution that began at `started` reported its end.
fn finished(events: &[&TraceLanguageExecution], started: &TraceLanguageExecution) -> bool {
    events.iter().any(|event| {
        event.identity == started.identity
            && event.subject() == started.subject()
            && matches!(
                event.payload,
                TraceLanguageExecutionPayload::ExecutionFinished { .. }
            )
    })
}

/// A `web.fetch` that reads, while the cell that called it is still running,
/// the document that cell's start named. It borrows the law's core: the core
/// owns this tool, so the tool never owns the core.
#[derive(Default)]
struct ReadingFetch {
    sink: Arc<RecordingSink>,
    core: std::sync::OnceLock<std::sync::Weak<lash::LashCore>>,
    read: Mutex<Vec<(lash::package::LinkedStart, Option<WorkflowDocument>)>>,
    /// The same reference read from the store alone, with no engine and no
    /// registry: what a fresh reader of the retained document holds.
    retained: Mutex<Vec<Option<WorkflowDocument>>>,
    contexts: Mutex<Vec<lash::workflow::view::ViewContext>>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ReadingFetch {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(vec![web_fetch::fetch_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        lash_core::ToolProvider::resolve_contract(
            &Fetch,
            name,
            &lash_core::OwnerConfigView::default(),
        )
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let records = self.sink.records();
        let reference = execution_starts(&language_events(&records))
            .into_iter()
            .map(|(_, reference)| reference.clone())
            .next_back()
            .expect("the calling run started");
        let core = self
            .core
            .get()
            .and_then(std::sync::Weak::upgrade)
            .expect("the law's core outlives its calls");
        let document = match core
            .host_artifacts()
            .execution_document(&reference)
            .await
            .expect("the document read answers")
        {
            WorkflowDocumentRead::Read(read) => Some(*read),
            _ => None,
        };
        let retained = match lash_vm_runtime::KernelDocuments::new(
            core.backend().module_artifacts(),
            core.backend().package_store(),
            std::sync::Arc::clone(
                lash_vm_client::packages::WorkerLibrary::built()
                    .unwrap()
                    .helpers(),
            ),
        )
        .execution_document(&reference)
        .await
        {
            Ok(document) => Some(document),
            Err(lash_execution_contracts::process::linking::EnginePortError::Missing {
                ..
            }) => None,
            Err(error) => panic!("the stored cell reads: {error:?}"),
        };
        let execution = call
            .context
            .execution_environment()
            .expect("the admitted cell's environment");
        let publications = &execution.publications;
        let context = core
            .host_artifacts()
            .workflow_view_context(retained.as_ref().expect("retained document"), publications)
            .await
            .expect("retained display context");
        self.contexts.lock_recover().push(context);
        self.retained.lock_recover().push(retained);
        self.read.lock_recover().push((reference, document));
        lash_core::ToolProvider::execute(&Fetch, call).await
    }
}

/// The law's world, its reading tool, and the law's own handle on the core
/// the tool borrows; the law holds it until the world is shut down.
async fn reading_world(
    sink: &Arc<RecordingSink>,
) -> (served::World, Arc<ReadingFetch>, Arc<lash::LashCore>) {
    let tools = Arc::new(ReadingFetch {
        sink: Arc::clone(sink),
        ..ReadingFetch::default()
    });
    let world = world_with_tools(
        Tier::SqliteMemory,
        sim::untimed_workers(),
        sink,
        tools.clone(),
    )
    .await
    .expect("SQLite memory is available");
    let core = Arc::new(world.core.clone());
    assert!(tools.core.set(Arc::downgrade(&core)).is_ok());
    (world, tools, core)
}

/// FIG-5576: a cell's start names a document a host can read while the cell
/// runs, as `main` of it, and every call the cell reports stands at an
/// execution site of that document, from its start to its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_cell_names_a_document_readable_while_it_runs_and_reports_its_calls_at_its_sites() {
    let sink = Arc::new(RecordingSink::default());
    let (world, tools, core) = reading_world(&sink).await;
    let output = world
        .run(
            "cell-document",
            served::spec(8),
            vec![served::cell(
                r#"
const fetched = await web.fetch({ url: "only" });
await control.finish(fetched.url);
"#,
            )],
        )
        .await;
    served::assert_answered("the cell document", &output);
    let read = tools.read.lock_recover().clone();
    let [(reference, document)] = read.as_slice() else {
        panic!("the cell's one fetch read its document: {read:?}");
    };
    assert_eq!(reference.selector, lash::package::StartSelector::Main);
    let document = document
        .as_ref()
        .expect("a running cell's document is readable by the reference its start named");
    let records = sink.records();
    let events = language_events(&records);
    let [(started, _)] = execution_starts(&events)[..] else {
        panic!("one cell started");
    };
    let emitted = emitted_sites(&events, started);
    let index = lash::workflow::workflow_overlay_document(document);
    assert_eq!(
        emitted.len(),
        2,
        "fetch and finish each name a site: {emitted:?}"
    );
    let output_tool_calls = world
        .core
        .session(lash_vocabulary::identity::SessionId::from("cell-document"))
        .durable()
        .await
        .unwrap()
        .read_turn_tool_calls(
            &output
                .activities
                .iter()
                .find_map(|activity| match &activity.event {
                    lash::TurnEvent::TurnStarted { turn_id, .. } => Some(turn_id.clone()),
                    _ => None,
                })
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        output_tool_calls
            .iter()
            .map(|call| call.tool.as_str())
            .collect::<Vec<_>>(),
        ["web.fetch", "control.finish"]
    );
    for call in &output_tool_calls {
        let sites = events
            .iter()
            .filter_map(|event| match &event.payload {
                TraceLanguageExecutionPayload::Node {
                    at,
                    fact:
                        lash::tracing::TraceNodeFact::Started {
                            call_id: Some(call_id),
                        },
                    ..
                } if event.identity == started.identity
                    && event.subject() == started.subject()
                    && call_id == &call.call_id =>
                {
                    Some(at.position())
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(sites.len(), 1, "{} names one execution site", call.tool);
        assert!(sites.is_subset(&emitted));
    }
    assert!(
        emitted.iter().all(|site| index.contains(site)),
        "every site the cell reported is in its document"
    );
    assert!(finished(&events, started), "the cell reported its end");
    world.shutdown().await;
    drop(core);
    let provider = Arc::downgrade(&tools);
    drop(tools);
    assert!(
        provider.upgrade().is_none(),
        "the law's tool is released with its world: the core it reads through never keeps it"
    );
}

/// DOC-CELL (FIG-6379): a cell's document is retained with the declarations
/// of the helpers it was lowered against, so a fresh reader, holding no
/// engine and no registry, derives the cell's `main` view from the store
/// alone, as the producing engine draws it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_retained_cell_draws_its_main_view_without_the_producing_engines_registry() {
    let sink = Arc::new(RecordingSink::default());
    let (world, tools, core) = reading_world(&sink).await;
    let output = world
        .run(
            "cell-retained",
            served::spec(8),
            vec![served::cell(
                r#"
const urls = ["only"].map((url) => url);
const fetched = await web.fetch({ url: urls[0] });
await control.finish(fetched.url);
"#,
            )],
        )
        .await;
    served::assert_answered("the retained cell", &output);
    let read = tools.read.lock_recover().clone();
    let [(_, Some(drawn))] = read.as_slice() else {
        panic!("the producing engine read the cell's document: {read:?}");
    };
    let retained = tools.retained.lock_recover().clone();
    let [Some(retained)] = retained.as_slice() else {
        panic!("the store alone reads the retained cell's document: {retained:?}");
    };
    assert_eq!(
        retained.selector().expect("linked start"),
        &lash::package::StartSelector::Main
    );
    assert!(
        !retained.declarations().is_empty(),
        "the cell's helpers are retained beside it"
    );
    assert_eq!(
        retained.declarations().keys().collect::<Vec<_>>(),
        retained.document().functions.keys().collect::<Vec<_>>()
    );
    let context = tools.contexts.lock_recover()[0].clone();
    assert_eq!(
        lash::workflow::view::derive(retained, &context).expect("the retained cell draws"),
        lash::workflow::view::derive(drawn, &context).expect("the engine's read draws"),
    );
    world.shutdown().await;
    drop(core);
}
