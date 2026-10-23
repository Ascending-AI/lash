//! A kernel process's `kernel_run` step outcomes, cut where they publish
//! their heap pages (FIG-6193, P1).
//!
//! A host starts one kernel process whose entry sleeps twice and returns.
//! Each `kernel_run` step parks its machine and seals a CheckpointRoot in
//! its activation's spool; the step's outcome publishes that root with its
//! run fact in one fenced commit. The matrix cuts every `step.outcome`
//! commit under fail-before, abort, ack-hidden and commit-then-abort,
//! recovers on the other node, and checks:
//!
//! - a step whose outcome committed never runs again: every later outcome
//!   commit of the process lands, none refused as a repeat;
//! - a step whose outcome did not commit runs again, and the process
//!   commits exactly the uncut run's outcomes and one terminal;
//! - nothing staged is left behind: no spool outlives its activation, and
//!   no stored page is unreachable from a root.
//!
//! A second process parks inside expressions (`K-EVAL-008`): at a `sleep`
//! nested in a list (`Waiting`), then in a callee called inside it, its
//! caller standing at the call (`Called`). Every cut ends it with the
//! uncut run's result.
//!
//! Another parks at the same places inside a `while` condition, which
//! resumes in the evaluation it stood in.
//!
//! A third parks at the same places on `then yield` waits: each step that
//! delivers an outcome leaves its task delivered, owing one more turn
//! (`K-TASK-005`), and every cut of that step ends as the uncut run.
//!
//! Workflows imported from TypeScript and Python source park mid-iteration
//! in each protocol loop cursor kind (list, tuple, map and set tables,
//! text, driver), and inside an entry's run-root cleanup (`K-TASK-019`);
//! every cut ends each as its uncut run.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/kernel_process.rs"]
mod kernel_process;
#[path = "support/matrix.rs"]
mod matrix;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use matrix::MatrixTestExt as _;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Tripwire,
};
use lash_execution_contracts::backend::StoreSet;
use lash_protocol_types::sync::MutexExt as _;

/// `worker()`: two sleeps, then its answer. Three `kernel_run` steps: one
/// to each sleep and one to the return.
const WORKER: &str = r#"kernel 1
numbers float
import 0 operation typescript.sleep(milliseconds: Int) -> Null
main { do perform 0(5) as Null do perform 0(7) as Null return "done" }
"#;

/// `worker()`: a `sleep` and a call that sleeps, both inside the list
/// it returns. Three `kernel_run` steps, as `WORKER`'s.
const NESTED: &str = r#"kernel 1
numbers float
import 0 operation typescript.sleep(milliseconds: Int) -> Null
main {
  let inner = fn(x) { do perform 0(7) as Null return x }
  let v = [1, perform 0(5) as Null, apply inner("b")]
  return v
}
"#;

/// `NESTED` with each wait taking its turn once delivered: the step that
/// delivers each outcome stands `Delivered` before the task runs on
/// (`K-TASK-005`). Three `kernel_run` steps, as `WORKER`'s.
const THEN_YIELD: &str = r#"kernel 1
numbers float
import 0 operation typescript.sleep(milliseconds: Int) -> Null
main {
  let inner = fn(x) { do perform 0(7) as Null then yield return x }
  let v = [1, perform 0(5) as Null then yield, apply inner("b")]
  return v
}
"#;

/// `worker()`: a `while` whose condition, on its first evaluation, holds
/// a `sleep` and a call that sleeps, both nested (`K-EVAL-008`); its body
/// makes the second evaluation false. Three `kernel_run` steps, as
/// `WORKER`'s: one to the condition's `sleep` (`Waiting`), one to the
/// callee's (the condition standing `Called`) and one to the return.
const WHILE_CONDITION: &str = r#"kernel 1
numbers float
import 0 operation typescript.sleep(milliseconds: Int) -> Null
main {
  let inner = fn(x) { do perform 0(7) as Null return x }
  let n = true
  while if(n, [perform 0(5) as Null, apply inner(true)][1], false) { set n = false }
  return ["b", n]
}
"#;

/// TypeScript source whose entry parks mid-iteration in a protocol loop
/// over each cursor kind it has: a list, a map's and a set's tables, a
/// text and a driver stepped through `next`. Six `kernel_run` steps.
const TS_CURSORS: &str = r#"const worker = async (): Promise<string> => {
  const out = [];
  for (const x of [1, 2]) { if (x === 2) { await sleep(5); } out.push(x); }
  const m = new Map([["a", 3], ["b", 4]]);
  for (const [key, v] of m) { if (key === "b") { await sleep(5); } out.push(v); }
  const s = new Set([5, 6]);
  for (const x of s) { if (x === 6) { await sleep(5); } out.push(x); }
  for (const c of "xy") { if (c === "y") { await sleep(5); } out.push(c); }
  for (const x of [7, 8].values()) { if (x === 8) { await sleep(5); } out.push(x); }
  return out.join(",");
};
"#;

/// Python source whose entry parks mid-iteration over a tuple, then inside
/// its run-root cleanup, which cancels a task and waits while its
/// `finally` sleeps.
const PY_TUPLE_AND_CLEANUP: &str = r#"import asyncio

async def worker() -> str:
    async def child():
        try:
            await asyncio.sleep(100)
        finally:
            await asyncio.sleep(5)
    out = []
    for x in (1, 2):
        if x == 2:
            await asyncio.sleep(5)
        out.append(x)
    asyncio.ensure_future(child())
    await asyncio.sleep(1)
    return "b" + str(out[1])
"#;

/// Each program's terminal output in its uncut run, which every cut run
/// ends with.
static UNCUT: Mutex<Vec<(&'static str, String)>> = Mutex::new(Vec::new());

// Kernel-text cases need only the ordinary timer component. Dialect source
// cases also need the compiler's helper publication.
const SERVICES: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("kernel-step-services", lash::plugins::FormatVersion::ONE);

fn services() -> lash::process::StaticProcessComponent {
    let provider = lash_vm_runtime::dialect_services::Services::new(
        lash_vm_runtime::dialect_services::Dialect::TypeScript,
        SERVICES.tool_provider(lash::tool_provider_key!("timer")),
        Duration::from_secs(30),
        Arc::new(|| 0),
    )
    .unwrap();
    lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(SERVICES.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider_registration(
                lash::tool_provider_key!("timer"),
                provider.registration(),
            )
            .with_package_provider(lash::tool_provider_key!("timer"), provider),
    )
    .as_component(SERVICES.format())
    .unwrap()
}

struct KernelSteps {
    source: &'static str,
    /// The dialect `source` is imported from, or kernel text.
    language: Option<lash::workflow::document::SourceLanguage>,
    /// The step outcomes the uncut run commits.
    outcomes: usize,
    staging: tempfile::TempDir,
    tripwire: Arc<Tripwire>,
    store: Mutex<Option<lash_sqlite_store::SqliteDurableStore>>,
    backend: Mutex<Option<lash::Backend>>,
    clock: Mutex<Option<Arc<SimClock>>>,
    core: Mutex<Option<lash::LashCore>>,
    process: Mutex<Option<lash_vocabulary::identity::ProcessId>>,
}

impl KernelSteps {
    fn new(source: &'static str) -> Self {
        // OPERATION-BOUNDARIES: three VM segments and two ordinary
        // timers, each with an admitted park and a deferred completion.
        Self::imported(source, None, 3 + 2 * 2)
    }

    fn imported(
        source: &'static str,
        language: Option<lash::workflow::document::SourceLanguage>,
        outcomes: usize,
    ) -> Self {
        Self {
            source,
            language,
            outcomes,
            staging: tempfile::tempdir().expect("a staging directory"),
            tripwire: Arc::default(),
            store: Mutex::default(),
            backend: Mutex::default(),
            clock: Mutex::default(),
            core: Mutex::default(),
            process: Mutex::default(),
        }
    }

    fn backend(&self) -> lash::Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The scenario's workers, staging under its own directory.
    fn workers(&self, clock: &Arc<SimClock>) -> lash::vm::WorkerService {
        let mut config = sim::untimed_workers().config().clone();
        config.staging_dir = Some(self.staging.path().to_path_buf());
        let clock = Arc::clone(clock);
        lash::vm::WorkerService::new(config)
            .with_call_hold(Arc::new(move || Box::new(clock.hold())))
    }

    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        let clock = self
            .clock
            .lock_recover()
            .clone()
            .expect("the database is built first");
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                lash::LashCore::rlm_builder(
                    backend.clone(),
                    served::rlm(&backend, self.workers(&clock)),
                )
                .serve_sessions(false)
                .process_component(Arc::new(services()), SERVICES)
                .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
                .data_retention(lash::DataRetention::standard())
                .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                .execution_budgets(lash::ExecutionBudgets::recommended())
                .delta_coalescing(lash::DeltaCoalescing::recommended())
                .build(lash::persistence::LeaseOwnerIdentity::opaque(
                    lash::persistence::LeaseOwnerId::new("kernel-step-deployment"),
                    lash::persistence::LeaseIncarnationId::new("kernel-step-boot"),
                ))
                .expect("the core builds")
            })
            .clone()
    }

    fn process(&self) -> Option<lash_vocabulary::identity::ProcessId> {
        self.process.lock_recover().clone()
    }

    async fn start_process(&self) -> Result<lash_vocabulary::identity::ProcessId, String> {
        let core = self.core();
        let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
            .component(
                if self.language.is_some() {
                    lash::vm::KERNEL_SERVICES_CONFIG
                } else {
                    SERVICES
                },
                &(),
            )
            .build()
            .unwrap();
        let (payload, env) = match self.language {
            None => kernel_process::publish(&core, self.source, &env).await,
            Some(language) => {
                let pin = lash_core::HostArtifactPin::mint();
                let (packages, request) = kernel_process::namespace(&core, &pin, &env).await;
                let imported = core
                    .host_artifacts()
                    .import_workflow(language, self.source, &request, None)
                    .await
                    .map_err(|error| format!("import the workflow: {error}"))?;
                let lash::workflow::WorkflowImport::Imported { document, .. } = imported else {
                    return Err(format!("the workflow imports: {imported:?}"));
                };
                let start = core
                    .host_artifacts()
                    .publish_workflow(
                        &pin,
                        &document,
                        &lash::package::StartSelector::Export(lash::schema::Name::new("worker")),
                        &request,
                        None,
                        None,
                    )
                    .await
                    .map_err(|error| format!("publish the workflow: {error}"))?;
                let env = core
                    .host_artifacts()
                    .link_process_env(
                        &env,
                        &lash::process::LinkTarget::Linked(start.clone()),
                        &packages,
                        &Default::default(),
                    )
                    .await
                    .map_err(|error| format!("link the workflow: {error}"))?;
                let payload = serde_json::to_value(lash_vm_runtime::KernelProcessInput {
                    code: lash_vm_runtime::process::InlineRequest::Linked { start },
                    args: Default::default(),
                })
                .unwrap();
                (payload, env)
            }
        };
        let env_ref = core
            .host_artifacts()
            .publish_process_env(&lash_core::HostArtifactPin::mint(), &env)
            .await
            .map_err(|error| format!("publish the env: {error}"))?;
        let kind = lash_vm_runtime::LASH_VM_ENGINE_KIND;
        let settings = core
            .processes()
            .engine_defaults(kind, &payload)
            .map_err(|error| format!("the kernel's settings: {error}"))?;
        core.processes()
            .start(
                lash_execution_contracts::process::ProcessStartRequest::engine(
                    kind.to_owned(),
                    payload,
                    lash_execution_contracts::process::EngineLaunch::new(env_ref, settings),
                    lash_execution_contracts::process::ProcessOriginator::host(),
                    lash_execution_contracts::process::LifetimeDecision::Detached,
                ),
                core.effect_host(),
            )
            .await
            .map(|receipt| receipt.process_id)
            .map_err(|error| format!("start the process: {error}"))
    }

    /// The spools still in the staging directory: one directory per
    /// activation beside each parent's lock.
    #[expect(
        clippy::disallowed_methods,
        reason = "the law lists the staging directory it created and named to the pool"
    )]
    fn spools(&self) -> Vec<std::path::PathBuf> {
        let Ok(parents) = std::fs::read_dir(self.staging.path()) else {
            return Vec::new();
        };
        parents
            .filter_map(Result::ok)
            .filter_map(|parent| std::fs::read_dir(parent.path()).ok())
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect()
    }
}

#[async_trait::async_trait]
impl Scenario for KernelSteps {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        *self.clock.lock_recover() = Some(Arc::clone(&clock));
        let stores = sim::memory(clock).await;
        let store = stores.durable_store();
        *self.store.lock_recover() = Some(store.clone());
        let stores: Arc<dyn StoreSet> = Arc::new(stores);
        *self.backend.lock_recover() = Some(served::configured_backend(
            stores,
            sim::settings(),
            Vec::new(),
        ));
        Arc::new(store)
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: lash::testing::node_activation(&self.core(), Arc::new(lash_durable::NoProbe))
                .expect("the core's node activation")
                .0
                .formats()
                .decodes(),
            serves: lash::testing::node_serves(&self.core()),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        lash::testing::node_activation(&self.core(), Arc::clone(&self.tripwire) as _)
            .expect("the core's node activation")
            .1
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let process = Box::pin(self.start_process()).await?;
        *self.process.lock_recover() = Some(process);
        // A starts and claims first, B once A is settled, so the matrix
        // cuts the uncut run's writes by node.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        self.process()
            .map(|process| ActorKey::process(process.as_str()).expect("a process actor key"))
            .into_iter()
            .collect()
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let Some(actor) = self.actors().pop() else {
            return false;
        };
        matches!(
            nodes.database().actor(&actor).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let Some(actor) = self.actors().pop() else {
            return vec!["nothing was started".to_owned()];
        };
        let trace = nodes.script().trace();
        let outcomes: Vec<&lash_durable_test::Write> = trace
            .iter()
            .filter(|write| {
                write.point.label == CommitLabel::STEP_OUTCOME
                    && write.actor.as_ref() == Some(&actor)
            })
            .collect();
        let committed = outcomes.iter().filter(|write| write.committed()).count();
        if committed != self.outcomes {
            violations.push(format!(
                "OPERATION-BOUNDARIES: the process committed {committed} step outcomes, not {} (VM segments plus timer parks and completions)",
                self.outcomes
            ));
        }
        // The cut write is the one outcome that may not land: a step that
        // ran again after its outcome committed would be refused here.
        let cut_at = cut.and_then(|cut| {
            outcomes
                .iter()
                .position(|write| write.cut == Some(cut.fault) && write.node == cut.node)
        });
        for (at, write) in outcomes.iter().enumerate() {
            if !write.committed() && Some(at) != cut_at {
                violations.push(format!("a step outcome did not land: {write}"));
            }
        }
        let terminals = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::PROCESS_TERMINAL && write.committed())
            .count();
        if terminals != 1 {
            violations.push(format!("the process committed {terminals} terminals"));
        }
        if let Some(process) = self.process() {
            let terminal = self
                .backend()
                .process_registry()
                .get_process(&process)
                .await
                .ok()
                .flatten()
                .and_then(|record| record.terminal().cloned());
            let output = terminal.map(|terminal| {
                serde_json::to_string(&lash_protocol_types::Durable(&terminal.into_await_output()))
                    .unwrap()
            });
            let mut uncut = UNCUT.lock_recover();
            let seen = uncut.iter().find(|(source, _)| *source == self.source);
            match (cut, seen, output) {
                (_, _, None) => violations.push("the process has no terminal".to_owned()),
                (None, None, Some(output)) => uncut.push((self.source, output)),
                (_, Some((_, expected)), Some(output)) if *expected != output => violations.push(
                    format!("the process ended {output}, not as uncut: {expected}"),
                ),
                (Some(_), None, Some(_)) => violations.push("no uncut run ended first".to_owned()),
                _ => {}
            }
        }
        let spools = self.spools();
        if !spools.is_empty() {
            violations.push(format!("spools outlived their activations: {spools:?}"));
        }
        let store = self.store.lock_recover().clone();
        if let Some(store) = store {
            match store.audit_orphans().await {
                Ok(0) => {}
                Ok(orphans) => {
                    violations.push(format!(
                        "{orphans} stored nodes were reachable from no root"
                    ));
                }
                Err(refusal) => violations.push(format!("the orphan audit refused: {refusal}")),
            }
        }
        violations
    }
}

/// A kernel step whose outcome committed never runs again, one whose
/// outcome did not commit runs again, and nothing it staged is left
/// behind, wherever its outcome's publication was cut.
#[tokio::test]
async fn a_kernel_steps_outcome_publishes_its_pages_once_across_a_cut() {
    let report = Matrix::new()
        .labels(&[CommitLabel::STEP_OUTCOME])
        .faults(&[
            Fault::FailBefore,
            Fault::Abort,
            Fault::AckHidden,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| KernelSteps::new(WORKER))
        .await;
    eprintln!("kernel steps: {} cells", report.cells.len());
    report.assert_held();
    assert!(
        report.labels().contains(&CommitLabel::STEP_OUTCOME),
        "the matrix never cut a step outcome"
    );
}

/// `K-EVAL-008`, `K-MACH-007`: a run parked inside an expression, at
/// `Waiting` and at `Called`, resumes from its continuation on the other
/// node wherever its step's outcome was cut, and ends as the uncut run.
#[tokio::test]
async fn a_run_parked_inside_an_expression_resumes_exactly_across_a_cut() {
    let report = Matrix::new()
        .labels(&[CommitLabel::STEP_OUTCOME])
        .faults(&[
            Fault::FailBefore,
            Fault::Abort,
            Fault::AckHidden,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| KernelSteps::new(NESTED))
        .await;
    eprintln!("nested kernel steps: {} cells", report.cells.len());
    report.assert_held();
    let uncut = UNCUT.lock_recover();
    let ended = uncut.iter().find(|(source, _)| *source == NESTED);
    assert!(
        ended.is_some_and(|(_, output)| output.contains(r#""b""#)),
        "{ended:?}"
    );
}

/// `K-EVAL-008`: a run parked inside a `while` condition, at `Waiting`
/// and at `Called`, resumes in that evaluation of the condition on the
/// other node wherever its step's outcome was cut, and ends as the uncut
/// run.
#[tokio::test]
async fn a_run_parked_in_a_while_condition_resumes_exactly_across_a_cut() {
    let report = Matrix::new()
        .labels(&[CommitLabel::STEP_OUTCOME])
        .faults(&[
            Fault::FailBefore,
            Fault::Abort,
            Fault::AckHidden,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| KernelSteps::new(WHILE_CONDITION))
        .await;
    eprintln!("while condition kernel steps: {} cells", report.cells.len());
    report.assert_held();
    let uncut = UNCUT.lock_recover();
    let ended = uncut.iter().find(|(source, _)| *source == WHILE_CONDITION);
    assert!(
        ended.is_some_and(|(_, output)| output.contains(r#""b""#)),
        "{ended:?}"
    );
}

/// AWAIT-PHASE, `K-TASK-005`: a step that delivers an outcome to a
/// `then yield` wait, statement or nested, and takes the turn it owes,
/// runs again from the parked root on the other node wherever its outcome
/// was cut, and the run ends as the uncut run.
#[tokio::test]
async fn a_delivered_then_yield_wait_resumes_exactly_across_a_cut() {
    let report = Matrix::new()
        .labels(&[CommitLabel::STEP_OUTCOME])
        .faults(&[
            Fault::FailBefore,
            Fault::Abort,
            Fault::AckHidden,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| KernelSteps::new(THEN_YIELD))
        .await;
    eprintln!("then yield kernel steps: {} cells", report.cells.len());
    report.assert_held();
    let uncut = UNCUT.lock_recover();
    let ended = uncut.iter().find(|(source, _)| *source == THEN_YIELD);
    assert!(
        ended.is_some_and(|(_, output)| output.contains(r#""b""#)),
        "{ended:?}"
    );
}

/// One imported workflow's cut matrix: every cut ends as its uncut run,
/// whose terminal output holds `ends`.
async fn imported_steps_resume_exactly(
    source: &'static str,
    language: lash::workflow::document::SourceLanguage,
    outcomes: usize,
    ends: &str,
) {
    let report = Matrix::new()
        .labels(&[CommitLabel::STEP_OUTCOME])
        .faults(&[
            Fault::FailBefore,
            Fault::Abort,
            Fault::AckHidden,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| KernelSteps::imported(source, Some(language), outcomes))
        .await;
    eprintln!("imported kernel steps: {} cells", report.cells.len());
    report.assert_held();
    let uncut = UNCUT.lock_recover();
    let ended = uncut.iter().find(|(seen, _)| *seen == source);
    assert!(
        ended.is_some_and(|(_, output)| output.contains(ends)),
        "{ended:?}"
    );
}

/// PROTO-LOOP, `K-MACH-007`: a TypeScript workflow parked mid-iteration
/// over a list, a map, a set, a text and a driver resumes each loop's
/// cursor on the other node wherever its step's outcome was cut.
#[tokio::test]
async fn a_typescript_run_parked_in_each_loop_cursor_resumes_exactly_across_a_cut() {
    imported_steps_resume_exactly(
        TS_CURSORS,
        lash::workflow::document::SourceLanguage::TypeScript,
        // Six VM segments, five timer parks and five completions.
        6 + 2 * 5,
        "1,2,3,4,5,6,x,y,7,8",
    )
    .await;
}

/// PROTO-LOOP, `K-TASK-019`: a Python workflow parked mid-iteration over a
/// tuple, then inside its run-root cleanup, resumes on the other node
/// wherever its step's outcome was cut, the cleanup holding the entry's
/// return.
#[tokio::test]
async fn a_python_run_parked_in_a_tuple_loop_and_its_root_cleanup_resumes_exactly_across_a_cut() {
    imported_steps_resume_exactly(
        PY_TUPLE_AND_CLEANUP,
        lash::workflow::document::SourceLanguage::Python,
        // Four VM segments; the three timers each park and settle.
        // Root cleanup cancels the long timer and waits for finally's.
        4 + 2 * 3,
        "b2",
    )
    .await;
}

#[path = "kernel_step_crash_laws/paged_carry.rs"]
mod paged_carry;

#[path = "kernel_step_crash_laws/carried_callback.rs"]
mod carried_callback;

#[path = "support/callback_carry.rs"]
mod callback_carry;
