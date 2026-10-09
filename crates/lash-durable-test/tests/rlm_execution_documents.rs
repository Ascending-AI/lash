//! Law L1 on the production paths (FIG-3571, put in terms of the workflow
//! document by FIG-5576), on the durable engine.
//!
//! A turn's cell runs on a served core with a trace sink, and the processes
//! a cell starts run on that core's node, each reading its module back from
//! the store through the decoder. In both, `ExecutionStarted` names a
//! document a host reads back by that reference alone; the complete compiled
//! site inventory (the instruction table and the aggregate batch tables)
//! equals the sites of the body that document's entry selects, by id, kind
//! and owner path; every site the VM reports is one of them, so the overlay
//! folded over the document has no mismatch; and an arm the run never took
//! has a node in the document and no site in the overlay. And every path a
//! model's code takes runs in a worker process.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;
#[path = "support/web_fetch.rs"]
mod web_fetch;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lash::workflow::{
    WorkflowDocumentRead, WorkflowDocumentRef, WorkflowExecutionDocument, WorkflowExecutionOverlay,
};
use lash_core::facade_support::{TraceRecord, TraceSink, TraceSinkError};
use lash_sansio::sync::MutexExt as _;
use lash_vm_runtime::{
    TraceLanguageExecution, TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload,
};

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
/// controls, the deferred `web.fetch`, and `sink` tracing every record.
async fn world(
    tier: Tier,
    workers: lash::vm::WorkerService,
    sink: &Arc<RecordingSink>,
) -> Option<World> {
    world_with_tools(tier, workers, sink, Arc::new(Fetch)).await
}

/// A fixture tool whose contract is visible when a process captures its tools.
struct ListedFetch;

#[async_trait::async_trait]
impl lash_core::ToolProvider for ListedFetch {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![web_fetch::fetch_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        lash_core::ToolProvider::resolve_contract(&Fetch, name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolProvider::execute(&Fetch, call).await
    }
}

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

/// Every loop kind TypeScript lowers, each with a multi-statement body, plus
/// member assignment in braced and unbraced arms, a switch, a try, an array
/// callback, a labelled statement and a process literal.
const CORPUS: &str = r#"
const items = [1, 2];
for (const item of items) {
  await web.fetch({ url: "array-first" });
  if (item > 0) {
    await web.fetch({ url: "array-then" });
  } else {
    await web.fetch({ url: "array-never" });
  }
}
for (const [key, value] of new Map([["a", 1]])) {
  await web.fetch({ url: "map-first" });
  await web.fetch({ url: "map-second" });
}
for (const member of new Set([1])) {
  await web.fetch({ url: "set-first" });
  await web.fetch({ url: "set-second" });
}
for (const [name, text] of new URLSearchParams("a=1")) {
  await web.fetch({ url: "params-first" });
  await web.fetch({ url: "params-second" });
}
for (const field in { a: 1 }) {
  await web.fetch({ url: "keys-first" });
  await web.fetch({ url: "keys-second" });
}
for (const { url } of [{ url: "destructured" }]) {
  await web.fetch({ url: url });
  await web.fetch({ url: "destructured-second" });
}
const box = { value: 0 };
if (items.length > 0) box.value = (await web.fetch({ url: "unbraced-member" })).length;
if (items.length > 0) {
  box.value = (await web.fetch({ url: "braced-member" })).length;
}
switch (items.length) {
  case 2:
    await web.fetch({ url: "switch" });
    break;
  default:
    await web.fetch({ url: "switch-default" });
}
try {
  await web.fetch({ url: "try" });
} catch (error) {
  await web.fetch({ url: "catch" });
}
const doubled = items.map((item) => item * 2);
/** @label Labelled fetch */
await web.fetch({ url: "labelled" });
const worker = async () => {
  for (const step of [1, 2]) {
    await web.fetch({ url: "worker-first" });
    await web.fetch({ url: "worker-second" });
  }
};
finish(doubled);
"#;

/// A process with a multi-statement loop, a branch, member assignment in a
/// braced and an unbraced arm, a callback, a labelled effect, and a literal
/// nested in it that it starts in turn.
const PROCESS_CORPUS: &str = r#"
const worker = await processes.create({ dialect: "typescript", source: `
const worker = async (limit: number) => {
  const box = { value: 0 };
  for (const step of [1, 2]) {
    await sleep(step);
    if (step > limit) {
      await sleep("worker-never".length);
    } else {
      box.value = step;
    }
  }
  if (box.value > 0) box.value += 1;
  const doubled = [1, 2].map((item) => item * 2);
  /** @label Labelled sleep */
  await sleep(doubled.length);
  const inner = await processes.create({ dialect: "typescript",
    source: 'const inner = async () => { for (const round of [1]) { await sleep(round); await sleep("inner".length); } return 1; };'
  });
  const nested = await processes.start({ definition: inner });
  return box.value;
};` });
const started = await processes.start({ definition: worker, args: { limit: 3 } });
finish("started");
"#;

/// A worker the cell creates and starts: every worker path a model's code
/// takes.
const RECEIPT_CORPUS: &str = r#"
const worker = await processes.create({ dialect: "typescript",
  source: 'const worker = async () => { return 1; };'
});
const started = await processes.start({ definition: worker });
finish("started");
"#;

fn language_events(records: &[TraceRecord]) -> Vec<&TraceLanguageExecution> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            lash_core::TraceEvent::LanguageExecution { event, .. } => Some(event),
            _ => None,
        })
        .collect()
}

type Site = (
    String,
    lash_sansio::ExecutionNodeKind,
    lash_sansio::WorkflowExecutionSite,
);

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
) -> BTreeSet<lash_sansio::WorkflowSiteRef> {
    events
        .iter()
        .filter(|event| &event.identity == identity)
        .filter_map(|event| event.payload.occurrence_key())
        .map(|(site, _)| site)
        .collect()
}

/// The document `reference` names, read as a host reads it.
async fn read_document(
    world: &World,
    reference: &WorkflowDocumentRef,
) -> WorkflowExecutionDocument {
    match world
        .core
        .host_artifacts()
        .execution_document(reference)
        .await
        .expect("the document read answers")
    {
        WorkflowDocumentRead::Read(document) => {
            assert_eq!(document.reference(), reference);
            *document
        }
        other => panic!("the document an execution names is readable: {other:?}"),
    }
}

/// The execution sites of the body `document` enters, with their kinds.
fn document_sites(document: &WorkflowExecutionDocument) -> BTreeSet<Site> {
    fn collect(body: &lash_vm::WorkflowSubgraph, sites: &mut BTreeSet<Site>) {
        for node in body.nodes() {
            sites.extend(
                node.execution_sites
                    .iter()
                    .map(|site| (node.id.to_string(), site.kind, site.clone())),
            );
            if let lash_vm::WorkflowNodeKind::Container(container) = &node.kind {
                for (_, child) in container.child_subgraphs() {
                    collect(child, sites);
                }
            }
        }
    }
    let mut sites = BTreeSet::new();
    collect(document.body(), &mut sites);
    sites
}

/// The document law for one execution: `compiled` is the entry its artifact
/// compiles to, `document` what its start named, `emitted` the sites it
/// reported on and `records` its own records. Answers the overlay they fold
/// to over the document.
fn assert_document_is_the_compiled_inventory(
    compiled: &lash_vm::CompiledProgram,
    document: &WorkflowExecutionDocument,
    emitted: &BTreeSet<lash_sansio::WorkflowSiteRef>,
    records: &[TraceRecord],
    context: &str,
) -> WorkflowExecutionOverlay {
    let inventory = lash_vm::testing::harness::compiled_execution_sites(compiled)
        .iter()
        .map(|site| {
            (
                site.node_id.clone(),
                site.node_kind,
                site.workflow_site.clone(),
            )
        })
        .collect::<BTreeSet<Site>>();
    let stated = document_sites(document);
    assert_eq!(
        inventory.difference(&stated).collect::<Vec<_>>(),
        Vec::<&Site>::new(),
        "{context}: every compiled site is in the document with its kind and owner path"
    );
    assert_eq!(
        stated.difference(&inventory).collect::<Vec<_>>(),
        Vec::<&Site>::new(),
        "{context}: every site of the document is a compiled site"
    );
    let index = document.overlay_document();
    for site in emitted {
        assert!(
            index.contains(site),
            "{context}: the reported site {site} is in the document"
        );
    }
    let overlay = lash::workflow::fold_workflow_overlay(
        None,
        Some(&index),
        records,
        lash::workflow::DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT,
    )
    .expect("the records fold");
    assert_eq!(overlay.document.as_ref(), Some(document.reference()));
    assert!(
        overlay.mismatches.is_empty(),
        "{context}: {:?}",
        overlay.mismatches
    );
    assert!(overlay.coverage.is_complete(), "{context}");
    let observed = overlay
        .sites
        .iter()
        .map(|site| site.site.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        emitted.difference(&observed).collect::<Vec<_>>(),
        Vec::<&lash_sansio::WorkflowSiteRef>::new(),
        "{context}: every reported site has a state in the overlay"
    );
    overlay
}

/// The records of the execution `identity` names: its language records,
/// and the body starts of its process's steps.
fn own_records(
    records: &[TraceRecord],
    identity: &TraceLanguageExecutionIdentity,
) -> Vec<TraceRecord> {
    records
        .iter()
        .filter(|record| match &record.event {
            lash_core::TraceEvent::LanguageExecution { event, .. } => &event.identity == identity,
            lash_core::TraceEvent::StepBodyStarted { step } => matches!(
                &identity.subject,
                lash::tracing::TraceRuntimeSubject::Process { process_id }
                    if *process_id == step.process_id
            ),
            _ => false,
        })
        .cloned()
        .collect()
}

/// The node of `artifact`'s view whose payload names `marker`.
fn node_naming(artifact: &lash_vm::ModuleArtifact, marker: &str) -> String {
    let graph = lash_vm::workflow_graph_from_artifact(artifact);
    let quoted = format!("\"{marker}\"");
    let matching = graph
        .nodes()
        .filter(|node| {
            !matches!(node.kind, lash_vm::WorkflowNodeKind::Container(_))
                && serde_json::to_string(&node.kind)
                    .expect("node kind serializes")
                    .contains(&quoted)
        })
        .map(|node| node.id.to_string())
        .collect::<Vec<_>>();
    let [id] = matching.as_slice() else {
        panic!("one node names `{marker}`, got {matching:?}");
    };
    id.clone()
}

/// The module `module_ref` names, read back from the backend's store.
async fn stored_artifact(world: &World, module_ref: &str) -> Arc<lash_vm::ModuleArtifact> {
    let module_ref: lash_vm::ModuleRef =
        serde_json::from_value(serde_json::json!(module_ref)).expect("a module ref");
    lash_vm::LashVmArtifacts::of_backend(&world.backend)
        .get_module_artifact(&module_ref)
        .await
        .expect("the store reads")
        .expect("the executed module is stored")
}

/// The arm that names `marker` never ran: the document states its node,
/// and the overlay holds no site for it.
fn assert_untaken(
    overlay: &WorkflowExecutionOverlay,
    document: &WorkflowExecutionDocument,
    artifact: &lash_vm::ModuleArtifact,
    marker: &str,
) {
    let id = node_naming(artifact, marker);
    assert!(
        document_sites(document)
            .iter()
            .any(|(node_id, _, _)| *node_id == id),
        "the untaken `{marker}` arm is in the document"
    );
    let observed = overlay
        .sites
        .iter()
        .filter(|site| site.site.node_id == id)
        .collect::<Vec<_>>();
    assert!(
        observed.is_empty(),
        "the untaken `{marker}` arm was never observed: {observed:?}"
    );
    assert!(
        overlay.sites.iter().any(|site| site.branch.is_some()),
        "the branch around it names the arm it took"
    );
}

async fn production_rlm_document_is_the_compiled_inventory_for_every_loop_kind(tier: Tier) {
    let sink = Arc::new(RecordingSink::default());
    let Some(world) = world(tier, sim::untimed_workers(), &sink).await else {
        return;
    };
    let output = world
        .run("l1-cell", served::spec(1024), vec![served::cell(CORPUS)])
        .await;
    served::assert_answered("the L1 corpus cell", &output);
    assert_eq!(output.final_value(), Some(&serde_json::json!([2, 4])));
    let records = sink.records();
    let events = language_events(&records);
    let starts = execution_starts(&events);
    let [(started, reference)] = starts.as_slice() else {
        panic!("one execution_started event, got {}", starts.len());
    };
    let artifact = stored_artifact(&world, started.identity.document.module_ref.as_str()).await;
    let compiled =
        lash_vm::compile(&artifact, lash_vm::Entry::Main, None).expect("the cell's main compiles");
    let document = read_document(&world, reference).await;
    let emitted = emitted_sites(&events, &started.identity);
    let overlay = assert_document_is_the_compiled_inventory(
        &compiled,
        &document,
        &emitted,
        &own_records(&records, &started.identity),
        "the RLM cell",
    );
    // A site's kind is the document's to state, not the event's.
    let kinds = document_sites(&document)
        .into_iter()
        .map(|(node_id, kind, site)| {
            (
                lash_sansio::WorkflowSiteRef::new(node_id, site.site_path),
                kind,
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let resource_operations = emitted
        .iter()
        .filter(|site| kinds.get(site) == Some(&lash_sansio::ExecutionNodeKind::ResourceOperation))
        .count();
    assert!(
        resource_operations >= 17,
        "the corpus must emit one resource-operation node per authored fetch statement, got {resource_operations}: {emitted:?}"
    );
    // A switch and a try project as one statement each, with no node per
    // arm, so their untaken default and catch have no node of their own;
    // the untaken `else` of an `if` does.
    assert_untaken(&overlay, &document, &artifact, "array-never");
    world.shutdown().await;
}

/// Run `corpus` as a cell on `world` and await the `count` processes it
/// starts, directly or nested, to their success.
async fn run_process_corpus(world: &World, corpus: &str, count: usize) {
    let output = world
        .run("l1-process", served::spec(1024), vec![served::cell(corpus)])
        .await;
    served::assert_answered("the L1 process corpus cell", &output);
    let registered = || async {
        lash_core::testing::process_roster_records_for_fixture(
            world.backend.process_registry().as_ref(),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            },
        )
        .await
        .expect("the registry lists its processes")
    };
    let ended = tokio::time::timeout(served::WATCHDOG, async {
        loop {
            let listed = registered().await;
            for record in &listed {
                if record.is_terminal() {
                    assert_eq!(
                        record.status(),
                        lash_core::ProcessStatus::Completed,
                        "process `{}` failed before its document is checked: {:?}",
                        record.id,
                        record.outcome(),
                    );
                }
            }
            if listed.len() == count && listed.iter().all(|record| record.status().is_terminal()) {
                return listed;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await;
    let Ok(ended) = ended else {
        let listed = registered()
            .await
            .into_iter()
            .map(|record| (record.status(), record.id))
            .collect::<Vec<_>>();
        panic!("deadlock watchdog: {count} processes end; registered {listed:?}");
    };
    for record in ended {
        assert_eq!(
            record.status(),
            lash_core::ProcessStatus::Completed,
            "process `{}` succeeds before its document is checked",
            record.id
        );
    }
}

/// Run [`PROCESS_CORPUS`] on `workers` and check the document law on the
/// worker and the literal nested in it.
async fn process_document_fixture(tier: Tier, workers: lash::vm::WorkerService) {
    let sink = Arc::new(RecordingSink::default());
    let Some(world) = world(tier, workers, &sink).await else {
        return;
    };
    // The worker, then the literal nested in it that the worker starts.
    run_process_corpus(&world, PROCESS_CORPUS, 2).await;
    let records = sink.records();
    let events = language_events(&records);
    let starts = execution_starts(&events);
    let processes = starts
        .iter()
        .filter(|(started, _)| {
            matches!(
                started.identity.document.entry,
                lash::workflow::WorkflowDocumentEntry::Process { .. }
            )
        })
        .collect::<Vec<_>>();
    let names = processes
        .iter()
        .map(|(started, _)| started.identity.entry_name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names.len(),
        2,
        "the worker and the literal nested in it both run: {names:?}"
    );
    let mut worker_name = None;
    for (started, _) in &processes {
        let artifact = stored_artifact(&world, started.identity.document.module_ref.as_str()).await;
        if artifact.ir().declarations.iter().any(|declaration| {
            matches!(declaration, lash_vm::Declaration::Process(process)
                if process.name == started.identity.entry_name
                    && process.params.iter().any(|param| param.name.as_str() == "limit"))
        }) {
            worker_name = Some(started.identity.entry_name.clone());
        }
    }
    // Both separately compiled modules lift their process at the same
    // main path. The worker's parameter identifies it independently of
    // trace arrival order; the nested process has no parameters.
    let worker_name = worker_name.expect("the worker takes the limit parameter");
    for (started, reference) in processes {
        let artifact = stored_artifact(&world, started.identity.document.module_ref.as_str()).await;
        let process_ref = artifact
            .process_ref(&started.identity.entry_name)
            .expect("the executed process is exported")
            .clone();
        let compiled = lash_vm::compile(&artifact, lash_vm::Entry::Process(&process_ref), None)
            .expect("the executed process compiles");
        let document = read_document(&world, reference).await;
        assert_eq!(
            document.entry_name(),
            Some(started.identity.entry_name.as_str()),
            "the reference selects the process the execution entered"
        );
        let emitted = emitted_sites(&events, &started.identity);
        assert!(!emitted.is_empty(), "the process run is observed");
        let overlay = assert_document_is_the_compiled_inventory(
            &compiled,
            &document,
            &emitted,
            &own_records(&records, &started.identity),
            &format!("process `{}`", started.identity.entry_name),
        );
        if started.identity.entry_name == worker_name {
            assert_untaken(&overlay, &document, &artifact, "worker-never");
        }
    }
    world.shutdown().await;
}

async fn production_process_document_is_the_compiled_inventory_after_a_store_round_trip(
    tier: Tier,
) {
    process_document_fixture(tier, sim::untimed_workers()).await;
}

/// Every production path a model's code takes (the cell, the process body,
/// compiling a definition, inspecting an artifact, capturing state, reading
/// a module's references) checks out a worker process, never this one.
async fn every_model_code_path_runs_in_a_worker(tier: Tier) {
    use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
    use lash_vm_client::service::{Request, Response, WorkerPath};
    let workers = sim::untimed_workers().with_worker_receipts();
    let id = lash_core::ProcessDefinitionId::from_sha256_digest([41; 32]);
    let mut remote = lash_vm_client::RemoteState::pristine(workers.clone());
    remote
        .insert_global(
            "definition",
            lash_vm::from_json(serde_json::json!({"$lash_definition_id": id.to_string()})),
        )
        .await
        .expect("worker installs the candidate root");
    assert_eq!(
        remote.referenced_definition_ids(),
        BTreeSet::from([id.clone()])
    );
    let capture = remote
        .capture(&Default::default(), lash_core::FleetFormat::current())
        .await
        .expect("worker captures candidate roots");
    assert_eq!(capture.definition_ids, BTreeSet::from([id]));
    let definition = workers
        .request_accounted(Request::CreateDefinition {
            source: "const answer = async (): Promise<number> => { return 42; };".into(),
            environment: lash_vm::LashVmHostEnvironment::new(lash_vm::LashVmHostCatalog::new()),
        })
        .await
        .expect("definition compiler runs in a worker");
    assert!(matches!(definition, Response::Definition(_)));
    let sink = Arc::new(RecordingSink::default());
    let Some(world) = world(tier, workers.clone(), &sink).await else {
        return;
    };
    run_process_corpus(&world, RECEIPT_CORPUS, 1).await;
    world.shutdown().await;
    let receipts = workers.worker_receipts();
    assert!(
        receipts
            .iter()
            .all(|receipt| receipt.pid != std::process::id()),
        "every actual checkout belongs to a child: {receipts:?}"
    );
    let paths = receipts
        .iter()
        .map(|receipt| receipt.path)
        .collect::<BTreeSet<_>>();
    for path in [
        WorkerPath::References,
        WorkerPath::Compile,
        WorkerPath::CreateDefinition,
        WorkerPath::Artifact,
        WorkerPath::State,
        WorkerPath::Cell,
        WorkerPath::Process,
    ] {
        assert!(
            paths.contains(&path),
            "production path {path:?} has no worker process receipt: {receipts:?}"
        );
    }
}

tiered_laws!(every_model_code_path_runs_in_a_worker);

/// The map laws on each tier, for durable cells and process bodies.
macro_rules! map_laws_on {
    ($($module:ident, $tier:ident);+ $(;)?) => {
        $(
            mod $module {
                #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
                async fn production_rlm_document_is_the_compiled_inventory_for_every_loop_kind() {
                    super::production_rlm_document_is_the_compiled_inventory_for_every_loop_kind(
                        super::served::Tier::$tier,
                    )
                    .await;
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
                async fn production_process_document_is_the_compiled_inventory_after_a_store_round_trip() {
                    super::production_process_document_is_the_compiled_inventory_after_a_store_round_trip(
                        super::served::Tier::$tier,
                    )
                    .await;
                }
            }
        )+
    };
}

map_laws_on!(
    maps_sqlite_memory, SqliteMemory;
    maps_sqlite_file, SqliteFile;
    maps_postgres, Postgres;
);

/// FIG-5338: a cell and a process keep one observed execution while
/// resuming their snapshots, and publish the process's complete inventory.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn durable_language_trace_continues_once_across_quiet_points() {
    let sink = Arc::new(RecordingSink::default());
    let world = world_with_tools(
        Tier::SqliteMemory,
        sim::untimed_workers(),
        &sink,
        Arc::new(ListedFetch),
    )
    .await
    .expect("SQLite memory is available");
    let output = world
        .run(
            "trace-quiet-points",
            served::spec(64),
            vec![served::cell(
                r#"
const definition = await processes.create({ dialect: "typescript",
  source: 'const worker = async () => { for (const value of [1, 2]) { await web.fetch({ url: value }); await sleep(100); } return 42; };'
});
const handle = await processes.start({ definition });
const result = await handle;
finish(result);
"#,
            )],
        )
        .await;
    served::assert_answered("the trace quiet points", &output);
    let records = sink.records();
    let events = language_events(&records);
    let starts = execution_starts(&events);
    assert_eq!(starts.len(), 2, "one cell start and one process start");
    let mut kinds = BTreeSet::new();
    for (started, reference) in starts {
        assert!(
            kinds.insert(matches!(
                started.identity.document.entry,
                lash::workflow::WorkflowDocumentEntry::Process { .. }
            )),
            "each execution starts once"
        );
        let own = events
            .iter()
            .filter(|event| event.identity == started.identity)
            .collect::<Vec<_>>();
        assert_eq!(
            own.iter()
                .filter(|event| matches!(
                    event.payload,
                    TraceLanguageExecutionPayload::ExecutionFinished { .. }
                ))
                .count(),
            1,
            "each execution finishes once"
        );
        let starts = own
            .iter()
            .filter_map(|event| match &event.payload {
                TraceLanguageExecutionPayload::NodeStarted {
                    node_id,
                    occurrence,
                    ..
                } => Some((node_id.clone(), *occurrence)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!starts.is_empty(), "the execution's nodes are observed");
        assert_eq!(
            starts.len(),
            starts.iter().collect::<BTreeSet<_>>().len(),
            "a resumed node never starts twice"
        );
        if matches!(
            started.identity.document.entry,
            lash::workflow::WorkflowDocumentEntry::Process { .. }
        ) {
            // FIG-5576: the worker reports each admitted step body as it
            // starts; the VM's own node start carries no call.
            let bound = records
                .iter()
                .filter_map(|record| match &record.event {
                    lash_core::TraceEvent::StepBodyStarted { step } => {
                        Some((step.site(), step.occurrence, &step.call_id, step.attempt))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                bound.len(),
                2,
                "both resumed tool calls report their admitted body start"
            );
            assert!(
                own.iter().all(|event| !matches!(
                    event.payload,
                    TraceLanguageExecutionPayload::NodeStarted {
                        call_id: Some(_),
                        ..
                    }
                )),
                "the VM's node start names no admitted call"
            );
            assert_eq!(bound[0].0, bound[1].0, "one static call site runs twice");
            assert_ne!(
                bound[0].1, bound[1].1,
                "the continuation retains occurrence numbering"
            );
            assert_ne!(
                bound[0].2, bound[1].2,
                "distinct calls retain distinct admitted identities"
            );
            let artifact =
                stored_artifact(&world, started.identity.document.module_ref.as_str()).await;
            let process_ref = artifact
                .process_ref(&started.identity.entry_name)
                .expect("the process is exported");
            let compiled = lash_vm::compile(&artifact, lash_vm::Entry::Process(process_ref), None)
                .expect("the process compiles");
            let document = read_document(&world, reference).await;
            let overlay = assert_document_is_the_compiled_inventory(
                &compiled,
                &document,
                &emitted_sites(&events, &started.identity),
                &own_records(&records, &started.identity),
                "the resumed process",
            );
            let called = overlay
                .sites
                .iter()
                .find(|site| site.site == bound[0].0)
                .expect("the step's site is observed");
            assert_eq!(
                called
                    .call
                    .as_ref()
                    .map(|call| (&call.call_id, call.attempt)),
                bound
                    .iter()
                    .max_by_key(|(_, occurrence, _, _)| *occurrence)
                    .map(|(_, _, call, attempt)| (*call, Some(*attempt))),
                "the site's latest occurrence is bound to the call its body ran under"
            );
            assert_eq!(
                own.iter()
                    .filter(|event| matches!(
                        event.payload,
                        TraceLanguageExecutionPayload::NodeWaiting { .. }
                    ))
                    .count(),
                2,
                "both sleeps publish their waits"
            );
            assert_eq!(
                own.iter()
                    .filter(|event| matches!(
                        event.payload,
                        TraceLanguageExecutionPayload::NodeResumed { .. }
                    ))
                    .count(),
                2,
                "both sleeps publish their resolutions"
            );
        }
    }
    assert_eq!(kinds, BTreeSet::from([false, true]));
    world.shutdown().await;
}

/// A listed `web.fetch` a process's step may retry: its first attempt at the
/// url `flaky` reports a retryable failure. It records the call and attempt
/// each body ran under.
#[derive(Default)]
struct FlakyFetch(Mutex<Vec<(lash_core::ToolCallId, u32)>>);

fn flaky_definition() -> lash_core::ToolDefinition {
    web_fetch::fetch_definition().with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("a nonzero attempt bound"),
        1,
        1,
    ))
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for FlakyFetch {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![flaky_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "web_fetch").then(|| Arc::new(flaky_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let attempt = call.context.attempt_number();
        self.0
            .lock_recover()
            .push((call.context.call_id().clone(), attempt));
        if call.args["url"] == "flaky" && attempt == 1 {
            return lash_core::ToolOutcome::failure(lash_core::ToolFailure::with_suggested_delay(
                lash_core::ToolFailureClass::External,
                "flaky_fetch_transient",
                "the first attempt reported a transient failure",
                Some(1),
            ))
            .into();
        }
        lash_core::ToolOutcome::ok(serde_json::json!({ "url": call.args["url"].clone() })).into()
    }
}

/// FIG-5576: the worker reports a step's body start only for an admitted
/// execution. A retried body reports again with the same site, occurrence
/// and call and its next attempt; a round of steps refused past the
/// process's tool-call limit reports none, so no site shows an admitted call
/// for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_retried_step_body_keeps_its_occurrence_and_call_and_a_refused_step_reports_none() {
    let sink = Arc::new(RecordingSink::default());
    let tools = Arc::new(FlakyFetch::default());
    let world = world_with_tools(
        Tier::SqliteMemory,
        sim::untimed_workers(),
        &sink,
        tools.clone(),
    )
    .await
    .expect("SQLite memory is available");
    // The limit admits the worker's one retried call and refuses its round
    // of five.
    let output = world
        .run(
            "step-body-starts",
            served::spec(4),
            vec![served::cell(
                r#"
const definition = await processes.create({ dialect: "typescript",
  source: 'const worker = async () => { const first = await web.fetch({ url: "flaky" }); const rest = await Promise.all([web.fetch({ url: "a" }), web.fetch({ url: "b" }), web.fetch({ url: "c" }), web.fetch({ url: "d" }), web.fetch({ url: "e" })]); return 1; };'
});
const started = await processes.start({ definition });
finish("started");
"#,
            )],
        )
        .await;
    served::assert_answered("the step body starts", &output);
    let ended = tokio::time::timeout(served::WATCHDOG, async {
        loop {
            let listed = lash_core::testing::process_roster_records_for_fixture(
                world.backend.process_registry().as_ref(),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..lash_core::ProcessListFilter::default()
                },
            )
            .await
            .expect("the registry lists its processes");
            if let [record] = listed.as_slice()
                && record.is_terminal()
            {
                return record.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("deadlock watchdog: the worker ends");
    assert_eq!(
        ended.status(),
        lash_core::ProcessStatus::Failed,
        "the refused round ends the process: {:?}",
        ended.outcome()
    );
    assert!(
        format!("{:?}", ended.outcome()).contains("tool_call_limit"),
        "the refusal is the tool-call limit: {:?}",
        ended.outcome()
    );

    let bodies = tools.0.lock_recover().clone();
    let [(first_call, 1), (second_call, 2)] = bodies.as_slice() else {
        panic!("the flaky call's body ran twice and no refused body ran: {bodies:?}");
    };
    assert_eq!(first_call, second_call, "a retry is the same call");

    let records = sink.records();
    let starts = records
        .iter()
        .filter_map(|record| match &record.event {
            lash_core::TraceEvent::StepBodyStarted { step } => Some(step.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [first, retried] = starts.as_slice() else {
        panic!("one body start per admitted attempt, none for the refused round: {starts:#?}");
    };
    assert_eq!(first.process_id, ended.id);
    assert_eq!(
        (retried.site(), retried.occurrence, &retried.call_id),
        (first.site(), first.occurrence, &first.call_id),
        "the retried body keeps its site, occurrence and call"
    );
    assert_eq!(&first.call_id, first_call, "the call the body ran under");
    assert!(
        retried.attempt > first.attempt,
        "the retry is a later attempt: {} then {}",
        first.attempt,
        retried.attempt
    );
    assert_ne!(first.event_key(), retried.event_key());

    let events = language_events(&records);
    let (started, reference) = execution_starts(&events)
        .into_iter()
        .find(|(started, _)| {
            matches!(
                started.identity.document.entry,
                lash::workflow::WorkflowDocumentEntry::Process { .. }
            )
        })
        .expect("the worker's start");
    let document = read_document(&world, reference).await;
    let overlay = lash::workflow::fold_workflow_overlay(
        None,
        Some(&document.overlay_document()),
        &own_records(&records, &started.identity),
        lash::workflow::DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT,
    )
    .expect("the worker's records fold");
    assert!(overlay.mismatches.is_empty(), "{:?}", overlay.mismatches);
    assert!(overlay.conflicts.is_empty(), "{:?}", overlay.conflicts);
    let called = overlay
        .sites
        .iter()
        .filter(|site| site.call.is_some())
        .collect::<Vec<_>>();
    let [site] = called.as_slice() else {
        panic!("only the admitted step's site shows a call: {called:#?}");
    };
    assert_eq!(site.site, first.site());
    let call = site.call.as_ref().expect("the bound call");
    assert_eq!(
        (call.occurrence, &call.call_id, call.attempt),
        (first.occurrence, &first.call_id, Some(retried.attempt))
    );
    assert_eq!(
        site.summary.retained_occurrences, 1,
        "two attempts are one occurrence"
    );
    world.shutdown().await;
}

/// A `web.fetch` that reads, while the cell that called it is still running,
/// the document that cell's start named.
#[derive(Default)]
struct ReadingFetch {
    sink: Arc<RecordingSink>,
    core: std::sync::OnceLock<lash::LashCore>,
    read: Mutex<Vec<(WorkflowDocumentRef, Option<WorkflowExecutionDocument>)>>,
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
            .expect("the calling cell started");
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

/// FIG-5576: a cell's start names a document a host can read while the cell
/// runs, whether or not the cell declares a process: with no map on the
/// start, the reference is the only way to the cell's labels, kinds and
/// arms.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_cell_without_a_process_names_a_document_readable_while_it_runs() {
    let sink = Arc::new(RecordingSink::default());
    let tools = Arc::new(ReadingFetch {
        sink: Arc::clone(&sink),
        ..ReadingFetch::default()
    });
    let world = world_with_tools(
        Tier::SqliteMemory,
        sim::untimed_workers(),
        &sink,
        tools.clone(),
    )
    .await
    .expect("SQLite memory is available");
    assert!(tools.core.set(world.core.clone()).is_ok());
    let output = world
        .run(
            "cell-document",
            served::spec(8),
            vec![served::cell(
                r#"
/** @label Fetch once */
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
    assert_eq!(reference.entry, lash::workflow::WorkflowDocumentEntry::Main);
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
    assert!(!emitted.is_empty());
    assert!(
        emitted.iter().all(|site| index.contains(site)),
        "every site the cell reported is in its document"
    );
    assert!(
        document
            .graph()
            .nodes()
            .any(|node| node.display_name() == "Fetch once"),
        "the label is the document's to state"
    );
    world.shutdown().await;
}
