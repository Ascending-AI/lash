//! Law L1 on the production paths (FIG-3571, put in terms of the workflow
//! document by FIG-5576, on kernel documents by FIG-5715), on the durable
//! engine.
//!
//! A turn's cell runs on a served core with a trace sink, and the process
//! the cell starts runs on that core's node. In both, `ExecutionStarted`
//! names a document a host reads back by that reference alone, and every
//! site the run reports is an execution site of that document. A process a
//! cell wrote is an entry of the cell's own document.

#![allow(clippy::expect_used, clippy::unwrap_used)]
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;
#[path = "support/web_fetch.rs"]
mod web_fetch;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lash::tracing::{
    TraceLanguageExecution, TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload,
};
use lash::workflow::document::Site;
use lash::workflow::{
    WorkflowDocument, WorkflowDocumentEntry, WorkflowDocumentRead, WorkflowDocumentRef,
};
use lash_core::facade_support::{TraceRecord, TraceSink, TraceSinkError};
use lash_sansio::sync::MutexExt as _;

use served::{Tier, World};
use web_fetch::{Fetch, GrantFetch};

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

/// A core running RLM turns on `workers`, with the session process
/// controls, `tools`, and `sink` tracing every record.
async fn world_with_tools(
    tier: Tier,
    workers: lash::vm::WorkerService,
    sink: &Arc<RecordingSink>,
    tools: Arc<dyn lash_core::ToolProvider>,
) -> Option<World> {
    let sink = Arc::clone(sink);
    World::new(tier, move |backend| {
        lash::LashCore::rlm_builder(
            backend.clone(),
            served::rlm(backend, Some(Arc::new(GrantFetch)), workers),
        )
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .tools(tools)
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
) -> Vec<(&'r TraceLanguageExecution, &'r WorkflowDocumentRef)> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            TraceLanguageExecutionPayload::ExecutionStarted => {
                Some((*event, &event.identity.document))
            }
            _ => None,
        })
        .collect()
}

/// The exact sites the execution `identity` names reported on.
fn emitted_sites(
    events: &[&TraceLanguageExecution],
    identity: &TraceLanguageExecutionIdentity,
) -> BTreeSet<Site> {
    events
        .iter()
        .filter(|event| &event.identity == identity)
        .filter_map(|event| event.payload.at())
        .map(|at| at.site.clone())
        .collect()
}

/// Whether the execution `identity` names reported its end.
fn finished(events: &[&TraceLanguageExecution], identity: &TraceLanguageExecutionIdentity) -> bool {
    events.iter().any(|event| {
        &event.identity == identity
            && matches!(
                event.payload,
                TraceLanguageExecutionPayload::ExecutionFinished { .. }
            )
    })
}

/// A `web.fetch` that reads, while the cell that called it is still running,
/// the document that cell's start named.
#[derive(Default)]
struct ReadingFetch {
    sink: Arc<RecordingSink>,
    core: std::sync::OnceLock<lash::LashCore>,
    read: Mutex<Vec<(WorkflowDocumentRef, Option<WorkflowDocument>)>>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ReadingFetch {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![web_fetch::fetch_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        lash_core::ToolProvider::resolve_contract(&Fetch, name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let records = self.sink.records();
        let reference = execution_starts(&language_events(&records))
            .into_iter()
            .map(|(_, reference)| reference.clone())
            .next_back()
            .expect("the calling run started");
        let core = self.core.get().expect("the law's core");
        let document = match core
            .host_artifacts()
            .execution_document(&reference)
            .await
            .expect("the document read answers")
        {
            WorkflowDocumentRead::Read(document) => Some(*document),
            _ => None,
        };
        self.read.lock_recover().push((reference, document));
        lash_core::ToolProvider::execute(&Fetch, call).await
    }
}

async fn reading_world(sink: &Arc<RecordingSink>) -> (served::World, Arc<ReadingFetch>) {
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
    assert!(tools.core.set(world.core.clone()).is_ok());
    (world, tools)
}

/// FIG-5576: a cell's start names a document a host can read while the cell
/// runs, as `main` of it, and every call the cell reports stands at an
/// execution site of that document, from its start to its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_cell_names_a_document_readable_while_it_runs_and_reports_its_calls_at_its_sites() {
    let sink = Arc::new(RecordingSink::default());
    let (world, tools) = reading_world(&sink).await;
    let output = world
        .run(
            "cell-document",
            served::spec(8),
            vec![served::cell(
                r#"
const fetched = await web.fetch({ url: "only" });
finish(fetched.url);
"#,
            )],
        )
        .await;
    served::assert_answered("the cell document", &output);
    let read = tools.read.lock_recover().clone();
    let [(reference, document)] = read.as_slice() else {
        panic!("the cell's one fetch read its document: {read:?}");
    };
    assert_eq!(reference.entry, WorkflowDocumentEntry::Main);
    let document = document
        .as_ref()
        .expect("a running cell's document is readable by the reference its start named");
    let records = sink.records();
    let events = language_events(&records);
    let [(started, _)] = execution_starts(&events)[..] else {
        panic!("one cell started");
    };
    let emitted = emitted_sites(&events, &started.identity);
    let index = document.overlay_document();
    assert_eq!(emitted.len(), 1, "the one fetch is one site: {emitted:?}");
    assert!(
        emitted.iter().all(|site| index.contains(site)),
        "every site the cell reported is in its document"
    );
    assert!(
        finished(&events, &started.identity),
        "the cell reported its end"
    );
    world.shutdown().await;
}

/// A process a cell wrote and started names the cell's own document, by the
/// entry the arrow lowered to, and reports its calls at that entry's sites.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_process_a_cell_wrote_runs_as_an_entry_of_the_cells_document() {
    let sink = Arc::new(RecordingSink::default());
    let (world, tools) = reading_world(&sink).await;
    let output = world
        .run(
            "cell-process",
            served::spec(8),
            vec![served::cell(
                r#"
const worker = async (url: string) => {
  const fetched = await web.fetch({ url });
  return fetched.url;
};
const handle = await processes.start({ definition: worker, args: { url: "inner" } });
finish(await processes.await({ handle }));
"#,
            )],
        )
        .await;
    served::assert_answered("the cell's process", &output);
    let records = sink.records();
    let events = language_events(&records);
    let starts = execution_starts(&events);
    let (cell, cell_document) = starts
        .iter()
        .find(|(_, reference)| reference.entry == WorkflowDocumentEntry::Main)
        .expect("the cell started");
    let (process, process_document) = starts
        .iter()
        .find(|(_, reference)| reference.entry != WorkflowDocumentEntry::Main)
        .expect("the process started");
    assert_eq!(process_document.document, cell_document.document);
    let WorkflowDocumentEntry::Entry { function } = &process_document.entry else {
        panic!("a process enters its document by an entry");
    };
    assert_eq!(function.as_str(), "worker");
    // The process's fetch read the document its own start named.
    let read = tools.read.lock_recover().clone();
    let [(reference, document)] = read.as_slice() else {
        panic!("the process's one fetch read its document: {read:?}");
    };
    assert_eq!(&reference, process_document);
    let document = document
        .as_ref()
        .expect("a running process's document is readable");
    let index = document.overlay_document();
    let emitted = emitted_sites(&events, &process.identity);
    assert!(!emitted.is_empty(), "the process reported its fetch");
    assert!(emitted.iter().all(|site| index.contains(site)));
    assert!(
        !emitted_sites(&events, &cell.identity).is_empty(),
        "the cell reported its start and await"
    );
    assert!(finished(&events, &process.identity) && finished(&events, &cell.identity));
    world.shutdown().await;
}
