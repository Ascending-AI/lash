//! Law L1 on the production paths (FIG-3571).
//!
//! A foreground cell runs through the ordinary executor with a trace sink, and
//! a TypeScript process runs through the process engine with its module
//! published to a SQLite store and read back through a second store's
//! decoder. In both, the complete compiled site inventory (the instruction
//! table and the aggregate batch tables) equals the `ExecutionStarted` map's
//! node set by id, kind and owner path; every branch membership names a
//! compiled branch site; every `(node_id, kind)` the VM emits is mapped; and
//! each named arm the run never took folds to `Skipped`.

use super::*;

const SEED: u64 = 0x5_2c06;

#[derive(Default)]
struct RecordingSink(Mutex<Vec<lash_core::facade_support::TraceRecord>>);

impl TraceSink for RecordingSink {
    fn append(
        &self,
        record: &lash_core::facade_support::TraceRecord,
    ) -> Result<(), lash_core::facade_support::TraceSinkError> {
        if matches!(
            &record.event,
            lash_core::TraceEvent::LanguageExecution { .. }
        ) {
            self.0.lock().expect("trace sink lock").push(record.clone());
        }
        Ok(())
    }
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

async fn run_cell(source: &str) -> Vec<lash_core::facade_support::TraceRecord> {
    let sink = Arc::new(RecordingSink::default());
    let double =
        crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("fig3571-l1"),
            lash_core::TurnId::from("turn-1"),
        ))
        .await
        .expect("open the cell's handler");
    let context =
        lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
            crate::testing::double_ports(&double, &handler),
            Arc::new(BindingRecordingDeferredProvider {
                executions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                observed_bindings: Arc::new(std::sync::Mutex::new(Vec::new())),
                enumerations: Default::default(),
            }),
            lash_core::ToolCatalog::default(),
            lash_core::testing::exec_code_invocation(
                "fig3571-l1",
                "turn-1",
                1,
                1,
                "exec-l1",
                "exec:l1",
            ),
        );
    let mut state = RlmExecutionState::for_engine("typescript");
    let response = execute_code_with_channel_and_bounds(
        &mut state,
        context,
        ExecRequest {
            language: "typescript".to_string(),
            code: source.to_string(),
        },
        crate::testing::memory_artifact_store().await,
        LashlangSurface {
            language_features: lashlang::LashlangLanguageFeatures::default()
                .with_label_annotations(),
            ..LashlangSurface::default()
        },
        Some(Arc::new(BindingDeferredResolver {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })),
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig {
            sink: Some(sink.clone()),
            trace_context: TraceContext::default(),
        },
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    handler.close().await.expect("close the cell's handler");
    assert_eq!(response.error, None, "the L1 corpus cell executes");
    sink.0.lock().expect("trace sink lock").clone()
}

fn language_events(
    records: &[lash_core::facade_support::TraceRecord],
) -> Vec<&TraceLanguageExecution> {
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
) -> Vec<(
    &'r TraceLanguageExecution,
    &'r lash_trace::TraceLanguageExecutionMap,
)> {
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
    identity: &lash_trace::TraceLanguageExecutionIdentity,
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
    compiled: &lashlang::CompiledProgram,
    map: &lash_trace::TraceLanguageExecutionMap,
    emitted: &BTreeSet<(String, lash_sansio::ExecutionNodeKind)>,
    context: &str,
) {
    let compiled_sites = lashlang::testing::harness::compiled_execution_sites(compiled);
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
fn node_naming(artifact: &lashlang::ModuleArtifact, marker: &str) -> String {
    let graph = lashlang::workflow_graph_from_artifact(artifact, &lashlang::NoStatementText);
    let quoted = format!("\"{marker}\"");
    let matching = graph
        .nodes()
        .filter(|node| {
            !matches!(node.kind, lashlang::WorkflowNodeKind::Container(_))
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

async fn stored_artifact(
    store: &lashlang::LashlangArtifacts,
    module_ref: &str,
) -> Arc<lashlang::ModuleArtifact> {
    let module_ref: lashlang::ModuleRef =
        serde_json::from_value(serde_json::json!(module_ref)).expect("a module ref");
    store
        .get_module_artifact(&module_ref)
        .await
        .expect("the store reads")
        .expect("the executed module is stored")
}

#[test]
fn production_rlm_map_is_the_compiled_inventory_for_every_loop_kind() {
    block_on(async {
        let records = run_cell(CORPUS).await;
        let events = language_events(&records);
        let maps = execution_maps(&events);
        let [(started, map)] = maps.as_slice() else {
            panic!("one execution_started event, got {}", maps.len());
        };
        let artifact = stored_artifact(
            &crate::testing::memory_artifact_store().await,
            &started.identity.module_ref,
        )
        .await;
        let compiled = lashlang::compile(&artifact, lashlang::Entry::Main, None)
            .expect("the cell's main compiles");
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
        let graph = lash_trace::TraceLashlangGraphStore::fold(None, &records)
            .expect("the cell's records fold");
        // A switch and a try project as one statement each, with no node per
        // arm, so their untaken default and catch have no node of their own
        // to fold; the untaken `else` of an `if` does.
        let id = node_naming(&artifact, "array-never");
        let node = graph
            .nodes
            .iter()
            .find(|node| node.id == id)
            .expect("the untaken `array-never` arm is in the folded graph");
        assert!(
            matches!(
                node.observation,
                lash_trace::TraceLashlangNodeObservation::Skipped { .. }
            ),
            "the untaken `array-never` arm folds to Skipped: {:?}",
            node.observation
        );
    });
}

/// A process with a multi-statement loop, a branch, member assignment in a
/// braced and an unbraced arm, a callback, a labelled effect, and a literal
/// nested in it that it starts in turn.
const PROCESS_CORPUS: &str = r#"
const worker = async (limit: number) => {
  const box = { value: 0 };
  for (const step of [1, 2]) {
    await processes.emit({ value: step });
    if (step > limit) {
      await processes.emit({ value: "worker-never" });
    } else {
      box.value = step;
    }
  }
  if (box.value > 0) box.value += 1;
  const doubled = [1, 2].map((item) => item * 2);
  /** @label Labelled emit */
  await processes.emit({ value: doubled });
  const inner = async () => {
    for (const round of [1]) {
      await processes.emit({ value: round });
      await processes.emit({ value: "inner" });
    }
    return 1;
  };
  const nested = await processes.start({ definition: inner });
  return box.value;
};
const started = await processes.start({ definition: worker, args: { limit: 3 } });
finish("started");
"#;

#[tokio::test]
async fn production_process_map_is_the_compiled_inventory_after_a_store_round_trip() {
    let dir = tempfile::tempdir().expect("store directory");
    let path = dir.path().join("artifacts.db");
    // The cell publishes through one store; the engine reads through another
    // opened on the same file, so every module it runs comes back through
    // the decoder.
    let cell_store = lashlang::LashlangArtifacts::new(Arc::new(
        lash_sqlite_store::Store::open(&path)
            .await
            .expect("open the publishing store"),
    ));
    let engine_store = lashlang::LashlangArtifacts::new(Arc::new(
        lash_sqlite_store::Store::open(&path)
            .await
            .expect("open the engine's store"),
    ));
    let sink = Arc::new(RecordingSink::default());
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let process_env_store = backend.process_env_store();
    let effect_host = backend.effect_host();
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default(),
        lashlang::LashlangLanguageFeatures::default().with_label_annotations(),
        lashlang::LashlangHostCatalog::new(),
    );
    let session_policy = lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("L1 process test model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let traced_engine = || {
        // The process controls reach the engine through the worker's tool
        // catalog, so its surface carries no copy of them.
        lash_lashlang_runtime::LashlangProcessEngine::new(engine_store.clone(), surface.clone())
            .with_execution_trace(
                Some(sink.clone() as Arc<dyn TraceSink>),
                TraceContext::default(),
            )
    };
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(traced_engine()),
    );
    let registry_dyn = Arc::clone(&registry);
    let watched = lash_core::facade_support::watch_process_registry(registry_dyn);
    let worker = lash_core_worker::DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            // The worker serves the shipped process-control tools, so a process
            // body emits and starts the literal nested in it.
            Arc::new(lash_core::facade_support::PluginHost::new({
                let mut factories = lash_core::testing::test_code_protocol_factories();
                factories.push(Arc::new(
                    lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
                ));
                factories
            })),
            runtime_host,
            lash_core_worker::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoSessionWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(session_policy.clone()),
    )
    .expect("valid test native substrate config");
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        effect_host: Arc::clone(&effect_host),
        originator_override: None,
        env_store: Arc::clone(&process_env_store),
        engines: Arc::new(lash_core::ProcessEngineRegistry::new().with_registration(
            lash_lashlang_runtime::lashlang_process_engine_registration(traced_engine()),
        )),
    });
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        lash_core::testing::TestExecutionPorts::over_host(effect_host, process_env_store),
        Arc::new(ProcessControlToolProvider),
        process_control_tool_catalog(),
        None,
        processes,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            session_policy,
        ),
    );
    let mut state = RlmExecutionState::for_engine("typescript");
    let response = execute_code_with_channel_and_bounds(
        &mut state,
        ctx,
        ExecRequest {
            language: "typescript".to_string(),
            code: PROCESS_CORPUS.to_string(),
        },
        cell_store,
        surface,
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    // Drive every started process to its end: the worker, then the literal
    // nested in it that the worker starts.
    let registry_dyn = Arc::clone(&registry);
    let mut finished = BTreeSet::new();
    for _ in 0..4 {
        let _admitted = worker
            .drive_pending_processes()
            .await
            .expect("drive the started processes");
        let listed = registry
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .expect("list processes");
        for record in listed {
            if finished.insert(record.id.clone()) {
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    lash_core::NativeProcessWork::for_registry(Arc::clone(&registry_dyn))
                        .await_terminal(&record.id),
                )
                .await
                .unwrap_or_else(|_| panic!("process `{}` reaches its end", record.id))
                .expect("await the process");
            }
        }
    }
    let records = sink.0.lock().expect("trace sink lock").clone();
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
    let worker_name = {
        let artifact = stored_artifact(&engine_store, &processes[0].0.identity.module_ref).await;
        artifact
            .ir()
            .declarations
            .iter()
            .find_map(|declaration| match declaration {
                lashlang::Declaration::Process(process)
                    if matches!(&process.origin, lashlang::ProcessOrigin::Lifted { site, .. }
                        if site.root == lashlang::AstRoot::Main && site.steps.len() == 2) =>
                {
                    Some(process.name.to_string())
                }
                _ => None,
            })
            .expect("the worker is lifted from main")
    };
    for (started, map) in processes {
        let artifact = stored_artifact(&engine_store, &started.identity.module_ref).await;
        let process_ref = artifact
            .process_ref(&started.identity.entry_name)
            .expect("the executed process is exported")
            .clone();
        let compiled = lashlang::compile(&artifact, lashlang::Entry::Process(&process_ref), None)
            .expect("the executed process compiles");
        let emitted = emitted_sites(&events, &started.identity);
        assert!(!emitted.is_empty(), "the process run is observed");
        assert_map_is_the_compiled_inventory(
            &compiled,
            map,
            &emitted,
            &format!("process `{}`", started.identity.entry_name),
        );
        let own = records
            .iter()
            .filter(|record| {
                matches!(&record.event, lash_core::TraceEvent::LanguageExecution { event, .. }
                    if event.identity == started.identity)
            })
            .cloned()
            .collect::<Vec<_>>();
        let graph = lash_trace::TraceLashlangGraphStore::fold(None, &own)
            .expect("the process's records fold");
        if started.identity.entry_name == worker_name {
            let id = node_naming(&artifact, "worker-never");
            let node = graph
                .nodes
                .iter()
                .find(|node| node.id == id)
                .expect("the untaken arm is in the folded graph");
            assert!(
                matches!(
                    node.observation,
                    lash_trace::TraceLashlangNodeObservation::Skipped { .. }
                ),
                "the worker's untaken arm folds to Skipped: {:?}",
                node.observation
            );
        }
    }
}
