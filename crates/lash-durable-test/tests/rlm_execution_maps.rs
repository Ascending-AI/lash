//! Law L1 on the production paths (FIG-3571), on the durable engine (ported
//! by FIG-5308 from the deleted lash-protocol-rlm `production_map_law.rs`).
//!
//! A turn's cell runs on a served core with a trace sink, and the processes
//! a cell starts run on that core's node, each reading its module back from
//! the store through the decoder. In both, the complete compiled site
//! inventory (the instruction table and the aggregate batch tables) equals
//! the `ExecutionStarted` map's node set by id, kind and owner path; every
//! branch membership names a compiled branch site; every `(node_id, kind)`
//! the VM emits is mapped; and each named arm the run never took folds to
//! `Skipped`. And every path a model's code takes runs in a worker process.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;
#[path = "support/web_fetch.rs"]
mod web_fetch;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lash_core::facade_support::{TraceRecord, TraceSink, TraceSinkError};
use lash_sansio::sync::MutexExt as _;
use lash_vm_runtime::{
    TraceLanguageExecution, TraceLanguageExecutionIdentity, TraceLanguageExecutionMap,
    TraceLanguageExecutionPayload, TraceLashlangNodeObservation,
};

use served::{Tier, World};
use web_fetch::{Fetch, GrantFetch};

/// Every language-execution record the core traced.
#[derive(Default)]
struct RecordingSink(Mutex<Vec<TraceRecord>>);

impl TraceSink for RecordingSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        if matches!(
            &record.event,
            lash_core::TraceEvent::LanguageExecution { .. }
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

fn execution_maps<'r>(
    events: &[&'r TraceLanguageExecution],
) -> Vec<(&'r TraceLanguageExecution, &'r TraceLanguageExecutionMap)> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            TraceLanguageExecutionPayload::ExecutionStarted { execution_map } => {
                Some((*event, execution_map))
            }
            _ => None,
        })
        .collect()
}

fn emitted_sites(
    events: &[&TraceLanguageExecution],
    identity: &TraceLanguageExecutionIdentity,
) -> BTreeSet<(String, lash_sansio::ExecutionNodeKind)> {
    events
        .iter()
        .filter(|event| &event.identity == identity)
        .filter_map(|event| match &event.payload {
            TraceLanguageExecutionPayload::NodeStarted {
                node_id, node_kind, ..
            }
            | TraceLanguageExecutionPayload::NodeCompleted {
                node_id, node_kind, ..
            }
            | TraceLanguageExecutionPayload::NodeFailed {
                node_id, node_kind, ..
            }
            | TraceLanguageExecutionPayload::NodeWaiting {
                node_id, node_kind, ..
            }
            | TraceLanguageExecutionPayload::NodeResumed {
                node_id, node_kind, ..
            } => Some((node_id.clone(), *node_kind)),
            _ => None,
        })
        .collect()
}

/// The map law for one execution: `compiled` is the entry its artifact
/// compiles to, `map` what the execution published, `emitted` what it ran.
fn assert_map_is_the_compiled_inventory(
    compiled: &lash_vm::CompiledProgram,
    map: &TraceLanguageExecutionMap,
    emitted: &BTreeSet<(String, lash_sansio::ExecutionNodeKind)>,
    context: &str,
) {
    let compiled_sites = lash_vm::testing::harness::compiled_execution_sites(compiled);
    let inventory = compiled_sites
        .iter()
        .map(|site| {
            (
                site.node_id.clone(),
                site.node_kind,
                site.workflow_site.clone(),
            )
        })
        .collect::<BTreeSet<Site>>();
    let mapped = map
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node.kind, node.site.clone()))
        .collect::<BTreeSet<Site>>();
    assert_eq!(
        inventory.difference(&mapped).collect::<Vec<_>>(),
        Vec::<&Site>::new(),
        "{context}: every compiled site is mapped with its kind and owner path"
    );
    assert_eq!(
        mapped.difference(&inventory).collect::<Vec<_>>(),
        Vec::<&Site>::new(),
        "{context}: every mapped site is a compiled site"
    );
    let branches = compiled_sites
        .iter()
        .filter(|site| site.node_kind == lash_sansio::ExecutionNodeKind::Branch)
        .map(|site| site.node_id.clone())
        .collect::<BTreeSet<_>>();
    for node in &map.nodes {
        for membership in &node.branch_memberships {
            assert!(
                branches.contains(&membership.branch_node_id),
                "{context}: `{}` is a member of `{}`, which is not a compiled branch site",
                node.id,
                membership.branch_node_id
            );
        }
    }
    let declared = map
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node.kind))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        emitted.difference(&declared).collect::<Vec<_>>(),
        Vec::<&(String, lash_sansio::ExecutionNodeKind)>::new(),
        "{context}: every emitted (node_id, kind) is mapped"
    );
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

/// Whether `node` folded to `Skipped`.
fn assert_skipped(records: &[TraceRecord], artifact: &lash_vm::ModuleArtifact, marker: &str) {
    let graph = lash::tracing::fold_lashlang_graph(
        None,
        records,
        lash::tracing::DEFAULT_LASH_VM_GRAPH_HISTORY_LIMIT,
    )
    .expect("the records fold");
    let id = node_naming(artifact, marker);
    let node = graph
        .nodes
        .iter()
        .find(|node| node.id == id)
        .unwrap_or_else(|| panic!("the untaken `{marker}` arm is in the folded graph"));
    assert!(
        matches!(
            node.observation,
            TraceLashlangNodeObservation::Skipped { .. }
        ),
        "the untaken `{marker}` arm folds to Skipped: {:?}",
        node.observation
    );
}

async fn production_rlm_map_is_the_compiled_inventory_for_every_loop_kind(tier: Tier) {
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
    let maps = execution_maps(&events);
    let [(started, map)] = maps.as_slice() else {
        panic!("one execution_started event, got {}", maps.len());
    };
    let artifact = stored_artifact(&world, &started.identity.module_ref).await;
    let compiled =
        lash_vm::compile(&artifact, lash_vm::Entry::Main, None).expect("the cell's main compiles");
    let emitted = emitted_sites(&events, &started.identity);
    assert_map_is_the_compiled_inventory(&compiled, map, &emitted, "the RLM cell");
    let resource_operations = emitted
        .iter()
        .filter(|(_, kind)| *kind == lash_sansio::ExecutionNodeKind::ResourceOperation)
        .count();
    assert!(
        resource_operations >= 17,
        "the corpus must emit one resource-operation node per authored fetch statement, got {resource_operations}: {emitted:?}"
    );
    // A switch and a try project as one statement each, with no node per
    // arm, so their untaken default and catch have no node of their own to
    // fold; the untaken `else` of an `if` does.
    assert_skipped(&records, &artifact, "array-never");
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
        world
            .backend
            .process_registry()
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            })
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
                        "process `{}` failed before its execution map is checked: {:?}",
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
            "process `{}` succeeds before its execution map is checked",
            record.id
        );
    }
}

/// Run [`PROCESS_CORPUS`] on `workers` and check the map law on the worker
/// and the literal nested in it.
async fn process_map_fixture(tier: Tier, workers: lash::vm::WorkerService) {
    let sink = Arc::new(RecordingSink::default());
    let Some(world) = world(tier, workers, &sink).await else {
        return;
    };
    // The worker, then the literal nested in it that the worker starts.
    run_process_corpus(&world, PROCESS_CORPUS, 2).await;
    let records = sink.records();
    let events = language_events(&records);
    let maps = execution_maps(&events);
    let processes = maps
        .iter()
        .filter(|(started, _)| started.identity.entry_kind == "process")
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
        let artifact = stored_artifact(&world, &started.identity.module_ref).await;
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
    for (started, map) in processes {
        let artifact = stored_artifact(&world, &started.identity.module_ref).await;
        let process_ref = artifact
            .process_ref(&started.identity.entry_name)
            .expect("the executed process is exported")
            .clone();
        let compiled = lash_vm::compile(&artifact, lash_vm::Entry::Process(&process_ref), None)
            .expect("the executed process compiles");
        let emitted = emitted_sites(&events, &started.identity);
        assert!(!emitted.is_empty(), "the process run is observed");
        assert_map_is_the_compiled_inventory(
            &compiled,
            map,
            &emitted,
            &format!("process `{}`", started.identity.entry_name),
        );
        if started.identity.entry_name == worker_name {
            let own = records
                .iter()
                .filter(|record| {
                    matches!(&record.event, lash_core::TraceEvent::LanguageExecution { event, .. }
                        if event.identity == started.identity)
                })
                .cloned()
                .collect::<Vec<_>>();
            assert_skipped(&own, &artifact, "worker-never");
        }
    }
    world.shutdown().await;
}

async fn production_process_map_is_the_compiled_inventory_after_a_store_round_trip(tier: Tier) {
    process_map_fixture(tier, sim::untimed_workers()).await;
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
                async fn production_rlm_map_is_the_compiled_inventory_for_every_loop_kind() {
                    super::production_rlm_map_is_the_compiled_inventory_for_every_loop_kind(
                        super::served::Tier::$tier,
                    )
                    .await;
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
                async fn production_process_map_is_the_compiled_inventory_after_a_store_round_trip() {
                    super::production_process_map_is_the_compiled_inventory_after_a_store_round_trip(
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
    let maps = execution_maps(&events);
    assert_eq!(maps.len(), 2, "one cell map and one process map");
    let mut kinds = BTreeSet::new();
    for (started, map) in maps {
        assert!(
            kinds.insert(started.identity.entry_kind.clone()),
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
        if started.identity.entry_kind == "process" {
            let bound = own
                .iter()
                .filter_map(|event| match &event.payload {
                    TraceLanguageExecutionPayload::NodeStarted {
                        node_id,
                        occurrence,
                        call_id: Some(call),
                        ..
                    } => Some((node_id, *occurrence, call)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                bound.len(),
                2,
                "both resumed tool calls bind their admitted identities"
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
            let artifact = stored_artifact(&world, &started.identity.module_ref).await;
            let process_ref = artifact
                .process_ref(&started.identity.entry_name)
                .expect("the process is exported");
            let compiled = lash_vm::compile(&artifact, lash_vm::Entry::Process(process_ref), None)
                .expect("the process compiles");
            assert_map_is_the_compiled_inventory(
                &compiled,
                map,
                &emitted_sites(&events, &started.identity),
                "the resumed process",
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
    assert_eq!(
        kinds,
        BTreeSet::from(["main".to_owned(), "process".to_owned()])
    );
    world.shutdown().await;
}
