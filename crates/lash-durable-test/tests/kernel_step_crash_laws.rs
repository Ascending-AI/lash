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

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/matrix.rs"]
mod matrix;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use matrix::MatrixTestExt as _;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_execution::StoreSet;
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Tripwire,
};
use lash_sansio::sync::MutexExt as _;

/// `worker()`: two sleeps, then its answer. Three `kernel_run` steps: one
/// to each sleep and one to the return.
const WORKER: &str = r#"kernel 1
numbers float
entry worker() -> Any

fn worker() {
  do sleep 5
  do sleep 7
  return "done"
}

main {
  finish null
}
"#;

/// The step outcomes the uncut run commits.
const OUTCOMES: usize = 3;

struct KernelSteps {
    staging: tempfile::TempDir,
    tripwire: Arc<Tripwire>,
    store: Mutex<Option<lash_sqlite_store::SqliteDurableStore>>,
    backend: Mutex<Option<lash::Backend>>,
    clock: Mutex<Option<Arc<SimClock>>>,
    core: Mutex<Option<lash::LashCore>>,
    process: Mutex<Option<lash_core::ProcessId>>,
}

impl KernelSteps {
    fn new() -> Self {
        Self {
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

    fn process(&self) -> Option<lash_core::ProcessId> {
        self.process.lock_recover().clone()
    }

    async fn start_process(&self) -> Result<lash_core::ProcessId, String> {
        let core = self.core();
        let document = lash::workflow::document::parse_document(WORKER)
            .map_err(|error| format!("the document parses: {error}"))?;
        let identity = lash_vm_runtime::KernelDocuments::new(self.backend().module_artifacts())
            .publish(
                &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                    lash_core::HostArtifactPin::mint(),
                ))
                .expect("a host pin is unguarded"),
                &document,
            )
            .await
            .map_err(|error| format!("publish the document: {error}"))?;
        let payload = serde_json::to_value(lash_vm_runtime::KernelProcessInput {
            document: identity,
            entry: lash::workflow::document::Name::new("worker"),
            args: serde_json::Map::new(),
        })
        .expect("the input encodes");
        let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(4))
            .build()
            .map_err(|refusal| format!("the env builds: {refusal}"))?;
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
                lash_core::ProcessStartRequest::engine(
                    kind.to_owned(),
                    payload,
                    lash_core::EngineLaunch::new(env_ref, settings),
                    lash_core::ProcessOriginator::host(),
                    lash_core::LifetimeDecision::Detached,
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
        let process = self.start_process().await?;
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
        if committed != OUTCOMES {
            violations.push(format!(
                "the process committed {committed} step outcomes, not {OUTCOMES}"
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
        .run_test(KernelSteps::new)
        .await;
    eprintln!("kernel steps: {} cells", report.cells.len());
    report.assert_held();
    assert!(
        report.labels().contains(&CommitLabel::STEP_OUTCOME),
        "the matrix never cut a step outcome"
    );
}
