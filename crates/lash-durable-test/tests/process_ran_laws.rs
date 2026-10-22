//! The laws of a counted process's committed statement runs and their
//! replay (FIG-6370).
//!
//! A host starts a kernel process observed at `counts`. Its body draws
//! random bits and branches on them, parks on a sleep, reads the clock,
//! draws again and parks once more before it answers, so it runs three
//! segments, each a `kernel_run` step. The simulated nodes A and B run what
//! the core's node runs (`lash::testing::node_activation`).
//!
//! - **Committed counts:** the matrix cuts every write of the process's
//!   passes and step outcomes under fail-before, ack-hidden, zombie, abort
//!   and commit-then-abort and recovers on the other node. A segment a kill
//!   recomputes draws other bits and may take the other branch; only the
//!   accepted attempt's counts and tape commit. In every cell the
//!   `process.ran` statement/block counts sum to exactly the committed
//!   path, and each sleep action issues once, with one record per segment:
//!   a recomputed pass appends nothing.
//! - **Replay:** every committed segment replays from what the journal
//!   retains to `Verified` with its path, and the paths' histogram is the
//!   statement/block projection of the `process.ran` sum. A tape that does
//!   not answer the segment's reads diverges; a record the segment's start
//!   refers to that is no longer retained, a segment that recorded no tape,
//!   and a pruned process are `Unreplayable`. A process carried onto a
//!   successor pair replays each segment on either side of the carry.
//!   An unobserved process replays
//!   nothing and its overlay says it is unobserved, never zero.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[allow(dead_code)]
#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/kernel_process.rs"]
mod kernel_process;
#[path = "support/matrix.rs"]
mod matrix;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use matrix::MatrixTestExt as _;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::workflow::{ReplaySegments, SegmentAnswer, SegmentReplay, Unreplayable};
use lash_core::facade_support::RecordedProcessStep;
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, DurableStore};
use lash_durable_test::{Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig};
use lash_execution_contracts::backend::StoreSet;
use lash_protocol_types::sync::MutexExt as _;

/// `counted()`: a draw it branches on, a sleep, a clock read and a second
/// draw, a second sleep, then its answer: three segments.
const COUNTED: &str = r#"kernel 1
numbers float
import 0 operation typescript.sleep(milliseconds: Int) -> Null
import 1 operation typescript.clock() -> Timestamp
import 2 operation typescript.random() -> Bytes
main {
  let bits = perform 2() as Bytes
  let first = invoke bytes.at(bits, 0)
  let low = false
  if num.lt(first, 128) {
    set low = true
    set first = 25
  }
  do perform 0(5000) as Null
  let at = perform 1() as Timestamp
  let next = perform 2() as Bytes
  let second = invoke bytes.at(next, 0)
  do perform 0(7000) as Null
  return [low, first, second]
}
"#;

/// The successor `counted()` is carried onto: the same document, under
/// another env.
const SUCCESSOR_TOOL_CALLS: usize = 3;

/// How a law's process is observed and what its host does to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    /// Observed at `counts`, run to its end.
    Counted,
    /// Observed at `counts`, carried onto a successor env at its first park.
    Carried,
    /// Observed `off`.
    Unobserved,
}

/// This law needs two native declarations and the ordinary dialect services.
/// Author their complete definitions into its document, avoiding an unrelated
/// full helper namespace on every crash-matrix cell.
fn source() -> String {
    let release = lash::vm::helper_release().unwrap();
    let mut definitions = String::new();
    for name in ["num.lt", "bytes.at"] {
        let (id, function) = release
            .functions
            .iter()
            .find(|(_, function)| function.definition().name.as_str() == name)
            .unwrap();
        assert!(function.definition().body().is_none());
        definitions.push_str(&format!(
            "native @{id}\ndefinition {}\n",
            serde_json::to_string(&lash_kernel_doc::print_definition(function.definition()))
                .unwrap()
        ));
    }
    COUNTED.replace("main {", &format!("{definitions}main {{"))
}

const SERVICES: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("counted-services", lash::plugins::FormatVersion::ONE);
fn services() -> lash::process::StaticProcessComponent {
    let draws = std::sync::atomic::AtomicU64::new(0);
    let provider = lash_vm_runtime::dialect_services::Services::new(
        lash_vm_runtime::dialect_services::Dialect::TypeScript,
        SERVICES.tool_provider(lash::tool_provider_key!("services")),
        Duration::from_secs(30),
        Arc::new(move || {
            draws
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .rotate_left(63)
        }),
    )
    .unwrap();
    lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(SERVICES.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider_registration(
                lash::tool_provider_key!("services"),
                provider.registration(),
            )
            .with_package_provider(lash::tool_provider_key!("services"), provider),
    )
    .as_component(SERVICES.format())
    .unwrap()
}

/// The bounds a direct replay engine runs under.
fn bounds() -> lash_kernel_vm::Bounds {
    let pool = sim::untimed_workers().config().run_bounds;
    lash_kernel_vm::Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 1_000_000,
        memory: 64 * 1024 * 1024,
        call_depth: pool.call_depth,
        live_tasks: pool.live_tasks,
        requests_per_park: pool.requests_per_park,
        join_members: pool.join_members,
    }
}

fn env(max_tool_calls: usize) -> lash::process::ProcessEnv {
    lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(max_tool_calls))
        .component(SERVICES, &())
        .build()
        .expect("the env builds")
}

/// One deployment whose host starts one `counted()` process.
struct Counted {
    plan: Plan,
    dialect: dialect::Dialect,
    postgres_url: Option<String>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
    backend: Mutex<Option<lash::Backend>>,
    clock: Mutex<Option<Arc<SimClock>>>,
    core: Mutex<Option<lash::LashCore>>,
    process: Mutex<Option<lash_vocabulary::identity::ProcessId>>,
    tripwire: Arc<lash_durable_test::Tripwire>,
}

impl Counted {
    fn new(plan: Plan) -> Self {
        Self::on(plan, dialect::Dialect::SqliteMemory, None)
    }

    fn on(plan: Plan, dialect: dialect::Dialect, postgres_url: Option<String>) -> Self {
        Self {
            plan,
            dialect,
            postgres_url,
            keep: Mutex::default(),
            backend: Mutex::default(),
            clock: Mutex::default(),
            core: Mutex::default(),
            process: Mutex::default(),
            tripwire: Arc::default(),
        }
    }

    fn backend(&self) -> lash::Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    fn clock(&self) -> Arc<SimClock> {
        self.clock
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    fn process(&self) -> Option<lash_vocabulary::identity::ProcessId> {
        self.process.lock_recover().clone()
    }

    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        let clock = self.clock();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                let workers = sim::workers(&clock);
                let bounds = workers.config().run_bounds;
                lash::LashCore::standard_builder(backend)
                    .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
                        workers,
                        "typescript",
                        bounds,
                    )))
                    .process_component(Arc::new(services()), SERVICES)
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("process-ran-deployment"),
                        lash::persistence::LeaseIncarnationId::new("process-ran-boot"),
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    async fn publication(
        &self,
        max_tool_calls: usize,
    ) -> (serde_json::Value, lash_core::ProcessEnvRef) {
        let core = self.core();
        let (payload, linked) =
            kernel_process::publish(&core, &source(), &env(max_tool_calls)).await;
        let reference = core
            .host_artifacts()
            .publish_process_env(&lash_core::HostArtifactPin::mint(), &linked)
            .await
            .unwrap();
        (payload, reference)
    }

    /// The host's start of `counted()`, observed as the plan says.
    async fn start_process(&self) -> Result<lash_vocabulary::identity::ProcessId, String> {
        let core = self.core();
        let (payload, env_ref) = self.publication(2).await;
        let kind = lash_vm_runtime::LASH_VM_ENGINE_KIND;
        let recorded = core
            .processes()
            .kernel_engine_defaults()
            .map_err(|error| format!("the kernel's settings: {error}"))?;
        let observe = match self.plan {
            Plan::Unobserved => lash_vocabulary::observation::RunObservation::Off,
            Plan::Counted | Plan::Carried => lash_vocabulary::observation::RunObservation::Counts,
        };
        let settings = recorded.observing(observe).into_engine_settings();
        core.processes()
            .start(
                lash_execution_contracts::process::ProcessStartRequest::engine(
                    kind.to_owned(),
                    payload,
                    lash_execution_contracts::process::EngineLaunch::new(env_ref, Some(settings)),
                    lash_execution_contracts::process::ProcessOriginator::host(),
                    lash_execution_contracts::process::LifetimeDecision::Detached,
                ),
                core.effect_host(),
            )
            .await
            .map(|receipt| receipt.process_id)
            .map_err(|error| format!("start the process: {error}"))
    }

    /// Step `nodes` until the process waits on its first sleep.
    async fn first_park(&self, nodes: &SimNodes, process: &lash_vocabulary::identity::ProcessId) {
        let actor = ActorKey::process(process.as_str()).unwrap();
        tokio::time::timeout(Duration::from_secs(15), nodes.quiesce())
            .await
            .expect("the first process run quiesces");
        for _ in 0..200 {
            let state = nodes.database().actor(&actor).await.unwrap().unwrap().state;
            assert_ne!(
                state,
                ActorState::Terminal,
                "the process ended before its first park: {:?}",
                self.core().processes().get(process).await.unwrap()
            );
            let waiting = state == ActorState::Waiting;
            if waiting {
                return;
            }
            assert!(
                nodes.step().await.is_some(),
                "stalled before the first park:\n{}",
                nodes.script().rendered_trace()
            );
        }
        panic!(
            "the process did not park: {}",
            nodes.script().rendered_trace()
        );
    }

    /// Every `process.ran` record the process committed, in order.
    async fn ran_records(
        &self,
        process: &lash_vocabulary::identity::ProcessId,
    ) -> Vec<Vec<(u32, u64)>> {
        self.facts(process)
            .await
            .into_iter()
            .filter_map(|fact| match fact {
                lash_execution_contracts::process::ProcessLifecycleFact::Ran(ran) => Some(ran.ran),
                _ => None,
            })
            .collect()
    }

    /// Every fact the process committed, in order.
    async fn facts(
        &self,
        process: &lash_vocabulary::identity::ProcessId,
    ) -> Vec<lash_execution_contracts::process::ProcessLifecycleFact> {
        let read = self
            .core()
            .processes()
            .events(
                lash::process::ProcessHistoryContinuation::start(process.clone()),
                std::num::NonZeroUsize::new(500).expect("a page size"),
                lash::process::ProcessEventQueryMode::Full,
            )
            .await
            .expect("the events are read");
        let lash::process::ProcessEventReadOutcome::Retained(page) = read.outcome else {
            panic!("the events are retained");
        };
        assert!(
            matches!(page.more, lash::process::ProcessEventPageMore::Complete),
            "one page holds the process's events"
        );
        let lash::process::ProcessEventPageEvents::Full(events) = page.events else {
            panic!("a full page");
        };
        events.into_iter().map(|event| event.fact).collect()
    }

    async fn replay(
        &self,
        process: &lash_vocabulary::identity::ProcessId,
        segments: ReplaySegments,
    ) -> Vec<SegmentReplay> {
        self.core()
            .processes()
            .replay(process, segments)
            .await
            .expect("the replay answers")
    }

    async fn overlay(
        &self,
        process: &lash_vocabulary::identity::ProcessId,
    ) -> Option<lash_core::ProcessWorkflowOverlay> {
        let observation = self
            .core()
            .processes()
            .observe(process)
            .snapshot()
            .await
            .expect("the process is observed");
        match observation.read_view {
            lash_core::ProcessReadView::Retained(view) => view.workflow,
            other => panic!("the process is not retained: {other:?}"),
        }
    }

    /// The laws every run of a counted plan keeps, cut or not.
    async fn counted_laws(&self, process: &lash_vocabulary::identity::ProcessId) -> Vec<String> {
        let mut violations = Vec::new();
        let records = self.ran_records(process).await;
        let committed = histogram(records.iter().flatten().copied());
        let document =
            lash::workflow::document::parse_document(&source()).expect("the document parses");
        // Replay paths contain statement starts and empty-block entries.
        // Action issues and while-condition tests share Ran's channel but
        // have separate ordinal domains, so they do not belong in this sum.
        let statement_ordinals: BTreeSet<_> = document.site_ordinals().into_values().collect();
        let committed_statements: BTreeMap<_, _> = committed
            .iter()
            .filter_map(|(&ordinal, &count)| {
                statement_ordinals
                    .contains(&ordinal)
                    .then_some((ordinal, count))
            })
            .collect();
        // Both sleeps issue once even across cuts and carries. Check their
        // counts independently rather than discarding the non-statement bins.
        let expected_actions: BTreeMap<_, _> = document
            .action_ordinals()
            .into_values()
            .map(|ordinal| (ordinal, 1))
            .collect();
        let committed_actions: BTreeMap<_, _> = committed
            .iter()
            .filter_map(|(&ordinal, &count)| {
                expected_actions
                    .contains_key(&ordinal)
                    .then_some((ordinal, count))
            })
            .collect();
        if committed_actions != expected_actions {
            violations.push(format!(
                "the sleep actions committed {committed_actions:?}, not {expected_actions:?}"
            ));
        }
        let replayed = self.replay(process, ReplaySegments::All).await;
        if replayed.len() != 3 {
            violations.push(format!(
                "the process committed {} segments, not 3: {replayed:?}",
                replayed.len()
            ));
        }
        // One record per segment: each segment runs a statement, and a
        // recomputed pass appends nothing.
        if records.len() != replayed.len() {
            violations.push(format!(
                "{} `process.ran` records for {} segments: {records:?}",
                records.len(),
                replayed.len()
            ));
        }
        let mut path = Vec::new();
        for segment in &replayed {
            match &segment.answer {
                SegmentAnswer::Verified { path: verified } => path.extend(
                    verified
                        .tasks
                        .iter()
                        .flat_map(|task| task.starts.iter().map(|ordinal| (*ordinal, 1))),
                ),
                other => violations.push(format!(
                    "segment {} replayed {other:?}, not to its committed path",
                    segment.segment
                )),
            }
        }
        let path = histogram(path);
        if path != committed_statements {
            violations.push(format!(
                "the committed path counts {path:?}, but `process.ran` statement/block counts sum to {committed_statements:?}"
            ));
        }
        let carries = self
            .facts(process)
            .await
            .into_iter()
            .filter(|fact| {
                matches!(
                    fact,
                    lash_execution_contracts::process::ProcessLifecycleFact::Succeeded(_)
                )
            })
            .count();
        let expected = usize::from(self.plan == Plan::Carried);
        if carries != expected {
            violations.push(format!("{carries} carries committed, not {expected}"));
        }
        let overlay = self.overlay(process).await;
        match overlay {
            Some(overlay) => {
                if overlay.observation != lash_vocabulary::observation::RunObservation::Counts {
                    violations.push(format!("the overlay says {:?}", overlay.observation));
                }
                let folded: BTreeMap<u32, u64> = overlay
                    .ran
                    .iter()
                    .map(|ran| (ran.ordinal, ran.committed))
                    .collect();
                // A carry restarts the sum: its successor's ordinals may
                // name another document's sites.
                if self.plan == Plan::Counted && folded != committed {
                    violations.push(format!(
                        "the overlay folds {folded:?}, not the committed {committed:?}"
                    ));
                }
                if overlay.ran.iter().any(|ran| ran.provisional != 0) {
                    violations.push(format!(
                        "an ended process holds provisional counts: {:?}",
                        overlay.ran
                    ));
                }
            }
            None => violations.push("the process has no workflow overlay".to_owned()),
        }
        violations
    }

    /// Replays of requests the journal's records were taken from, each
    /// changed as a store that lost or never had them would hold them.
    async fn tampered_laws(&self, process: &lash_vocabulary::identity::ProcessId) -> Vec<String> {
        let mut violations = Vec::new();
        let core = self.core();
        let steps = lash_core::facade_support::read_process_steps(&core.effect_host(), process)
            .await
            .expect("the steps read");
        let record = core
            .processes()
            .get(process)
            .await
            .expect("the process reads")
            .expect("the process is retained");
        let lash_execution_contracts::process::ProcessInput::Engine {
            payload, launch, ..
        } = &record.input
        else {
            panic!("a kernel process runs an engine");
        };
        let engine = lash_vm_runtime::KernelProcessEngine::new(
            lash_vm_runtime::KernelDocuments::new(
                self.backend().module_artifacts(),
                self.backend().package_store(),
            ),
            lash_vm_runtime::TYPESCRIPT_DIALECT,
            sim::workers(&self.clock()),
            bounds(),
        )
        .expect("the shipped library assembles");
        let replay = |steps: Vec<RecordedProcessStep>, segment: u64| {
            let engine = engine.clone();
            let request = lash_vm_runtime::ProcessReplayRequest {
                pages: core.effect_host().pages().expect("the store's pages"),
                process: process.clone(),
                settings: launch.settings.clone(),
                payload: payload.clone(),
                steps,
                segments: ReplaySegments::Only(BTreeSet::from([segment])),
            };
            async move {
                let answers = engine.replay(&request).await.expect("the replay answers");
                let [answer] = answers.as_slice() else {
                    panic!("one segment asked, {answers:?} answered");
                };
                answer.answer.clone()
            }
        };
        let runs: Vec<usize> = steps
            .iter()
            .enumerate()
            .filter(|(_, step)| step.tool.ends_with("kernel_run"))
            .map(|(at, _)| at)
            .collect();
        let [entry, middle, last] = runs.as_slice() else {
            return vec![format!("{} kernel runs retained: {steps:?}", runs.len())];
        };
        let expect = |violations: &mut Vec<String>, law: &str, got: SegmentAnswer, want| {
            if got != want {
                violations.push(format!("{law}: replayed {got:?}, not {want:?}"));
            }
        };
        let verified = |answer: &SegmentAnswer| matches!(answer, SegmentAnswer::Verified { .. });

        // The untouched records verify: what follows is each change alone.
        let untouched = replay(steps.clone(), 1).await;
        if !verified(&untouched) {
            violations.push(format!(
                "the untouched middle segment replayed {untouched:?}"
            ));
        }

        // (c) A tape that answers none of the segment's reads diverges, and
        // so does one that answers another draw.
        let emptied = retaped(&steps, *middle, process, |output| {
            output["reads"] = serde_json::json!({});
        });
        expect(
            &mut violations,
            "an emptied tape",
            replay(emptied, 1).await,
            SegmentAnswer::Diverged,
        );
        let redrawn = retaped(&steps, *middle, process, |output| {
            let draws = output["reads"]["effects"]
                .as_array_mut()
                .expect("the operation tape");
            let draw = draws
                .iter_mut()
                .find(|read| read["use_"]["slot"] == 2)
                .expect("the random read");
            let bytes = &mut draw["answer"]["Ok"]["bytes"];
            let text = bytes.as_str().expect("a recorded byte draw");
            let first = u8::from_str_radix(&text[..2], 16).unwrap() ^ 128;
            *bytes = serde_json::json!(format!("{first:02x}{}", &text[2..]));
        });
        expect(
            &mut violations,
            "another draw",
            replay(redrawn, 1).await,
            SegmentAnswer::Diverged,
        );

        // (e) A segment whose start refers to an output no longer retained
        // answers `Pruned`, never a path: the entry segment's output is the
        // state the middle segment resumed.
        let mut forgotten = steps.clone();
        forgotten[*entry].outcome = None;
        expect(
            &mut violations,
            "a start whose output is gone",
            replay(forgotten, 1).await,
            SegmentAnswer::Unreplayable {
                reason: Unreplayable::Pruned,
            },
        );
        // A segment recorded before its tape and start were recorded.
        let unrecorded = retaped(&steps, *last, process, |output| {
            output.as_object_mut().unwrap().remove("reads");
            output.as_object_mut().unwrap().remove("start");
        });
        expect(
            &mut violations,
            "a segment recorded without a tape",
            replay(unrecorded, 2).await,
            SegmentAnswer::Unreplayable {
                reason: Unreplayable::NoTape,
            },
        );

        // On the heap a start names the page it resumed by the output that
        // holds it, read under that output's run fact: no carry rewrites a
        // parked page, so no start is recorded whole. A start naming a page
        // its output does not hold answers `Pruned`, never a path, and so
        // does an output whose run fact no longer records it.
        let elsewhere = retaped(&steps, *middle, process, |output| {
            output["start"]["from"]["output"]["root"] = serde_json::json!("00".repeat(32));
        });
        expect(
            &mut violations,
            "a start naming another page",
            replay(elsewhere, 1).await,
            SegmentAnswer::Unreplayable {
                reason: Unreplayable::Pruned,
            },
        );
        let mut unowned = steps.clone();
        unowned[*entry].outcome_fact = None;
        expect(
            &mut violations,
            "a start whose output's run fact is gone",
            replay(unowned, 1).await,
            SegmentAnswer::Unreplayable {
                reason: Unreplayable::Pruned,
            },
        );
        violations
    }
}

/// `steps` with the kernel output of `steps[at]` changed by `edit`.
fn retaped(
    steps: &[RecordedProcessStep],
    at: usize,
    process: &lash_vocabulary::identity::ProcessId,
    edit: impl FnOnce(&mut serde_json::Value),
) -> Vec<RecordedProcessStep> {
    let mut steps = steps.to_vec();
    let Some(lash_core_store::tool_run::SettledOutput::Completed(material)) = &steps[at].outcome
    else {
        panic!("the kernel run completed");
    };
    let mut output: serde_json::Value =
        serde_json::from_str(material.payload()).expect("the output is JSON");
    let fields = output
        .as_object_mut()
        .and_then(|output| output.values_mut().next())
        .expect("a tagged output");
    edit(fields);
    steps[at].outcome = Some(lash_core_store::tool_run::SettledOutput::Completed(
        lash_core_store::tool_run::Material::journal_local(
            lash_core_store::tool_run::MaterialOwner::Process {
                process_id: process.clone(),
            },
            lash_core_store::tool_run::MaterialRole::AttemptOutput,
            output.to_string(),
        ),
    ));
    steps
}

fn histogram(counts: impl IntoIterator<Item = (u32, u64)>) -> BTreeMap<u32, u64> {
    let mut histogram = BTreeMap::new();
    for (ordinal, count) in counts {
        *histogram.entry(ordinal).or_default() += count;
    }
    histogram
}

#[async_trait::async_trait]
impl Scenario for Counted {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        *self.clock.lock_recover() = Some(Arc::clone(&clock));
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        *self.backend.lock_recover() = Some(served::configured_backend(
            stores,
            sim::settings(),
            Vec::new(),
        ));
        database
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
            max_active: 8,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        lash::testing::node_activation(&self.core(), Arc::clone(&self.tripwire) as _)
            .expect("the core's node activation")
            .1
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        // The host is outside the deployment under test: its start is uncut.
        let process = self.start_process().await?;
        let actor = ActorKey::process(process.as_str()).unwrap();
        let facts = nodes
            .database()
            .claim_facts(None, &actor)
            .await
            .unwrap()
            .actor
            .unwrap();
        let serves = lash::testing::node_serves(&self.core());
        let missing: Vec<_> = facts
            .requires
            .iter()
            .filter(|required| !serves.contains(required))
            .collect();
        assert!(
            missing.is_empty(),
            "the authored fixture has unserved requirements: {missing:?}"
        );
        *self.process.lock_recover() = Some(process.clone());
        nodes.start("a");
        if self.plan == Plan::Carried {
            self.first_park(nodes, &process).await;
            let (payload, env) = self.publication(SUCCESSOR_TOOL_CALLS).await;
            let successor = lash::process::ProcessSuccessor::new(payload, env);
            let asked = self
                .core()
                .processes()
                .succeed(&process, successor)
                .await
                .map_err(|error| format!("the successor is refused: {error}"))?;
            if asked != lash::process::ProcessSuccession::Requested {
                return Err(format!("the successor was {asked:?}"));
            }
        }
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        self.process()
            .iter()
            .map(|process| ActorKey::process(process.as_str()).unwrap())
            .collect()
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let Some(process) = self.process() else {
            return false;
        };
        matches!(
            nodes.database().actor(&ActorKey::process(process.as_str()).unwrap()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal
        )
    }

    async fn check(&self, _nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let Some(process) = self.process() else {
            return vec!["nothing was started".to_owned()];
        };
        let record = self
            .core()
            .processes()
            .get(&process)
            .await
            .expect("the process reads")
            .expect("the process is retained");
        let mut violations = Vec::new();
        match record.lifecycle.terminal() {
            Some(lash_execution_contracts::process::ProcessTerminal::Settled { output })
                if output.is_success() => {}
            terminal => {
                violations.push(format!("the counted process did not succeed: {terminal:?}"))
            }
        }
        match self.plan {
            Plan::Counted | Plan::Carried => {
                violations.extend(self.counted_laws(&process).await);
                if cut.is_none() && self.plan == Plan::Counted {
                    violations.extend(self.tampered_laws(&process).await);
                    violations.extend(self.pruned_laws(&process).await);
                }
            }
            Plan::Unobserved => violations.extend(self.unobserved_laws(&process).await),
        }
        violations
    }
}

impl Counted {
    /// (e) A pruned process answers each segment asked of it `Pruned`, and
    /// no segment when asked for all: it retains none.
    async fn pruned_laws(&self, process: &lash_vocabulary::identity::ProcessId) -> Vec<String> {
        let cutoff = SimClock::timestamp_ms_at(self.clock().logical_ms()) + 3_600_000;
        let report = self
            .core()
            .processes()
            .prune(
                cutoff,
                None,
                lash_execution_contracts::process::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("the process prunes");
        let mut violations = Vec::new();
        if self
            .core()
            .processes()
            .get(process)
            .await
            .is_ok_and(|record| record.is_some())
        {
            return vec![format!("the ended process was not pruned: {report:?}")];
        }
        let asked = self
            .replay(process, ReplaySegments::Only(BTreeSet::from([0, 2])))
            .await;
        let pruned = SegmentAnswer::Unreplayable {
            reason: Unreplayable::Pruned,
        };
        if asked
            .iter()
            .map(|segment| segment.segment)
            .collect::<Vec<_>>()
            != [0, 2]
            || asked.iter().any(|segment| segment.answer != pruned)
        {
            violations.push(format!("a pruned process replayed {asked:?}"));
        }
        let all = self.replay(process, ReplaySegments::All).await;
        if !all.is_empty() {
            violations.push(format!("a pruned process replayed segments: {all:?}"));
        }
        violations
    }

    /// (f) An unobserved process replays nothing and its overlay says it
    /// is unobserved, never zero.
    async fn unobserved_laws(&self, process: &lash_vocabulary::identity::ProcessId) -> Vec<String> {
        let mut violations = Vec::new();
        if !self.ran_records(process).await.is_empty() {
            violations.push("an unobserved process committed counts".to_owned());
        }
        let replayed = self.replay(process, ReplaySegments::All).await;
        let unobserved = SegmentAnswer::Unreplayable {
            reason: Unreplayable::Unobserved,
        };
        if replayed.len() != 3 || replayed.iter().any(|segment| segment.answer != unobserved) {
            violations.push(format!("an unobserved process replayed {replayed:?}"));
        }
        match self.overlay(process).await {
            Some(overlay) => {
                if overlay.observation != lash_vocabulary::observation::RunObservation::Off
                    || !overlay.ran.is_empty()
                {
                    violations.push(format!(
                        "the overlay of an unobserved process says {:?} with {:?}",
                        overlay.observation, overlay.ran
                    ));
                }
            }
            None => violations.push("the process has no workflow overlay".to_owned()),
        }
        violations
    }
}

/// Committed counts and replay under every cut: each segment's counts
/// commit once, with the accepted attempt's tape, and every committed
/// segment replays to the path they count.
#[tokio::test]
async fn counted_segments_commit_once_and_replay_to_their_path_across_every_cut() {
    counted_matrix(dialect::Dialect::SqliteMemory, None).await;
}

/// The same laws on PostgreSQL, when a server is configured.
#[tokio::test]
async fn counted_segments_commit_once_and_replay_to_their_path_when_configured() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    counted_matrix(dialect::Dialect::Postgres, Some(url)).await;
}

async fn counted_matrix(dialect: dialect::Dialect, postgres_url: Option<String>) {
    let labels = [
        lash_durable::CommitLabel::PROCESS_ADVANCE,
        lash_durable::CommitLabel::STEP_OUTCOME,
    ];
    let report = Matrix::new()
        .across_nodes()
        .labels(&labels)
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| Counted::on(Plan::Counted, dialect, postgres_url.clone()))
        .await;
    eprintln!("process ran on {dialect:?}: {} cells", report.cells.len());
    report.assert_held();
    for label in labels {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// (d) A process carried onto a successor pair at a park replays each
/// segment, those before the carry and those after it.
#[tokio::test]
async fn a_carried_process_replays_each_segment() {
    Matrix::new()
        .faults(&[])
        .horizon(Duration::from_secs(600))
        .run_test(|| Counted::new(Plan::Carried))
        .await
        .assert_held();
}

/// (f) An unobserved process counts nothing and replays nothing.
#[tokio::test]
async fn an_unobserved_process_is_unobserved_never_zero() {
    Matrix::new()
        .faults(&[])
        .horizon(Duration::from_secs(600))
        .run_test(|| Counted::new(Plan::Unobserved))
        .await
        .assert_held();
}
