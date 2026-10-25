//! DQ-REOPEN: durable workflows have the same observable execution when
//! every committed park is followed by a fresh OS process over the same file.
//! The parent never executes a workflow; children start through the facade
//! and the production process activation drives every segment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "cross_process_reopen/corpus.rs"]
mod corpus;
#[path = "support/kernel_namespace.rs"]
mod kernel_namespace;
#[path = "support/packages.rs"]
mod packages;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use lash_durable::DurableStore;
use lash_durable_test::{Matrix, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_execution_contracts::backend::StoreSet;
use lash_protocol_types::sync::MutexExt as _;
use lash_vocabulary::identity::ProcessId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, Mutex},
};

const LAW: &str = "durable_corpus_resumes_in_a_fresh_process_at_every_park";
const COMPONENT: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("reopen-corpus", lash::plugins::FormatVersion::ONE);

#[derive(Default, Serialize, Deserialize)]
struct State {
    process: Option<ProcessId>,
    logical_ms: u64,
    parks: usize,
    checkpoints: BTreeMap<String, String>,
    requests: Vec<Value>,
    facts: Vec<Value>,
    result: Option<Value>,
    pids: Vec<u32>,
    charges: Vec<Value>,
    worker_charges: Vec<Value>,
}

fn read_state(path: &Path) -> State {
    serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap()
}
fn write_state(path: &Path, state: &State) {
    std::fs::write(path.join("state.json"), serde_json::to_vec(state).unwrap()).unwrap();
}

struct Tools {
    requests: Mutex<Vec<Value>>,
    meters: Mutex<Vec<Value>>,
}
fn definition(name: &str) -> lash::tools::ToolDefinition {
    lash::tools::ToolDefinition::raw(
        format!("reopen:{name}"),
        format!("corpus.{name}"),
        "Durable corpus echo or input",
        json!({}),
        lash::tools::ToolDefinition::any_output(
            "Corpus echo and deferred input carry arbitrary workflow values.",
        ),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(120))
    .with_declaration(
        lash::tools::ToolDeclaration::deferring(),
        Some(lash::tools::ParkBound::UntilScopeEnd),
    )
    .unwrap()
}
#[async_trait::async_trait]
impl lash_core::ToolProvider for Tools {
    fn tool_manifests(
        &self,
        _: &lash_core::OwnerConfigView,
    ) -> Result<Vec<lash::tools::ToolManifest>, lash_execution_contracts::PluginError> {
        Ok(["echo", "input"]
            .map(|name| definition(name).manifest())
            .into())
    }
    fn resolve_contract(
        &self,
        name: &str,
        _: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash::tools::ToolContract>> {
        name.strip_prefix("corpus.")
            .filter(|name| ["echo", "input"].contains(name))
            .map(|name| Arc::new(definition(name).contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let _key = call.context.completion_key().unwrap();
        self.requests.lock_recover().push(json!({
            "call": call.context.call_id(), "operation": call.name(), "args": call.args,
        }));
        if call.name() == "corpus.input" {
            return lash_core::ToolAttemptOutcome::pending(lash_core::PendingCompletion::new());
        }
        lash_core::ToolAttemptOutcome::pending(
            lash_core::PendingCompletion::new().resolved_by_timer(
                lash_durable::DurableInstant(
                    call.context.admission_now().0
                        + if call.args.as_str() == Some("b") {
                            20_000
                        } else {
                            10_000
                        },
                ),
                call.args.clone(),
            ),
        )
    }
}

fn core(
    stores: Arc<dyn StoreSet>,
    clock: &Arc<SimClock>,
    tools: Arc<Tools>,
    dialect: &str,
) -> lash::LashCore {
    let backend = served::configured_backend(stores, sim::settings(), Vec::new());
    let meter_tools = tools.clone();
    let workers = sim::workers(clock).with_run_meter_observer(move |owner, meters, ended| {
        meter_tools.meters.lock_recover().push(json!({
            "owner": format!("{owner:?}"), "charged": meters.charged.expect("exact meter"), "ended": ended,
        }));
    });
    let bounds = workers.config().run_bounds;
    let provider = lash::tool_provider_key!("corpus");
    let spec = lash::plugins::PluginSpec::new()
        .with_tool_provider(provider, tools.clone())
        .with_package_provider(
            provider,
            packages::operation_provider(COMPONENT.component(), provider, tools),
        );
    let component = lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(COMPONENT.component()),
        spec,
    )
    .as_component(COMPONENT.format())
    .unwrap();
    lash::LashCore::standard_builder(backend)
        .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
            workers, dialect, bounds,
        )))
        .process_component(Arc::new(component), COMPONENT)
        .process_component(
            Arc::new(lash::process_controls::ProcessControlsComponent::new(
                lash_execution_contracts::process::lifetime::starter,
                packages::run_policy_naming(&[
                    COMPONENT,
                    lash::process_controls::PROCESS_CONTROLS_CONFIG,
                ]),
            )),
            lash::process_controls::PROCESS_CONTROLS_CONFIG,
        )
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget {
            nodes: lash::CommitBudgetLimit::bounded(4096),
            ..Default::default()
        })
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("reopen-corpus"),
            lash::persistence::LeaseIncarnationId::new(format!("reopen-{}", std::process::id())),
        ))
        .unwrap()
}

async fn start(core: &lash::LashCore, case: &corpus::Case) -> ProcessId {
    let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(64))
        .component(COMPONENT, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .component(lash::process_controls::PROCESS_CONTROLS_CONFIG, &())
        .build()
        .unwrap();
    let artifacts = core.host_artifacts();
    let pin = lash_core::HostArtifactPin::mint();
    let (packages, request) = kernel_namespace::namespace(core, &pin, &env).await;
    let imported = artifacts
        .import_workflow(case.language(), case.source, &request, None)
        .await
        .unwrap();
    let lash::workflow::WorkflowImport::Imported { document, .. } = imported else {
        panic!("{}: {imported:?}", case.name)
    };
    let start = artifacts
        .publish_workflow(
            &pin,
            &document,
            &lash::package::StartSelector::Export(lash::schema::Name::new("demo")),
            &request,
            None,
            None,
        )
        .await
        .unwrap();
    let env = artifacts
        .link_process_env(
            &env,
            &lash::process::LinkTarget::Linked(start.clone()),
            &packages,
            &Default::default(),
        )
        .await
        .unwrap();
    let env_ref = artifacts.publish_process_env(&pin, &env).await.unwrap();
    let payload = serde_json::to_value(lash_vm_runtime::KernelProcessInput {
        code: lash_vm_runtime::process::InlineRequest::Linked { start },
        args: case.args().as_object().unwrap().clone(),
    })
    .unwrap();
    let kind = lash_vm_runtime::LASH_VM_ENGINE_KIND;
    let settings = core.processes().engine_defaults(kind, &payload).unwrap();
    core.processes()
        .start(
            lash::process::ProcessStartRequest::engine(
                kind.to_owned(),
                payload,
                lash::process::EngineLaunch::new(env_ref, settings),
                lash::process::ProcessOriginator::host(),
                lash::process::LifetimeDecision::Detached,
            ),
            core.effect_host(),
        )
        .await
        .unwrap()
        .process_id
}

// Compare authored facts, retaining occurrence/call/site identity and ordering.
// Wall-clock deadlines and writer epochs describe the deployment, not the program.
fn facts_projection(fact: lash::process::ProcessLifecycleFact) -> Value {
    use lash::process::ProcessLifecycleFact as F;
    match fact {
        F::Waiting { wait } | F::Resumed { wait } => {
            let mut kind = serde_json::to_value(&wait.kind).unwrap();
            kind.as_object_mut().unwrap().remove("resume_at_ms");
            kind.as_object_mut().unwrap().remove("until_ms");
            json!({"kind": kind, "site": wait.site})
        }
        F::Terminal { outcome, .. } => json!({"terminal": lash_protocol_types::Durable(&outcome)}),
        other => other.payload_for_projection(),
    }
}
async fn facts(core: &lash::LashCore, process: &ProcessId) -> Vec<Value> {
    let read = core
        .processes()
        .events(
            lash::process::ProcessHistoryContinuation::start(process.clone()),
            std::num::NonZeroUsize::new(500).unwrap(),
            lash::process::ProcessEventQueryMode::Full,
        )
        .await
        .unwrap();
    let lash::process::ProcessEventReadOutcome::Retained(page) = read.outcome else {
        panic!("events not retained")
    };
    assert!(matches!(
        page.more,
        lash::process::ProcessEventPageMore::Complete
    ));
    let lash::process::ProcessEventPageEvents::Full(events) = page.events else {
        panic!("not full events")
    };
    events
        .into_iter()
        .map(|event| json!({"type": event.fact.event_type(), "fact": facts_projection(event.fact)}))
        .collect()
}

// The acknowledged-commit probe stops the runner before adoption or any
// following effect. The supervisor runs outside the runtime's worker threads;
// a parked slice exits the OS process with that acknowledgement still held.
type CommitBoundary = (lash_durable::ActorKey, std::sync::mpsc::Sender<()>);
struct ParkProbe {
    tripwire: Tripwire,
    tx: tokio::sync::mpsc::UnboundedSender<CommitBoundary>,
}
impl lash_durable::DurableProbe for ParkProbe {
    fn owner_read(
        &self,
        actor: &lash_durable::ActorKey,
        epoch: lash_durable::Epoch,
        event: lash_durable::OwnerRead,
        amount: u64,
    ) {
        self.tripwire.owner_read(actor, epoch, event, amount);
    }
    fn owner_cut(
        &self,
        actor: &lash_durable::ActorKey,
        epoch: lash_durable::Epoch,
        label: lash_durable::CommitLabel,
        cut: lash_durable::OwnerCut,
    ) -> Result<(), lash_durable::DurableError> {
        self.tripwire.owner_cut(actor, epoch, label, cut)?;
        if actor.kind() == lash_durable::ActorKind::Process
            && label == lash_durable::CommitLabel::PROCESS_ADVANCE
            && cut == lash_durable::OwnerCut::Acknowledged
        {
            let (tx, rx) = std::sync::mpsc::channel();
            self.tx.send((actor.clone(), tx)).unwrap();
            rx.recv().unwrap();
        }
        Ok(())
    }
    fn body_entered(&self, id: &lash_durable::domain::AdmittedId) {
        self.tripwire.body_entered(id);
    }
    fn vm_program_entered(&self, key: &lash_durable::domain::ExecKey) {
        self.tripwire.vm_program_entered(key);
    }
    fn outcome_lookup(&self, id: &lash_durable::domain::AdmittedId) {
        self.tripwire.outcome_lookup(id);
    }
    fn committed_ordinal_emitted(
        &self,
        owner: &lash_durable::domain::OwnerKey,
        run: lash_durable::domain::RunSeq,
        ordinal: lash_durable::domain::Ordinal,
    ) {
        self.tripwire.committed_ordinal_emitted(owner, run, ordinal);
    }
    fn checkpoint_restored(
        &self,
        session: &lash_vocabulary::identity::SessionId,
        run: &lash_vocabulary::identity::TurnId,
    ) {
        self.tripwire.checkpoint_restored(session, run);
    }
}

// Read the authenticated meter of the exact committed checkpoint. This
// diagnostic uses the same typed graph decoder as production; it never
// reconstructs a machine in the supervising process.
async fn charge(core: &lash::LashCore, process: &ProcessId, path: &Path) -> Option<(String, u64)> {
    use lash_core_store::checkpoint_graph::{
        DecodeLimits, EncodedNode, GraphDecoder, NodeId, NodeKind,
    };
    let limits = DecodeLimits::within_total(64 * 1024 * 1024);
    let state = lash_core_execution::runtime::actor::process::stored_engine_state(
        core.backend().durable().as_ref(),
        process,
        limits,
    )
    .await
    .unwrap()?;
    let root = state.pages?;
    let db = rusqlite::Connection::open(path.join("lash.db")).unwrap();
    let mut query = db
        .prepare("SELECT hex(hash), content FROM blobs WHERE encoding = 1")
        .unwrap();
    let nodes: std::collections::BTreeMap<_, _> = query
        .query_map([], |row| {
            let hash: String = row.get(0)?;
            let bytes: Vec<u8> = row.get(1)?;
            let id = NodeId::from_hex(&hash.to_lowercase()).unwrap();
            Ok((
                id,
                EncodedNode {
                    id,
                    bytes: Arc::from(bytes),
                },
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let checkpoint: lash_kernel_state::paged::CheckpointRoot = GraphDecoder::new(&nodes, limits)
        .decode(root, NodeKind::CHECKPOINT_ROOT)
        .unwrap();
    Some((
        root.to_hex(),
        checkpoint
            .meters
            .charged
            .expect("the corpus runs with exact charges"),
    ))
}

async fn records(core: &lash::LashCore) -> Vec<lash_execution_contracts::process::ProcessRecord> {
    lash_core::testing::process_roster_records_for_fixture(
        core.backend().process_registry().as_ref(),
        &lash_execution_contracts::process::ProcessListFilter {
            status: lash::process::ProcessStatusFilter::Any,
            ..Default::default()
        },
    )
    .await
    .unwrap()
}
fn process_name(
    record: &lash_execution_contracts::process::ProcessRecord,
    root: &ProcessId,
) -> String {
    if record.id == root {
        "root".into()
    } else {
        format!(
            "start:{}",
            record
                .start_key
                .as_ref()
                .expect("a nested process has its declaring start key")
                .as_str()
        )
    }
}
// Independently minted child ids are deployment identities. Relate them by
// the durable start key, and calls by their producer's effect occurrence,
// preserving every use of those identities in values and trace facts.
fn canonical(value: &mut Value, identities: &BTreeMap<String, String>) {
    match value {
        Value::String(text) => {
            for (from, to) in identities {
                *text = text.replace(from, to);
            }
        }
        Value::Array(values) => {
            for value in values {
                canonical(value, identities);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                canonical(value, identities);
            }
        }
        _ => {}
    }
}
async fn observations(core: &lash::LashCore, root: &ProcessId, state: &mut State) {
    let mut identities = BTreeMap::new();
    let mut observed = Vec::new();
    let mut processes = records(core).await;
    processes.sort_by_key(|record| process_name(record, root));
    for record in processes {
        let name = process_name(&record, root);
        identities.insert(record.id.as_str().to_owned(), name.clone());
        let facts = facts(core, &record.id).await;
        for event in &facts {
            if event["type"] == "process.effect_outcome"
                && let Some(call) = event["fact"]["call_id"].as_str()
            {
                if record.id != root {
                    let identity = format!("call:{name}:{}", event["fact"]["at"]);
                    assert!(
                        identities.insert(call.to_owned(), identity).is_none(),
                        "one call per child effect occurrence"
                    );
                }
                let call_id = call.parse::<lash_core::ToolCallId>().unwrap();
                let request = core
                    .processes()
                    .tool_call(&record.id, &call_id)
                    .await
                    .unwrap()
                    .expect("an effect retains its request");
                state.requests.push(json!({"process": record.id, "call":request.call_id,"operation": request.tool,"args":request.args}));
            }
        }
        observed.push((name, facts));
    }
    observed.sort_by(|a, b| a.0.cmp(&b.0));
    state.facts = observed
        .into_iter()
        .map(|(name, facts)| json!({"process":name,"facts":facts}))
        .collect();
    for value in state
        .requests
        .iter_mut()
        .chain(&mut state.facts)
        .chain(&mut state.charges)
        .chain(&mut state.worker_charges)
        .chain(&mut state.result)
    {
        canonical(value, &identities);
    }
}

async fn child(path: &Path, case: &corpus::Case, mode: &str) {
    let mut state = read_state(path);
    state.pids.push(std::process::id());
    let clock = SimClock::new();
    clock.advance_to(state.logical_ms).await;
    let stores = sim::file(path.join("lash.db"), clock.clone()).await;
    let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
    let tools = Arc::new(Tools {
        requests: Mutex::new(Vec::new()),
        meters: Mutex::new(Vec::new()),
    });
    let core = core(Arc::new(stores), &clock, tools.clone(), case.dialect);
    if mode == "init" {
        state.process = Some(Box::pin(start(&core, case)).await);
        write_state(path, &state);
        return;
    }
    let process = state.process.clone().unwrap();
    if mode == "stale" {
        use std::io::{BufRead as _, Write as _};
        let actor = lash_durable::ActorKey::process(process.as_str()).unwrap();
        let row = database.actor(&actor).await.unwrap().unwrap();
        let tx = database.begin(&actor, row.epoch).await.unwrap();
        println!("DQ-OPENED");
        std::io::stdout().flush().unwrap();
        let mut signal = String::new();
        std::io::stdin().lock().read_line(&mut signal).unwrap();
        assert_eq!(signal, "advance\n");
        let result = database
            .commit(tx, lash_durable::CommitLabel::new("duplicate-restore"))
            .await;
        assert!(
            matches!(result, Err(lash_durable::DurableError::OwnershipLost(_))),
            "stale restore: {result:?}"
        );
        println!("DQ-FENCED");
        return;
    }
    let (tx, mut commits) = tokio::sync::mpsc::unbounded_channel();
    let (backend, activation) = lash::testing::node_activation(
        &core,
        Arc::new(ParkProbe {
            tripwire: Tripwire::default(),
            tx,
        }),
    )
    .unwrap();
    let nodes = Arc::new(SimNodes::new(
        database,
        clock.clone(),
        Script::new(),
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: backend.formats().decodes(),
            serves: lash::testing::node_serves(&core),
            max_active: 1,
        },
        activation,
    ));
    let (drive_tx, mut drive_rx) =
        tokio::sync::mpsc::unbounded_channel::<(bool, tokio::sync::oneshot::Sender<()>)>();
    let execution_nodes = nodes.clone();
    let execution_clock = clock.clone();
    let execution = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                execution_nodes.start("corpus");
                while let Some((step, done)) = drive_rx.recv().await {
                    if step {
                        if execution_nodes.step().await.is_none() {
                            execution_clock.advance_by(100).await;
                        }
                    } else {
                        execution_nodes.quiesce().await;
                    }
                    let _ = done.send(());
                }
                execution_nodes.kill("corpus");
            });
    });
    for iteration in 0..2000 {
        let (ready, mut driven) = tokio::sync::oneshot::channel();
        drive_tx.send((iteration != 0, ready)).unwrap();
        loop {
            tokio::select! {
                biased;
                Some((actor, acknowledgement)) = commits.recv() => {
                    let parked_process = ProcessId::try_from(actor.id().to_owned()).unwrap();
                    let record = backend.process_registry().get_process(&parked_process).await.unwrap().unwrap();
                    // A kernel checkpoint precedes effect execution. The runtime
                    // has parked only after its deferred outcomes and waits have
                    // committed; cutting earlier could interrupt a Once body.
                    if !record.waits().is_empty()
                        && let Some((checkpoint, charged)) = charge(&core, &parked_process, path).await
                        && state.checkpoints.get(actor.id()) != Some(&checkpoint)
                    {
                        state.checkpoints.insert(actor.id().to_owned(), checkpoint);
                        state.parks += 1;
                        state.charges.push(json!({"process": actor.id(), "charged": charged}));
                        state.requests.extend(tools.requests.lock_recover().drain(..));
                        state.worker_charges.extend(tools.meters.lock_recover().drain(..));
                        state.logical_ms = clock.logical_ms();
                        if mode == "slice" {
                            write_state(path, &state);
                            // This is the boundary under test: no drop, owner
                            // hand-back, cache flush or callback survives it.
                            std::process::exit(0);
                        }
                    }
                    acknowledgement.send(()).unwrap();
                }
                _ = &mut driven => { break; }
            }
        }
        let record = backend
            .process_registry()
            .get_process(&process)
            .await
            .unwrap()
            .unwrap();
        if let Some(terminal) = record.terminal() {
            let output = terminal.clone().into_await_output();
            let lash::process::ProcessAwaitOutput::Settled { output } = &output else {
                panic!("not settled: {output:?}")
            };
            assert!(output.is_success(), "{} failed: {output:?}", case.name);
            state.result = Some(output.value_for_projection());
            state
                .requests
                .extend(tools.requests.lock_recover().drain(..));
            state
                .worker_charges
                .extend(tools.meters.lock_recover().drain(..));
            observations(&core, &process, &mut state).await;
            state.logical_ms = clock.logical_ms();
            drop(drive_tx);
            execution.join().unwrap();
            write_state(path, &state);
            return;
        }
        let mut processes = records(&core).await;
        processes.sort_by_key(|record| process_name(record, &process));
        // A host supplies external input only after the committed park has
        // been observed (and, in sliced mode, by the following OS process).
        for record in &processes {
            for call in core
                .completions()
                .parked(lash::admin::CallOwner::Process(record.id.clone()))
                .await
                .unwrap()
            {
                if call.tool_id.as_str() == "reopen:input" {
                    core.completions()
                        .resolve(
                            call.key.as_str(),
                            lash_core::Resolution::Ok(json!("answer")),
                        )
                        .await
                        .unwrap();
                }
            }
        }
    }
    panic!(
        "{} never ended: {}",
        case.name,
        nodes.script().rendered_trace()
    );
}

fn command(path: &Path, case: usize, mode: &str) -> std::process::Command {
    let mut command = std::process::Command::new("bash");
    command
        .args(["-c", "ulimit -v 8388608; exec \"$@\"", "reopen-child"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", LAW, "--nocapture"])
        .env("LASH_REOPEN_CHILD", path)
        .env("LASH_REOPEN_CASE", case.to_string())
        .env("LASH_REOPEN_MODE", mode);
    command
}
fn launch(path: &Path, case: usize, mode: &str) {
    let output = command(path, case, mode).output().unwrap();
    assert!(
        output.status.success(),
        "child {mode} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// Own the stale subprocess from spawn through reaping, including a failed
// readiness assertion; dropping std::process::Child alone does not stop it.
struct ChildGuard(Option<std::process::Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// DQ-DUPLICATE: an old restore opened before a new owner advances the
/// parked process cannot commit, even from a different OS process.
#[test]
fn duplicate_restore_in_another_process_gets_a_typed_fence_refusal() {
    use std::io::{BufRead as _, Write as _};
    use std::process::Stdio;
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &State::default());
    launch(dir.path(), 0, "init");
    launch(dir.path(), 0, "slice");
    let before = read_state(dir.path());
    assert_eq!(before.parks, 1);
    assert!(before.result.is_none());
    let mut stale = ChildGuard(Some(
        command(dir.path(), 0, "stale")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let mut output = std::io::BufReader::new(stale.0.as_mut().unwrap().stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "old restore did not open"
        );
        if line.trim() == "DQ-OPENED" {
            break;
        }
    }
    launch(dir.path(), 0, "slice");
    while read_state(dir.path()).result.is_none() {
        launch(dir.path(), 0, "slice");
    }
    assert!(
        read_state(dir.path()).result.is_some(),
        "exactly one restore advanced to completion"
    );
    stale
        .0
        .as_mut()
        .unwrap()
        .stdin
        .take()
        .unwrap()
        .write_all(b"advance\n")
        .unwrap();
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut output, &mut rest).unwrap();
    let result = stale.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        rest.contains("DQ-FENCED"),
        "the stale process must observe OwnershipLost: {rest}"
    );
}

fn minimum_crossings(case: &corpus::Case, state: &State, restarts: usize) {
    assert!(
        state.parks >= case.minimum,
        "{}: {} parks, declared minimum {}",
        case.name,
        state.parks,
        case.minimum
    );
    assert_eq!(
        restarts, state.parks,
        "{}: every park must cross a process boundary",
        case.name
    );
}

#[test]
fn durable_corpus_resumes_in_a_fresh_process_at_every_park() {
    let cases = corpus::cases();
    if let Ok(path) = std::env::var("LASH_REOPEN_CHILD") {
        let index: usize = std::env::var("LASH_REOPEN_CASE").unwrap().parse().unwrap();
        let mode = std::env::var("LASH_REOPEN_MODE").unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(child(Path::new(&path), &cases[index], &mode));
        return;
    }
    for (index, case) in cases.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        std::fs::create_dir(&seed).unwrap();
        write_state(&seed, &State::default());
        launch(&seed, index, "init");
        for name in ["expected", "sliced"] {
            let path = dir.path().join(name);
            std::fs::create_dir(&path).unwrap();
            // Snapshot the committed database, including its WAL, after init
            // exited. Copying only lash.db could silently lose the start.
            rusqlite::Connection::open(seed.join("lash.db"))
                .unwrap()
                .execute("VACUUM INTO ?1", [path.join("lash.db").to_str().unwrap()])
                .unwrap();
            std::fs::copy(seed.join("state.json"), path.join("state.json")).unwrap();
        }
        let expected = dir.path().join("expected");
        launch(&expected, index, "whole");
        let expected = read_state(&expected);
        let sliced = dir.path().join("sliced");
        let mut children = 0;
        let actual = loop {
            assert!(children < 100, "{} exceeded 100 process slices", case.name);
            launch(&sliced, index, "slice");
            children += 1;
            let state = read_state(&sliced);
            if state.result.is_some() {
                break state;
            }
        };
        // init's PID is inherited in both copies; every subsequent slice
        // must report a new PID, including the one that reaches completion.
        assert_eq!(
            actual.pids.iter().collect::<BTreeSet<_>>().len(),
            actual.pids.len()
        );
        minimum_crossings(case, &actual, children - 1);
        assert_eq!(actual.parks, expected.parks, "{} parks", case.name);
        assert_eq!(actual.result, expected.result, "{} result", case.name);
        assert_eq!(actual.requests, expected.requests, "{} requests", case.name);
        assert_eq!(actual.facts, expected.facts, "{} activity/trace", case.name);
        assert_eq!(
            actual.charges, expected.charges,
            "{} checkpoint charges",
            case.name
        );
        assert_eq!(
            actual.worker_charges, expected.worker_charges,
            "{} worker charges through completion",
            case.name
        );
        println!(
            "{}.{}: parks={} restarts={} charges={:?}",
            case.name,
            case.dialect,
            actual.parks,
            children - 1,
            actual.charges
        );
    }
}
