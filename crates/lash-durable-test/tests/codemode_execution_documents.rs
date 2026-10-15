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
const TOOLS_COMPONENT: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new("codemode-document-tools", lash_core::FormatVersion::ONE);

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
                    lash_core::lifetime::session_or_starter,
                    lash_core::testing::run_policy_naming(&[TOOLS_COMPONENT]),
                ),
            ))
            .process_component(
                lash_core::testing::tool_component(TOOLS_COMPONENT, Arc::clone(&tools)),
                TOOLS_COMPONENT,
            )
            .plugin(lash_core::testing::tool_plugin(TOOLS_COMPONENT, tools))
            .plugin(web_fetch::grant_fetch())
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
            TraceLanguageExecutionPayload::ExecutionStarted { .. } => {
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
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok(vec![web_fetch::fetch_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
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
    assert_eq!(
        emitted.len(),
        2,
        "fetch and finish each name a site: {emitted:?}"
    );
    assert_eq!(
        output
            .result
            .tool_calls
            .iter()
            .map(|call| call.tool.as_str())
            .collect::<Vec<_>>(),
        ["web_fetch", "finish"]
    );
    for call in &output.result.tool_calls {
        let sites = events
            .iter()
            .filter_map(|event| match &event.payload {
                TraceLanguageExecutionPayload::Node {
                    at,
                    fact:
                        lash::tracing::TraceNodeFact::Started {
                            call_id: Some(call_id),
                        },
                } if event.identity == started.identity && call_id == &call.call_id => {
                    Some(at.site.clone())
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
await control.finish(await processes.await({ handle }));
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

/// FIG-6371: worker deltas across a cell's parked Once calls sum to the
/// same counts as the same document with immediately answered reads. A later
/// cell of the same session can run off and stays unobserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_cells_parked_calls_sum_to_unsplit_counts_and_the_next_cell_can_be_off() {
    use lash::workflow::document::{Datum, ErrorDatum, Handle, Site, Timestamp};
    use lash::workflow::view::RunObservation;
    use lash_kernel_vm::{
        Bounds, EffectModes, Host, KernelMachine, Machine, PreparedLibrary, Program, Start, Step,
        Target,
    };
    struct Immediate;
    impl Host for Immediate {
        fn clock(&mut self) -> Timestamp {
            panic!("this cell reads no clock")
        }
        fn random(&mut self) -> u64 {
            panic!("this cell draws no random")
        }
        fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
            panic!("no projections")
        }
        fn perform(
            &mut self,
            _: &lash::workflow::document::EffectName,
            args: &[Datum],
            _: &Site,
        ) -> Result<Result<Datum, ErrorDatum>, lash_kernel_vm::HostFault> {
            Ok(Ok(args.first().cloned().unwrap_or(Datum::Null)))
        }
        fn print(&mut self, _: &Datum) {}
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }
    let sink = Arc::new(RecordingSink::default());
    let (world, tools) = reading_world(&sink).await;
    let session = world.session("cell-counts", served::spec(8)).await;
    world.script(
        "counted-cell",
        vec![served::cell(
            r#"await web.fetch({url: "only"}); await control.finish(null);"#,
        )],
    );
    let output = session
        .send(lash::TurnInput::text("counted-cell"))
        .run(lash::RunSpec {
            observe: RunObservation::Counts,
            ..lash::RunSpec::default()
        })
        .output()
        .await
        .expect("counted cell answers");
    served::assert_answered("counted cell", &output);
    let reads = tools.read.lock_recover().clone();
    let document = reads[0].1.as_ref().unwrap();
    let kernel = document.document();
    let modes = EffectModes::reading(kernel.manifest.effects.keys().cloned());
    let embedding = lash::vm::built_worker_dialects(&lash::vm::WorkerTuning::default()).unwrap();
    let mut unsplit = KernelMachine::start_reading(
        Program {
            document: Arc::new(kernel.clone()),
            library: PreparedLibrary::new(Arc::clone(embedding.registry().unwrap())),
        },
        Bounds {
            charge: 1_000_000,
            memory: 64 << 20,
            call_depth: 64,
            live_tasks: 16,
            requests_per_park: 16,
            join_members: 16,
        },
        Start {
            target: Target::Main,
            args: vec![],
            bindings: Default::default(),
        },
        modes,
    )
    .unwrap();
    unsplit.set_observe(RunObservation::Counts);
    let ended = unsplit.run(&mut Immediate, u64::MAX).unwrap();
    assert!(
        matches!(ended, Step::Ended(lash_kernel_vm::End::Finished(_))),
        "{ended:?}"
    );
    let records = sink.records();
    let overlay = lash::workflow::fold_workflow_overlay(None, None, &records, 1).unwrap();
    assert_eq!(overlay.observation, Some(RunObservation::Counts));
    assert_eq!(overlay.ran, unsplit.take_ran());
    assert!(
        records
            .iter()
            .filter(|record| matches!(
                record.event,
                lash::tracing::TraceEvent::LanguageExecution {
                    event: TraceLanguageExecution {
                        payload: TraceLanguageExecutionPayload::Ran { .. },
                        ..
                    },
                    ..
                }
            ))
            .count()
            >= 2,
        "counts cross actual parks"
    );
    let view = lash::workflow::view::derive(
        document,
        lash_vm_runtime::standard_functions().unwrap().as_ref(),
    )
    .unwrap();
    let counted = lash::workflow::view::coverage(&view, &overlay.ran_counts());
    assert!(counted.statements.iter().any(|row| row.runs == Some(1)));

    let before = records.len();
    world.script(
        "off-cell",
        vec![served::cell("await control.finish(null);")],
    );
    let off = session
        .send(lash::TurnInput::text("off-cell"))
        .output()
        .await
        .unwrap();
    served::assert_answered("off cell", &off);
    let records = sink.records();
    let off = lash::workflow::fold_workflow_overlay(None, None, &records[before..], 1).unwrap();
    let unobserved = lash::workflow::view::coverage(&view, &off.ran_counts());
    assert_eq!(unobserved.observation, Some(RunObservation::Off));
    assert!(unobserved.statements.iter().all(|row| row.runs.is_none()));
    assert!(off.ran.is_empty());
    world.shutdown().await;
}
