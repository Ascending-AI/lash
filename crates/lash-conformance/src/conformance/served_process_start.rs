//! FIG-3779: a drifted binding's recorded process start is served, and only
//! a start at the live frontier refuses.
//!
//! One RLM turn runs a cell that calls `agents.spawn` once and finishes with
//! the child's reply. The child session runs as a process on the tier's
//! process worker. Every attempt but the redrive runs under the capability
//! registry the turn was first driven with; the redrive runs under one that
//! registers another capability, so `spawn_agent`'s input schema changed and
//! the cell's `agents.spawn` binding drifted: the call is served only from
//! the journal.
//!
//! A Restate process start journals a frontier marker before it acts, then
//! registers the process and sends its workflow. Each law cuts the first
//! attempt at one point of that and redrives it under the drifted binding:
//!
//! - after the start was issued (the child is running): the recorded start is
//!   served and the turn completes with the child's reply;
//! - before the marker: the start is needed live, so every redrive parks with
//!   the binding drift and no process started;
//! - after the marker, before the registration: the marker is served but the
//!   registry holds no row for it, so the start refuses and records nothing,
//!   and every redrive parks with no process started;
//! - after the registration, before the send: the marker is served and the
//!   row is there, so the start is the recorded one and the turn completes.
//!
//! A parked law then restores the capability registry and the turn completes
//! with exactly one process started.

use crate::admit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::{SessionId, TurnId};

/// What the child is asked to do; the child's model call is the one whose
/// request carries it.
const TASK: &str = "Answer with the spawn frontier literal.";

/// What the child answers.
const CHILD_REPLY: &str = "spawn frontier literal";

fn parent_cell() -> String {
    format!(
        "<typescript>\nconst reply = await agents.spawn({{ task: {TASK:?}, capability: \"default\" }});\nfinish(reply);\n</typescript>"
    )
}

fn child_cell() -> String {
    format!("<typescript>\nfinish({CHILD_REPLY:?});\n</typescript>")
}

/// The subagent plugin a tier registers `agents.spawn` with, under the
/// capability registry the turn was first driven with and under one that
/// changed `spawn_agent`'s input schema.
#[derive(Clone)]
pub struct SubagentFactories {
    pub recorded: Arc<dyn crate::facade_support::PluginFactory>,
    pub drifted: Arc<dyn crate::facade_support::PluginFactory>,
}

/// Which capability registry an attempt runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Capabilities {
    Recorded,
    Drifted,
}

/// Where the first attempt is cut down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    /// The child is running: the start was issued in full.
    StartIssued,
    /// Before the start's frontier marker is journaled.
    BeforeMarker,
    /// After the marker, before the registry write.
    BeforeRegistration,
    /// After the registry write, before the workflow send.
    BeforeSend,
}

impl Cut {
    fn label(self) -> &'static str {
        match self {
            Self::StartIssued => "issued",
            Self::BeforeMarker => "marker",
            Self::BeforeRegistration => "register",
            Self::BeforeSend => "send",
        }
    }

    /// Whether the start had registered its process when the cut fell, so a
    /// drifted redrive serves it.
    fn served(self) -> bool {
        matches!(self, Self::StartIssued | Self::BeforeSend)
    }
}

/// Everything a law's attempts share.
#[derive(Clone)]
struct SpawnWorld {
    stores: Arc<dyn crate::StoreSet>,
    host: crate::RuntimeHostConfig,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    subagents: SubagentFactories,
    faults: crate::testing::ProcessRegistryFaults,
    registry: Arc<dyn crate::ProcessRegistry>,
    process_work: crate::ProcessWorkWiring,
    parent_calls: Arc<AtomicUsize>,
    child_calls: Arc<AtomicUsize>,
    /// Fired by the child's first model call: the start was issued in full.
    child_started: Arc<std::sync::Mutex<Option<crate::ConformanceCrash>>>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
impl SpawnWorld {
    fn new(
        effect_host: Arc<dyn crate::EffectHost>,
        stores: Arc<dyn crate::StoreSet>,
        runner: &Arc<dyn crate::ConformanceTurnRunner>,
        rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
        subagents: SubagentFactories,
    ) -> Self {
        let parent_calls = Arc::new(AtomicUsize::new(0));
        let child_calls = Arc::new(AtomicUsize::new(0));
        let child_started: Arc<std::sync::Mutex<Option<crate::ConformanceCrash>>> = Arc::default();
        let model = crate::testing::TestProvider::builder()
            .kind("stub")
            .complete({
                let parent_calls = Arc::clone(&parent_calls);
                let child_calls = Arc::clone(&child_calls);
                let child_started = Arc::clone(&child_started);
                move |request: crate::LlmRequest| {
                    let is_child = format!("{:?}", request.messages).contains(TASK);
                    let text = if is_child {
                        child_calls.fetch_add(1, Ordering::SeqCst);
                        if let Some(crash) = child_started
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take()
                        {
                            crash.fire();
                        }
                        child_cell()
                    } else {
                        parent_calls.fetch_add(1, Ordering::SeqCst);
                        parent_cell()
                    };
                    async move {
                        Ok(crate::LlmResponse {
                            parts: vec![crate::LlmOutputPart::Text {
                                text,
                                response_meta: None,
                            }],
                            ..crate::LlmResponse::default()
                        })
                    }
                }
            })
            .build();
        let mut host = crate::LawBackend::over_stores(Arc::clone(&stores), effect_host)
            .host_config(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1),
            );
        host.providers.provider_resolver =
            Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
        let faults = crate::testing::ProcessRegistryFaults::new(stores.process_registry());
        let watched = crate::facade_support::watch_process_registry(Arc::new(faults.clone()));
        let worker_factories = rlm
            .iter()
            .cloned()
            .chain([Arc::clone(&subagents.recorded)])
            .collect::<Vec<_>>();
        let worker = lash_core_worker::DurableProcessWorker::new(
            lash_core_worker::DurableProcessWorkerConfig::new(
                Arc::new(crate::facade_support::PluginHost::new(worker_factories)),
                host.clone(),
                lash_core_worker::WorkerProcessWork::SelfNative(watched.clone()),
                Arc::new(crate::NoQueuedWork::new()),
                crate::testing::runtime_lease_owner(),
            ),
        )
        .expect("build the served-process-start process worker");
        let registry = Arc::clone(watched.registry());
        let process_work = runner.process_work(watched, worker);
        Self {
            stores,
            host,
            rlm,
            subagents,
            faults,
            registry,
            process_work,
            parent_calls,
            child_calls,
            child_started,
        }
    }

    async fn runtime(
        &self,
        session_id: &SessionId,
        store: Arc<dyn crate::RuntimePersistence>,
        capabilities: Capabilities,
    ) -> crate::LashRuntime {
        let subagents = match capabilities {
            Capabilities::Recorded => Arc::clone(&self.subagents.recorded),
            Capabilities::Drifted => Arc::clone(&self.subagents.drifted),
        };
        let factories = self
            .rlm
            .iter()
            .cloned()
            .chain([subagents])
            .collect::<Vec<_>>();
        let mut policy = crate::testing::mock_session_policy();
        policy.session_id = Some(session_id.clone());
        let state = crate::RuntimeSessionState {
            session_id: session_id.clone(),
            policy: policy.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        Box::pin(
            crate::LashRuntime::builder(self.host.clone(), crate::testing::runtime_lease_owner())
                .with_session_id(session_id)
                .with_policy(policy)
                .with_initial_state(state)
                .with_plugin_factories(factories)
                .with_store(store)
                .with_process_registry(Arc::clone(&self.registry))
                .with_process_work(self.process_work.clone())
                .with_queued_work(Arc::new(crate::NoQueuedWork::new()))
                .build(),
        )
        .await
        .expect("build the served-process-start runtime")
    }

    /// The subagent processes the registry holds for `session_id`'s turn.
    async fn started(&self, session_id: &SessionId) -> Vec<crate::ProcessRecord> {
        self.registry
            .list_observed_by(
                session_id,
                &crate::ProcessListFilter {
                    status: crate::ProcessStatusFilter::Any,
                    ..Default::default()
                },
            )
            .await
            .expect("list the session's processes")
    }
}

/// What a redrive of the law's turn saw.
type Answer = Result<crate::AssembledTurn, crate::RuntimeError>;

/// One attempt at `session_id`'s turn under `capabilities`, reporting its
/// answer on `answers` when there is one: a cut attempt never answers.
fn attempt(
    world: &SpawnWorld,
    session_id: &SessionId,
    turn_id: &TurnId,
    store: &Arc<dyn crate::RuntimePersistence>,
    capabilities: Capabilities,
    answers: Option<tokio::sync::mpsc::UnboundedSender<Answer>>,
) -> crate::ConformanceTurnAttempt {
    let world = world.clone();
    let session_id = session_id.clone();
    let turn_id = turn_id.clone();
    let store = Arc::clone(store);
    Arc::new(move |scope| {
        let world = world.clone();
        let session_id = session_id.clone();
        let turn_id = turn_id.clone();
        let store = Arc::clone(&store);
        let answers = answers.clone();
        Box::pin(async move {
            let mut runtime = world.runtime(&session_id, store, capabilities).await;
            let mut input = crate::TurnInput::text("spawn the child");
            input.trace_turn_id = Some(turn_id);
            let turn = runtime
                .stream_turn(
                    input,
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            let end = crate::ConformanceTurnEnd::of(&turn);
            if let Some(answers) = answers {
                let _ = answers.send(turn);
            }
            end
        })
    })
}

async fn answer(answers: &mut tokio::sync::mpsc::UnboundedReceiver<Answer>) -> Answer {
    answers
        .recv()
        .await
        .unwrap_or_else(|| panic!("the tier's runner ran the redrive"))
}

/// The replay key of the spawn's process start in `session_id`'s turn,
/// found from a completed probe run of the same turn in a same-length
/// session: keys spell the session id, so the probe's start key names the
/// real one once its session id is substituted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn start_marker_key(
    world: &SpawnWorld,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    probe_session: &SessionId,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> String {
    assert_eq!(
        probe_session.as_str().len(),
        session_id.as_str().len(),
        "same-length session ids"
    );
    let store = crate::conformance::law_session_store(world.stores.as_ref(), probe_session).await;
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    let probe = attempt(
        world,
        probe_session,
        turn_id,
        &store,
        Capabilities::Recorded,
        Some(answers),
    );
    let scope = crate::ExecutionScope::turn(probe_session, turn_id);
    runner.run_turn(admit(scope.clone()), probe).await;
    answer(&mut answered)
        .await
        .expect("the probe turn completes");
    let keys = runner
        .recorded_replay_keys(&scope)
        .await
        .expect("the tier reads the replay keys it journaled");
    let mut markers = keys
        .iter()
        .filter(|key| key.contains(":process:start") && key.ends_with(":frontier"));
    let marker = markers
        .next()
        .unwrap_or_else(|| panic!("the probe's spawn journaled its start marker: {keys:#?}"));
    assert!(
        markers.next().is_none(),
        "the probe's turn journaled one start marker: {keys:#?}"
    );
    marker.replace(probe_session.as_str(), session_id.as_str())
}

/// Runs the law's first attempt under the recorded capabilities, cut at
/// `cut`, then one redrive under the drifted ones, and returns its answer.
async fn cut_then_redrive(
    world: &SpawnWorld,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    prefix: &str,
    cut: Cut,
) -> (
    SessionId,
    TurnId,
    Arc<dyn crate::RuntimePersistence>,
    Answer,
) {
    let turn_id = TurnId::from(format!("{prefix}-spawn-turn"));
    let session_id = SessionId::from(format!("{prefix}-{}-real", cut.label()));
    let store = crate::conformance::law_session_store(world.stores.as_ref(), &session_id).await;
    let admitted = admit(crate::ExecutionScope::turn(&session_id, &turn_id));
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    let first = attempt(
        world,
        &session_id,
        &turn_id,
        &store,
        Capabilities::Recorded,
        None,
    );
    let redrive = attempt(
        world,
        &session_id,
        &turn_id,
        &store,
        Capabilities::Drifted,
        Some(answers),
    );
    match cut {
        Cut::BeforeMarker => {
            let probe_session = SessionId::from(format!("{prefix}-{}-prob", cut.label()));
            let marker =
                start_marker_key(world, runner, &probe_session, &session_id, &turn_id).await;
            runner
                .run_cut_then_redriven_turn(
                    admitted.clone(),
                    crate::JournalCut {
                        replay_key: marker,
                        at: crate::JournalCutPoint::BeforeEffect,
                    },
                    first,
                    redrive,
                )
                .await;
        }
        Cut::StartIssued | Cut::BeforeRegistration | Cut::BeforeSend => {
            let crash = crate::ConformanceCrash::new();
            let fire = {
                let crash = crash.clone();
                Arc::new(move || crash.fire()) as Arc<dyn Fn() + Send + Sync>
            };
            let hold = match cut {
                Cut::BeforeRegistration => {
                    Some(crate::testing::RegistrationHoldPoint::BeforeRegistering)
                }
                Cut::BeforeSend => Some(crate::testing::RegistrationHoldPoint::AfterRegistering),
                _ => None,
            };
            match hold {
                Some(point) => world.faults.hold_next_registration(point, fire),
                // The child's first model call fires the crash: its process
                // was registered and its workflow sent.
                None => {
                    *world
                        .child_started
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(crash.clone());
                }
            }
            runner
                .run_turn_until_crash(admitted.clone(), first, crash)
                .await;
            runner.run_turn(admitted.clone(), redrive).await;
        }
    }
    let turn = answer(&mut answered).await;
    (session_id, turn_id, store, turn)
}

fn assert_finished_with_the_child_reply(cut: Cut, turn: Answer) {
    let turn = turn.unwrap_or_else(|error| panic!("{cut:?}: the turn completes: {error:?}"));
    let crate::TurnOutcome::Finished(finished) = &turn.outcome else {
        panic!(
            "{cut:?}: outcome {:?}; errors {:?}",
            turn.outcome, turn.errors
        );
    };
    let reply = format!("{finished:?}");
    assert!(
        reply.contains(CHILD_REPLY),
        "{cut:?}: the turn finishes with the child's reply: {reply}"
    );
}

/// One law: cut the first attempt at `cut`, redrive under the drifted
/// binding, and check the start was served or parked with nothing started.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn served_process_start_law(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    subagents: SubagentFactories,
    cut: Cut,
) {
    let world = SpawnWorld::new(effect_host, stores, &runner, rlm, subagents);
    let (session_id, turn_id, store, turn) = cut_then_redrive(&world, &runner, prefix, cut).await;
    let children_before_redrive = world.child_calls.load(Ordering::SeqCst);
    if cut.served() {
        assert_finished_with_the_child_reply(cut, turn);
        assert!(
            store
                .load_turn_park(&session_id)
                .await
                .expect("read the park")
                .is_none(),
            "{cut:?}: a served start leaves no park"
        );
    } else {
        let admitted = admit(crate::ExecutionScope::turn(&session_id, &turn_id));
        let mut turn = Some(turn);
        for redrive in ["first", "second"] {
            let error = turn
                .take()
                .expect("each redrive answered")
                .expect_err("a start needed live refuses");
            assert_eq!(
                error.code,
                crate::RuntimeErrorCode::LashlangCellBindingDrift,
                "{cut:?}, {redrive} redrive: {error:?}"
            );
            let park = store
                .load_turn_park(&session_id)
                .await
                .expect("read the park")
                .expect("the refused turn is parked");
            let crate::store::ParkReason::BindingDrift { message } = &park.reason else {
                panic!("{cut:?}: the park names the binding drift: {park:?}");
            };
            assert!(
                message.contains("`agents.spawn`") && message.contains("changed"),
                "{cut:?}: the park names the drifted binding: {message}"
            );
            assert!(
                world.started(&session_id).await.is_empty(),
                "{cut:?}, {redrive} redrive: a start needed live registers no process"
            );
            if redrive == "first" {
                let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
                runner
                    .run_turn(
                        admitted.clone(),
                        attempt(
                            &world,
                            &session_id,
                            &turn_id,
                            &store,
                            Capabilities::Drifted,
                            Some(answers),
                        ),
                    )
                    .await;
                turn = Some(answer(&mut answered).await);
            }
        }
        assert_eq!(
            world.child_calls.load(Ordering::SeqCst),
            children_before_redrive,
            "{cut:?}: no child ran while the turn was parked"
        );
        // Restoring the capabilities runs the start live, once.
        let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
        runner
            .run_turn(
                admitted,
                attempt(
                    &world,
                    &session_id,
                    &turn_id,
                    &store,
                    Capabilities::Recorded,
                    Some(answers),
                ),
            )
            .await;
        assert_finished_with_the_child_reply(cut, answer(&mut answered).await);
    }
    let started = world.started(&session_id).await;
    assert_eq!(
        started.len(),
        1,
        "{cut:?}: exactly one subagent process was started: {started:#?}"
    );
    assert!(
        started[0].is_terminal(),
        "{cut:?}: the child ran to its end: {:?}",
        started[0]
    );
    assert!(
        world.parent_calls.load(Ordering::SeqCst) >= 1,
        "the parent's model was asked"
    );
}

/// Law: a drifted `agents.spawn` whose process start was issued before the
/// turn crashed is served on the redrive, and the turn completes.
pub async fn a_drifted_spawn_whose_start_was_issued_is_served(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    subagents: SubagentFactories,
) {
    served_process_start_law(
        prefix,
        effect_host,
        stores,
        runner,
        rlm,
        subagents,
        Cut::StartIssued,
    )
    .await;
}

/// Law: a drifted `agents.spawn` cut before its start's frontier marker
/// needs its start live, so every redrive parks with no process started.
pub async fn a_drifted_spawn_cut_before_its_start_marker_parks(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    subagents: SubagentFactories,
) {
    served_process_start_law(
        prefix,
        effect_host,
        stores,
        runner,
        rlm,
        subagents,
        Cut::BeforeMarker,
    )
    .await;
}

/// Law: a drifted `agents.spawn` cut after its start's marker but before its
/// registration parks with no process started: a served marker with no row
/// is not a recorded start.
pub async fn a_drifted_spawn_cut_before_its_registration_parks(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    subagents: SubagentFactories,
) {
    served_process_start_law(
        prefix,
        effect_host,
        stores,
        runner,
        rlm,
        subagents,
        Cut::BeforeRegistration,
    )
    .await;
}

/// Law: a drifted `agents.spawn` cut after its registration but before its
/// workflow send is the recorded start: the redrive re-registers it
/// idempotently, sends its workflow and completes.
pub async fn a_drifted_spawn_cut_before_its_send_is_served(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    subagents: SubagentFactories,
) {
    served_process_start_law(
        prefix,
        effect_host,
        stores,
        runner,
        rlm,
        subagents,
        Cut::BeforeSend,
    )
    .await;
}
