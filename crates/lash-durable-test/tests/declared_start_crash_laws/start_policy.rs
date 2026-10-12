//! A start's environment policy runs once per committed declaration
//! (FIG-6034): START-POLICY-ONCE (D9) and DECLARED-SETTINGS-REALIZE (D4).
//!
//! A model start declares its child inside its call's attempt, and the
//! declaration commits with the call's outcome (`round.outcome`, a code
//! cell's call too) in the transaction that registers the child: the
//! declaration and its registration are one write. Realization is the
//! child's first pass, which builds its runtime from the registered launch.
//! Each law runs node A until its one cut kills it, then recovers on node B,
//! which runs the deployment's later build.

use super::*;

/// What the counting policy states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stated {
    /// [`limited`]`(5)`.
    First,
    /// [`limited`]`(6)`: what a changed policy states.
    Changed,
    /// A refusal.
    Refuse,
    /// A fault once, then [`Stated::First`].
    FaultOnce,
}

/// An environment of no tools whose tool-call limit is `limit`.
fn limited(limit: usize) -> lash::process::ProcessEnv {
    lash::process::ProcessEnv::builder(
        lash_core::SessionToolAccess::restricted(std::iter::empty()).expect("no tools"),
        lash::MaxToolCalls::new(limit),
    )
    .build()
    .expect("the environment builds")
}

const REFUSAL: &str = "the law's policy refuses this start";

/// A policy that counts its calls and states what [`Stated`] says.
struct Counting {
    stated: Mutex<Stated>,
    calls: AtomicUsize,
}

impl Counting {
    fn new(stated: Stated) -> Arc<Self> {
        Arc::new(Self {
            stated: Mutex::new(stated),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn state(&self, stated: Stated) {
        *self.stated.lock_recover() = stated;
    }
}

struct CountingPolicy(Arc<Counting>);

#[async_trait::async_trait]
impl lash_core::ProcessEnvPolicy for CountingPolicy {
    async fn choose(
        &self,
        cx: &lash_core::ProcessEnvCx<'_>,
    ) -> Result<lash_core::ProcessStartChoice, lash_core::ProcessEnvPolicyError> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        let stated = *self.0.stated.lock_recover();
        let limit = match stated {
            Stated::First => 5,
            Stated::Changed => 6,
            Stated::Refuse => {
                return Err(lash_core::ProcessEnvPolicyError::Refused(
                    lash::process::ProcessEnvRefusal::Policy {
                        reason: REFUSAL.to_owned(),
                    },
                ));
            }
            Stated::FaultOnce => {
                self.0.state(Stated::First);
                return Err(lash_core::ProcessEnvPolicyError::Fault(
                    "the law's policy faults once".to_owned(),
                ));
            }
        };
        Ok(lash_core::ProcessStartChoice {
            env: lash_core::ProcessEnvChoice::New(limited(limit)),
            engine_settings: cx.engine_defaults()?,
        })
    }
}

/// One node activation or another: the deployment's later build once
/// `switched`.
struct Switch {
    first: Arc<dyn Activation>,
    later: Arc<dyn Activation>,
    switched: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl Activation for Switch {
    async fn activate(&self, owned: lash_durable::runner::Owned) -> lash_durable::runner::Exit {
        if self.switched.load(Ordering::SeqCst) {
            self.later.activate(owned).await
        } else {
            self.first.activate(owned).await
        }
    }
}

const POLICY_SESSION: &str = "start-policy-parent";
const POLICY_INPUT: &str = "start-policy law: start the child";

fn policy_session() -> SessionId {
    SessionId::try_from(POLICY_SESSION.to_owned()).unwrap()
}

/// A deployment whose parent turn starts one child of a held definition
/// through `start_process`, or of a code mode closure; `later` is the core a
/// recovering node runs.
struct Deployment {
    stores: Arc<dyn StoreSet>,
    backend: lash::Backend,
    first: lash::LashCore,
    later: lash::LashCore,
    scripts: Arc<served::Scripts>,
    tripwire: Arc<Tripwire>,
    _keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
    _pin: lash_core::HostArtifactPin,
}

/// The settings each built core's kernel engine runs a code mode closure
/// under: what its node's defaults are.
#[derive(Clone, Copy)]
struct Bounds {
    first: u64,
    later: u64,
}

fn core_over(
    backend: &lash::Backend,
    scripts: &Arc<served::Scripts>,
    clock: &Arc<SimClock>,
    policy: Arc<dyn lash_core::ProcessEnvPolicy>,
    code: Option<u64>,
    boot: &str,
) -> lash::LashCore {
    let builder = match code {
        Some(instructions) => lash::LashCore::rlm_builder(
            backend.clone(),
            lash::rlm::RlmProtocolPluginFactory::new(
                lash::rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash::rlm::RlmChannel::Cell)
                    .instruction_limit(lash::rlm::InstructionBound::instructions(instructions))
                    .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                    .build(),
                lash::rlm::CellDialect::typescript(),
            )
            .with_worker_service(sim::workers(clock)),
        ),
        None => lash::LashCore::standard_builder(backend.clone()),
    };
    builder
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(served::model(Arc::clone(scripts)), served::metadata())
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                |_: &lash_core::StartCx| lash_core::Lifetime::Detached,
                policy,
            ),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new(format!("start-policy-{boot}")),
            lash::persistence::LeaseIncarnationId::new(format!("start-policy-{boot}-boot")),
        ))
        .expect("the core builds")
}

impl Deployment {
    /// A standard deployment whose model calls `start_process` on a held
    /// definition, under `policy`; or, with `bounds`, a code mode one whose
    /// cell starts a closure under the run's environment, the first build's
    /// kernel bounds `bounds.first` and the later's `bounds.later`.
    async fn new(
        clock: &Arc<SimClock>,
        policy: &Arc<Counting>,
        bounds: Option<Bounds>,
    ) -> (Self, Arc<dyn DurableStore>) {
        let keep = Mutex::default();
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) =
            dialect::open(Dialect::SqliteMemory, None, Arc::clone(clock), &keep).await;
        let backend = served::configured_backend(
            Arc::clone(&stores),
            sim::settings(),
            vec![Arc::new(lash_core_execution::testing::HeldProcessEngine)],
        );
        let scripts = Arc::new(served::Scripts::default());
        let counted =
            || Arc::new(CountingPolicy(Arc::clone(policy))) as Arc<dyn lash_core::ProcessEnvPolicy>;
        let (first, later) = match bounds {
            Some(bounds) => {
                let defaults =
                    || lash_core::runtime::env_policy::policy::from_run(|_| Ok(Vec::new()));
                (
                    core_over(
                        &backend,
                        &scripts,
                        clock,
                        defaults(),
                        Some(bounds.first),
                        "first",
                    ),
                    core_over(
                        &backend,
                        &scripts,
                        clock,
                        defaults(),
                        Some(bounds.later),
                        "later",
                    ),
                )
            }
            None => (
                core_over(&backend, &scripts, clock, counted(), None, "first"),
                core_over(&backend, &scripts, clock, counted(), None, "later"),
            ),
        };
        let pin = lash_core::HostArtifactPin::mint();
        if bounds.is_none() {
            let draft = lash_core::ProcessDefinitionDraft::new(
                lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND,
                serde_json::json!({ "fixture": "start-policy-once" }),
                [],
            )
            .expect("the held definition's draft");
            let definition = first
                .host_artifacts()
                .publish_definition(&pin, &draft)
                .await
                .expect("the held definition publishes");
            scripts.register(
                POLICY_INPUT,
                vec![
                    served::response(vec![served::call(
                        "start-policy-call",
                        "start_process",
                        serde_json::json!({ "definition_id": definition.id.to_tagged_json() }),
                    )]),
                    served::response(vec![lash_core::LlmOutputPart::Text {
                        text: PARENT_DONE.to_owned(),
                        response_meta: None,
                    }]),
                ],
            );
        } else {
            scripts.register(POLICY_INPUT, vec![served::cell(CLOSURE_CELL)]);
        }
        (
            Self {
                stores,
                backend,
                first,
                later,
                scripts,
                tripwire: Arc::default(),
                _keep: keep,
                _pin: pin,
            },
            database,
        )
    }

    /// Run the parent turn with `script`'s cut on A, `between` once A is
    /// down, and B until the turn ended. Answers the children registered.
    async fn run(
        &self,
        clock: Arc<SimClock>,
        database: Arc<dyn DurableStore>,
        cut: Option<Script>,
        between: impl FnOnce(),
    ) -> Vec<ProcessRecord> {
        let activation = |core: &lash::LashCore| {
            lash::testing::node_activation(core, Arc::clone(&self.tripwire) as _)
                .expect("the core's node activation")
                .1
        };
        let switch = Arc::new(Switch {
            first: activation(&self.first),
            later: activation(&self.later),
            switched: std::sync::atomic::AtomicBool::new(false),
        });
        let cuts = cut.is_some();
        let script = cut.unwrap_or_default();
        let nodes = Arc::new(SimNodes::new(
            database,
            Arc::clone(&clock),
            script,
            SimNodesConfig {
                lease: Matrix::test_lease(),
                decodes: lash::testing::node_activation(
                    &self.first,
                    Arc::new(lash_durable::NoProbe),
                )
                .expect("the core's node activation")
                .0
                .formats()
                .decodes(),
                serves: Vec::new(),
                max_active: 8,
            },
            Arc::clone(&switch) as Arc<dyn Activation>,
        ));
        let session = self
            .first
            .session(policy_session())
            .create(lash::SessionCreation::root(
                lash::plugins::SessionToolAccess::ambient(),
                served::spec(16),
            ))
            .await
            .expect("the parent is created");
        session
            .send(lash::TurnInput::text(POLICY_INPUT))
            .await
            .expect("the parent's input is accepted");
        let horizon = clock.logical_ms() + 600_000;
        nodes.start("a");
        nodes.quiesce().await;
        if cuts {
            while nodes.script().cuts().is_empty() {
                assert!(
                    clock.logical_ms() < horizon && nodes.step().await.is_some(),
                    "the cut never fired on A: {}",
                    nodes.script().rendered_trace()
                );
            }
            nodes.quiesce().await;
            between();
            switch.switched.store(true, Ordering::SeqCst);
            nodes.start("b");
        }
        loop {
            let ended = matches!(nodes.database().turn(&policy_session()).await, Ok(None));
            let children = registered(&self.backend).await;
            let mut passed = true;
            for child in &children {
                passed &= matches!(
                    nodes.database().actor(&process_actor(&child.id)).await,
                    Ok(Some(row)) if row.owner.is_none() && row.state != ActorState::Ready
                );
            }
            if ended && passed {
                nodes.quiesce().await;
                return children;
            }
            assert!(
                clock.logical_ms() < horizon && nodes.step().await.is_some(),
                "not done: {}",
                nodes.script().rendered_trace()
            );
        }
    }

    /// The tool results the parent's model read.
    fn read(&self) -> String {
        self.scripts.requests(POLICY_INPUT).join("\n")
    }
}

/// The parent's one call commits its outcome, whose transaction registers
/// the child: the cut lands there.
const START_OUTCOME: CommitLabel = CommitLabel::ROUND_OUTCOME;

/// START-POLICY-ONCE (D9), through the production round path.
#[tokio::test(flavor = "current_thread")]
async fn a_start_policy_runs_once_per_committed_declaration() {
    let committed = limited(5).stable_ref().unwrap();

    // A committed declaration, then a crash before the child's first pass
    // and a changed policy: recovery realizes what was declared.
    let clock = SimClock::new();
    let policy = Counting::new(Stated::First);
    let (deployment, database) = Deployment::new(&clock, &policy, None).await;
    let script = Script::new();
    script.cut_on("a", START_OUTCOME, 1, Fault::CommitThenAbort);
    let children = deployment
        .run(Arc::clone(&clock), database, Some(script), || {
            policy.state(Stated::Changed)
        })
        .await;
    let [child] = children.as_slice() else {
        panic!("one child: {children:#?}");
    };
    assert_eq!(
        child.env_ref(),
        Some(&committed),
        "the declared environment"
    );
    assert_eq!(child.engine_settings(), None, "the declared settings");
    assert_eq!(policy.calls(), 1, "recovery never re-runs the policy");

    // A crash after the policy returned, before the outcome committed: the
    // abandoned attempt registers nothing.
    let clock = SimClock::new();
    let policy = Counting::new(Stated::First);
    let (deployment, database) = Deployment::new(&clock, &policy, None).await;
    let script = Script::new();
    script.cut_on("a", START_OUTCOME, 1, Fault::Abort);
    let children = deployment
        .run(Arc::clone(&clock), database, Some(script), || {})
        .await;
    // `start_process` runs once: its interrupted attempt is answered as
    // such, and a rerun, were there one, would register its own child.
    assert!(
        (1..=2).contains(&policy.calls()),
        "the abandoned attempt's policy call and at most one rerun: {}",
        policy.calls()
    );
    assert_eq!(
        children.len(),
        policy.calls() - 1,
        "no child of the abandoned attempt: {children:#?}"
    );
    assert!(
        children
            .iter()
            .all(|child| child.env_ref() == Some(&committed))
    );

    // A refusal is the call's tool error: no intent, no child.
    let clock = SimClock::new();
    let policy = Counting::new(Stated::Refuse);
    let (deployment, database) = Deployment::new(&clock, &policy, None).await;
    let children = deployment
        .run(Arc::clone(&clock), database, None, || {})
        .await;
    assert!(children.is_empty(), "a refused start registers nothing");
    assert!(deployment.read().contains(REFUSAL), "{}", deployment.read());
    assert_eq!(policy.calls(), 1);

    // A fault retries the attempt and records no refusal.
    let clock = SimClock::new();
    let policy = Counting::new(Stated::FaultOnce);
    let (deployment, database) = Deployment::new(&clock, &policy, None).await;
    let children = deployment
        .run(Arc::clone(&clock), database, None, || {})
        .await;
    assert_eq!(
        children.len(),
        1,
        "the retried attempt starts the child: {} calls, read {}",
        policy.calls(),
        deployment.read()
    );
    assert_eq!(children[0].env_ref(), Some(&committed));
    assert_eq!(policy.calls(), 2, "the fault and its retry");
    let read = deployment.read();
    assert!(
        !read.contains("faults once") && !read.contains(REFUSAL),
        "no refusal is recorded: {read}"
    );
}

/// The closure a code mode cell starts: it spends more instructions than
/// the later build's kernel bounds allow, and answers its sum.
const CLOSURE_CELL: &str = "const handle = await processes.start({ definition: async () => { let s = 0; for (let i = 0; i < 20000; i++) { s = s + i; } return s; } });\nawait control.finish(await handle);";

/// DECLARED-SETTINGS-REALIZE (D4).
#[tokio::test(flavor = "current_thread")]
async fn a_declared_start_realizes_the_settings_its_policy_stated() {
    let bounds = Bounds {
        first: 10_000_000,
        later: 20_000,
    };
    let clock = SimClock::new();
    let policy = Counting::new(Stated::First);
    let (deployment, database) = Deployment::new(&clock, &policy, Some(bounds)).await;
    let script = Script::new();
    script.cut_on("a", START_OUTCOME, 1, Fault::CommitThenAbort);
    let children = deployment
        .run(Arc::clone(&clock), database, Some(script), || {})
        .await;
    let [child] = children.as_slice() else {
        panic!("one child: {children:#?}");
    };
    let lash_core::ProcessInput::Engine { kind, payload, .. } = child.input.as_ref() else {
        panic!("a code mode closure is an engine start: {child:#?}");
    };
    let declared = deployment
        .first
        .processes()
        .engine_defaults(kind, payload)
        .expect("the first build's defaults");
    let restarted = deployment
        .later
        .processes()
        .engine_defaults(kind, payload)
        .expect("the later build's defaults");
    assert_ne!(declared, restarted, "the restart changed the kernel bounds");
    assert_eq!(
        child.engine_settings(),
        declared.as_ref(),
        "the realized row keeps the declared settings"
    );
    let output = deployment
        .stores
        .process_registry()
        .get_process(&child.id)
        .await
        .expect("the child is read")
        .expect("the child is registered");
    assert_eq!(
        output.status(),
        ProcessStatus::Completed,
        "the child ran under the declared bounds, which its loop fits: {output:#?}"
    );
}
