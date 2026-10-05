//! The Run coordinator (FIG-4880) through a real handler on the in-process
//! Restate server double: committed finals drain every lower protected rank
//! before they issue declarations, and an attempt's bounded stream rides its
//! capture to its presentation.
//!
//! Each law runs several calls of one logical Run through `RunCoordinator`
//! inside a `LashTestHandlerHost` handler, whose own journal holds the Run's
//! records. A crash drops the attempt that hit it, and the double replays the
//! invocation into the same handler, which serves every durable record and
//! runs only the step that never became durable.
//!
//! The seeded law ports the group index's drain-transitivity oracle
//! (`effect_group_drain_transitivity`) to the Run's records: it fails when a
//! final's declarations are issued while a lower-ranked committed final, of
//! either kind, has not seated, when a call is presented out of rank order,
//! or when a Deferred descriptor takes a rank or a presentation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use lash_core::engine::{ObservationSink, ObservedEvent, ReplayKey, ShiftObservation};
use lash_core::plugin::{BehaviorRevision, PluginRevision};
use lash_core::runtime::AttemptStream;
use lash_core::store::plugin_writers::PluginCallbackIdentity;
use lash_core::tool_dispatch::{
    BeforeCheckReply, DecidedCall, DeclaredStartObligation, RunCoordinator, SingletonAttempt,
    SingletonBodyOutcome, SingletonCapture, SingletonPreparedRequest, SingletonRunError,
    SingletonTerminal, SingletonToolCall, SingletonToolHandlers,
};
use lash_core::tool_run::{
    AdmittedBinding, AfterCheckVerdict, AttemptOrdinal, AttributedVerdict, CallDecision,
    ExternalCancelPolicy, PresentationBinding, RunEvent, RunEventOrdinal, RunJournalEntry,
    RunLifecycle, RunRecord, SegmentOrdinal, ToolDeclaration,
};
use lash_core::{
    AdmittedScope, EffectOpener, Lifetime, ProcessExecutionEnvRef, ProcessId, ProcessInput,
    ProcessProvenance, ProcessStartRegistration, ScopedEffectController, StartKey, ToolCallId,
};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, ServerConfig};
use lash_sansio::{SessionStreamEvent, ToolIntentKind};

const PLUGIN: &str = "fig4880-tools";
const UNRELATED: &str = "lash:run:fig4880-unrelated:close";

fn revision() -> PluginRevision {
    PluginRevision::new(PLUGIN, BehaviorRevision::new(1).unwrap())
}

fn binding() -> AdmittedBinding {
    let callback = |key: &str| PluginCallbackIdentity {
        owner: revision(),
        key: key.to_owned(),
    };
    AdmittedBinding {
        executable: callback("tool:probe"),
        preparation: callback("tool:probe"),
        presentation: PresentationBinding {
            presenter: Some(callback("present:probe")),
            steps: Vec::new(),
        },
    }
}

fn owner() -> EffectOpener {
    EffectOpener::turn("session", "turn")
}

/// What one call's body does.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    /// Done, declaring these intents.
    Declares(Vec<ToolIntentKind>),
    /// Done, declaring one process start.
    Starts,
    /// Done, declaring nothing.
    IntentFree,
    Failed,
    Cached,
    Retry {
        after_ms: u64,
    },
    Stateful {
        key: String,
    },
    /// Parked on a Deferred source.
    Deferred,
}

fn call(label: &str, kind: &Kind) -> SingletonToolCall {
    let declaration = match kind {
        Kind::Declares(intents) => ToolDeclaration::default().with_intents(intents.iter().copied()),
        Kind::Starts => ToolDeclaration::default().with_intents([ToolIntentKind::StartProcess]),
        Kind::IntentFree
        | Kind::Failed
        | Kind::Cached
        | Kind::Retry { .. }
        | Kind::Stateful { .. } => ToolDeclaration::default(),
        Kind::Deferred => ToolDeclaration::deferring(),
    };
    SingletonToolCall {
        owner: owner(),
        segment: SegmentOrdinal(0),
        call_id: ToolCallId::fixture(label),
        tool_name: "probe".to_owned(),
        arguments: serde_json::json!({ "label": label }),
        declaration,
        binding: binding(),
        available: vec![revision()],
        cancel: ExternalCancelPolicy::Ignore,
        // A start without an environment is refused.
        environment: matches!(kind, Kind::Starts)
            .then(|| ProcessExecutionEnvRef::new("process-env:fig4977")),
    }
}

fn check_cancel_cause() -> lash_core::tool_run::HookCause {
    lash_core::tool_run::HookCause {
        error_type: "check-cancel".to_owned(),
        error_version: std::num::NonZeroU32::MIN,
        payload: serde_json::json!({ "reason": "only this call" }),
    }
}

fn output_of(call_id: &ToolCallId) -> String {
    format!("fig4880 done {call_id}")
}

/// One entry of the order in which the probe saw protected work happen.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    Unrelated,
    RealizeBegin(ToolCallId),
    RealizeEnd(ToolCallId),
    /// A declared start's launch began.
    LaunchBegin(ToolCallId),
    /// A declared start's hold discharged.
    Discharged(ToolCallId),
}

/// The callbacks of every call of one Run, with a count of every execution.
struct Probe {
    kinds: BTreeMap<ToolCallId, Kind>,
    materials: Option<Arc<dyn lash_core::store::ToolMaterialStore>>,
    sources: Mutex<BTreeMap<ToolCallId, lash_core::AwaitEventKey>>,
    complete_sources: bool,
    /// Stream events each body observes into its attempt's stream.
    streams: BTreeMap<ToolCallId, Vec<SessionStreamEvent>>,
    cancel: AtomicBool,
    cancel_before: Option<ToolCallId>,
    cancel_after: Option<ToolCallId>,
    cancelled_calls: Mutex<Vec<ToolCallId>>,
    parallel: Option<Arc<tokio::sync::Barrier>>,
    body_barrier: Option<Arc<tokio::sync::Barrier>>,
    program_release: Option<aggregate::ProgramRelease>,
    parallel_order: Vec<ToolCallId>,
    parallel_completed: std::sync::atomic::AtomicUsize,
    parallel_wake: tokio::sync::Notify,
    retry: lash_core::tool_run::RecordedRetryPolicy,
    gate: Option<(ToolCallId, ToolCallId)>,
    unavailable: Option<(ToolCallId, Arc<AtomicBool>)>,
    gate_open: AtomicBool,
    gate_after_crash: bool,
    gate_wake: tokio::sync::Notify,
    cancel_at_timer: bool,
    handler_attempts: std::sync::atomic::AtomicUsize,
    replay_delay: Option<Duration>,
    cancel_at_gate: bool,
    plugin_host: Option<Arc<lash_core::plugin::PluginHost>>,
    state_seed: Option<lash_core::plugin::PluginState>,
    plugins: Mutex<Option<Arc<lash_core::plugin::PluginSession>>>,
    executions: Mutex<Vec<(ToolCallId, AttemptOrdinal)>>,
    /// The exactly-once fence of declared intents, keyed by call and kind.
    realized: Mutex<Vec<(ToolCallId, ToolIntentKind)>>,
    /// Calls whose realization holds until the unrelated effect has run.
    held: BTreeSet<ToolCallId>,
    /// The launch registrar: a key always answers the process it got first.
    processes: Mutex<BTreeMap<StartKey, ProcessId>>,
    launches: Mutex<Vec<(ToolCallId, ProcessId)>>,
    discharges: Mutex<Vec<(ToolCallId, ProcessId, bool)>>,
    /// Calls whose launch holds until the unrelated effect has run.
    held_launch: BTreeSet<ToolCallId>,
    /// Slow external acknowledgment after the fenced outcome exists.
    held_after_realization: BTreeSet<ToolCallId>,
    unrelated: AtomicBool,
    unrelated_ran: tokio::sync::Notify,
    /// A fault the first realization of a call takes after its first
    /// intent: the step is not journaled and runs again.
    fault_after_first_intent: Option<ToolCallId>,
    faulted: AtomicBool,
    seen: Mutex<Vec<Seen>>,
    presentations: Mutex<Vec<ToolCallId>>,
    presentation_failure: bool,
    declaration_drift_on_replay: bool,
    emitted: Mutex<Vec<(ToolCallId, AttemptStream)>>,
}

#[derive(Clone, Debug)]
enum Step {
    Decide(usize),
    Concurrent,
    /// Request the Run's cancellation.
    Cancel,
    Drain,
    /// Register the unrelated effect, then drain while it progresses.
    DrainBesideUnrelated,
}

type Finished = Result<Vec<RunRecord>, SingletonRunError>;

struct Driven {
    backend: RestateTestBackend,
    finished: Arc<Mutex<Vec<Finished>>>,
    terminals: Arc<Mutex<BTreeMap<ToolCallId, SingletonTerminal>>>,
}

impl Driven {
    fn records(&self) -> Vec<RunRecord> {
        self.finished
            .lock()
            .unwrap()
            .last()
            .expect("the handler finished")
            .as_ref()
            .expect("the Run finished")
            .clone()
    }

    /// The names of the `ctx.run` records the handler's journal holds, in
    /// order.
    fn journal(&self) -> Vec<String> {
        let mut names = Vec::new();
        for view in self
            .backend
            .server()
            .invocations()
            .into_iter()
            .filter(|view| view.target.starts_with("LashTestHandlerHost/"))
        {
            for entry in self.backend.server().journal(&view.id).unwrap() {
                if entry.ty == MessageType::RunCommand {
                    names.push(entry.name.unwrap_or_default());
                }
            }
        }
        names
    }
}

/// The unrelated effect: a record of another logical Run, which closes it.
fn unrelated_record(probe: Arc<Probe>) -> lash_core::RunRecordStep<'static> {
    Box::pin(async move {
        probe.run_unrelated();
        Ok(RunJournalEntry {
            state: Vec::new(),
            record: RunRecord {
                segment: SegmentOrdinal(0),
                first: RunEventOrdinal(0),
                events: vec![RunEvent::Lifecycle {
                    state: RunLifecycle::Closing,
                }],
                trace: None,
            },
            materials: Vec::new(),
        })
    })
}

async fn drain_beside_unrelated(
    scoped: &ScopedEffectController<'_>,
    run: &mut RunCoordinator<'_>,
    probe: &Arc<Probe>,
    terminals: &Mutex<BTreeMap<ToolCallId, SingletonTerminal>>,
) -> Result<(), SingletonRunError> {
    // The unrelated effect is issued before the drain, so the journal order
    // is the same on every replay; its handle is then polled beside the
    // drain, which never waits on it.
    let mut unrelated = scoped
        .controller()
        .record_run_record(UNRELATED.to_owned(), unrelated_record(Arc::clone(probe)));
    let issued = std::future::poll_fn(|context| {
        Poll::Ready(match unrelated.as_mut().poll(context) {
            Poll::Ready(entry) => Some(entry),
            Poll::Pending => None,
        })
    })
    .await;
    let (drained, unrelated) = match issued {
        Some(entry) => (run.drain().await, entry),
        None => tokio::join!(run.drain(), unrelated),
    };
    unrelated?;
    terminals.lock().unwrap().extend(drained?);
    Ok(())
}

/// Run the program in a handler, crashing at `crashes`.
async fn drive(
    seed: u64,
    crashes: Vec<CrashPoint>,
    calls: Arc<Vec<(SingletonToolCall, Kind)>>,
    program: Arc<Vec<Step>>,
    probe: Arc<Probe>,
) -> Driven {
    let backend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .unwrap();
    backend.install_tool_realizer(probe.clone());
    for point in crashes {
        backend.server().crash_on(CrashRule::new(point));
    }
    let crash_count = lash_restate_test::CrashCount::new();
    assert!(backend.server().on_crash(crash_count.listener()));
    let release_gate = probe.gate.as_ref().map(|(_, release)| {
        let server = backend.server().clone();
        let release = release.clone();
        let probe = Arc::clone(&probe);
        tokio::spawn(async move {
            loop {
                let durable_final = server.invocations().iter().any(|view| {
                    server.journal(&view.id).unwrap().iter().any(|entry| {
                        let Some(Ok(bytes)) = entry.run_completion() else {
                            return false;
                        };
                        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                            return false;
                        };
                        let Some(record) = value.get("record") else {
                            return false;
                        };
                        let Ok(record) = serde_json::from_value::<RunRecord>(record.clone()) else {
                            return false;
                        };
                        record.events.iter().any(|event| {
                            matches!(event,
                            RunEvent::Decided { call_id, decision: CallDecision::Final { .. }, .. }
                            if *call_id == release)
                        })
                    })
                });
                if durable_final && (!probe.gate_after_crash || crash_count.get() > 0) {
                    if probe.cancel_at_gate {
                        probe.cancel.store(true, Ordering::SeqCst);
                    }
                    probe.gate_open.store(true, Ordering::SeqCst);
                    probe.gate_wake.notify_waiters();
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    });
    let complete_sources = if probe.complete_sources {
        let engine = backend.clone();
        let probe = Arc::clone(&probe);
        Some(tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            loop {
                let subscribed = engine
                    .server()
                    .invocations()
                    .iter()
                    .any(|view| view.target.ends_with("/subscribe_source"));
                if subscribed {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "Run never subscribed"
                );
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            engine.server().advance(Duration::from_secs(3601));
            engine.server().settle().await;
            assert!(
                probe.presentations.lock().unwrap().is_empty(),
                "elapsed time produces no result"
            );
            let sources = probe.sources.lock().unwrap().clone();
            for (call_id, source) in sources.iter().rev() {
                use lash_core::tool_run::{
                    MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRole,
                    SealWriter, SourceSeal,
                };
                let capture = SingletonCapture::Done {
                    output: output_of(call_id),
                    commands: Vec::new(),
                    intents: Vec::new(),
                    stream: Default::default(),
                    start: None,
                };
                let bundle = MaterialBundle::of([MaterialPayload::new(
                    MaterialOwner::Source {
                        source: source.clone(),
                    },
                    MaterialRole::AttemptOutput,
                    Some(revision()),
                    serde_json::to_string(&capture).unwrap(),
                )])
                .unwrap()
                .unwrap();
                let retained = probe
                    .materials
                    .as_ref()
                    .unwrap()
                    .retain_material(
                        &MaterialHolder::Source {
                            source: source.clone(),
                        },
                        &bundle,
                    )
                    .await
                    .unwrap();
                let reply: crate::Reply<crate::durable_wait::RestateSourceSealReply> = engine
                    .ingress()
                    .call_object_json(
                        "LashDurableWaitIndex",
                        "session",
                        "seal_source",
                        &crate::Call::new(crate::durable_wait::RestateSourceSealRequest {
                            source: source.clone(),
                            writer: SealWriter::External,
                            seal: SourceSeal::Resolved {
                                result: Box::new(retained.references[0].clone()),
                            },
                        }),
                    )
                    .await
                    .unwrap();
                assert!(matches!(
                    reply.into_body(),
                    crate::durable_wait::RestateSourceSealReply::Outcome { .. }
                ));
            }
        }))
    } else {
        None
    };
    let finished: Arc<Mutex<Vec<Finished>>> = Arc::new(Mutex::new(Vec::new()));
    let terminals = Arc::new(Mutex::new(BTreeMap::new()));
    let timer_cancel = if probe.cancel_at_timer {
        let server = backend.server().clone();
        let probe = Arc::clone(&probe);
        Some(tokio::spawn(async move {
            loop {
                let timer = server.invocations().iter().any(|view| {
                    server
                        .journal(&view.id)
                        .unwrap()
                        .iter()
                        .any(|entry| entry.ty == MessageType::SleepCommand)
                });
                if timer {
                    probe.cancel.store(true, Ordering::SeqCst);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }))
    } else {
        None
    };
    let attempt: lash_restate_test::HandlerAttempt = {
        let finished = Arc::clone(&finished);
        let terminals = Arc::clone(&terminals);
        Arc::new(move |scoped| {
            let calls = Arc::clone(&calls);
            let program = Arc::clone(&program);
            let probe = Arc::clone(&probe);
            let finished = Arc::clone(&finished);
            let terminals = Arc::clone(&terminals);
            Box::pin(async move {
                let replay = probe.handler_attempts.fetch_add(1, Ordering::SeqCst) > 0;
                let calls = if replay && probe.declaration_drift_on_replay {
                    let mut changed = (*calls).clone();
                    for (call, _) in &mut changed {
                        call.declaration.isolated = true;
                    }
                    Arc::new(changed)
                } else {
                    calls
                };
                if replay && let Some(delay) = probe.replay_delay {
                    tokio::time::sleep(delay).await;
                }
                // Independent whole/crashed executions use the same injected
                // observation time; the law compares their complete records.
                lash_core::facade_support::TraceRuntime::new(Arc::new(
                    lash_core::testing::TestClock::new(1),
                ))
                .turn_execution(&scoped);
                if let Some(host) = &probe.plugin_host {
                    let session = host
                        .isolated_registry()
                        .build_session(lash_core::plugin::PluginSessionRequest::creation(
                            "session",
                            Default::default(),
                        ))
                        .unwrap();
                    if let Some(seed) = &probe.state_seed {
                        session.hydrate_state(seed).unwrap();
                    }
                    *probe.plugins.lock().unwrap() = Some(session);
                }
                let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                let mut run = RunCoordinator::open(
                    &scoped,
                    owner(),
                    SegmentOrdinal(0),
                    calls
                        .iter()
                        .flat_map(|(call, _)| call.available.clone())
                        .collect(),
                );
                let mut outcome = Ok(());
                for step in program.iter() {
                    outcome = match step {
                        Step::Decide(index) => super::decide_round(
                            &mut run,
                            std::slice::from_ref(&calls[*index].0),
                            Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                            Default::default(),
                        )
                        .await
                        .map(|decided| {
                            if let Some(DecidedCall::Deferred { source }) =
                                decided.into_iter().next()
                            {
                                terminals.lock().unwrap().insert(
                                    calls[*index].0.call_id.clone(),
                                    SingletonTerminal::Deferred { source },
                                );
                            }
                        }),
                        Step::Concurrent => super::decide_round(
                            &mut run,
                            &round,
                            Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                            probe.retry.clone(),
                        )
                        .await
                        .map(|decisions| {
                            for (call, decision) in round.iter().zip(decisions) {
                                if let DecidedCall::Deferred { source } = decision {
                                    terminals.lock().unwrap().insert(
                                        call.call_id.clone(),
                                        SingletonTerminal::Deferred { source },
                                    );
                                }
                            }
                        }),
                        Step::Cancel => {
                            probe.cancel.store(true, Ordering::SeqCst);
                            Ok(())
                        }
                        Step::Drain => run
                            .drain()
                            .await
                            .map(|drained| terminals.lock().unwrap().extend(drained)),
                        Step::DrainBesideUnrelated => {
                            drain_beside_unrelated(&scoped, &mut run, &probe, &terminals).await
                        }
                    };
                    if outcome.is_err() {
                        break;
                    }
                }
                finished
                    .lock()
                    .unwrap()
                    .push(outcome.map(|()| run.into_records()));
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .unwrap()
    .unwrap();
    backend.server().settle().await;
    if let Some(release) = release_gate {
        release.await.unwrap();
    }
    if let Some(cancel) = timer_cancel {
        cancel.await.unwrap();
    }
    if let Some(complete) = complete_sources {
        complete.await.unwrap();
    }
    Driven {
        backend,
        finished,
        terminals,
    }
}

fn name(call_id: &ToolCallId, step: &str) -> String {
    format!("lash:run:{call_id}:{step}")
}

/// The schedule record that decides a call of a one-member round, named by
/// its first event ordinal.
fn schedule(first: u64) -> String {
    format!("lash:run:schedule:{first}")
}

/// The ported oracle: every violation of the Run's drain order in `records`.
/// A final's declarations need every lower-ranked committed final seated,
/// whether or not it declared; calls are presented in rank order; ranks are
/// reserved in decision order and a call keeps the rank it reserved; a
/// Deferred descriptor takes neither a rank nor a presentation; a call
/// decided after `cancelled_from` (a decision index) is cancelled and
/// declares nothing, while every final before it drains to its
/// presentation.
fn drain_violations(
    records: &[RunRecord],
    deferred: &BTreeSet<ToolCallId>,
    cancelled_from: Option<usize>,
) -> Vec<String> {
    let mut violations = Vec::new();
    let mut ranks: BTreeMap<ToolCallId, u64> = BTreeMap::new();
    let mut finals: BTreeMap<ToolCallId, bool> = BTreeMap::new();
    let mut seated: BTreeSet<ToolCallId> = BTreeSet::new();
    let mut presented: BTreeSet<ToolCallId> = BTreeSet::new();
    let mut issued: BTreeSet<ToolCallId> = BTreeSet::new();
    let mut decisions = 0;
    for event in records.iter().flat_map(|record| &record.events) {
        match event {
            RunEvent::Decided {
                call_id,
                rank,
                decision,
                ..
            } => {
                if deferred.contains(call_id) {
                    violations.push(format!("Deferred {call_id} was decided"));
                }
                let reserved = u64::try_from(ranks.len()).unwrap() + 1;
                if *rank != reserved {
                    violations.push(format!(
                        "{call_id} took rank {rank}, but decision order reserved rank {reserved}"
                    ));
                }
                if cancelled_from.is_some_and(|from| decisions >= from)
                    && *decision != CallDecision::Cancelled
                {
                    violations.push(format!(
                        "{call_id} was decided after cancellation as {decision:?}"
                    ));
                }
                decisions += 1;
                ranks.insert(call_id.clone(), *rank);
                if let CallDecision::Final { declares, .. } = decision {
                    finals.insert(call_id.clone(), *declares);
                    if !declares {
                        seated.insert(call_id.clone());
                    }
                }
            }
            RunEvent::DeclarationsIssued { call_id } => {
                issued.insert(call_id.clone());
                let rank = ranks.get(call_id).copied().unwrap_or(u64::MAX);
                let unseated = finals
                    .keys()
                    .filter(|other| ranks[*other] < rank && !seated.contains(*other))
                    .map(|other| format!("{other} (rank {})", ranks[other]))
                    .collect::<Vec<_>>();
                if !unseated.is_empty() {
                    violations.push(format!(
                        "{call_id} (rank {rank}) issued declarations while lower committed \
                         finals had not seated: {unseated:?}"
                    ));
                }
            }
            RunEvent::DeclarationsSettled { call_id } => {
                seated.insert(call_id.clone());
            }
            RunEvent::Presented { call_id, .. } => {
                if deferred.contains(call_id) {
                    violations.push(format!("Deferred {call_id} was presented"));
                }
                let rank = ranks.get(call_id).copied().unwrap_or(u64::MAX);
                let skipped = ranks
                    .iter()
                    .filter(|(other, other_rank)| {
                        **other_rank < rank && !presented.contains(*other)
                    })
                    .map(|(other, other_rank)| format!("{other} (rank {other_rank})"))
                    .collect::<Vec<_>>();
                if !skipped.is_empty() {
                    violations.push(format!(
                        "{call_id} (rank {rank}) was presented before lower ranks: {skipped:?}"
                    ));
                }
                presented.insert(call_id.clone());
            }
            _ => {}
        }
    }
    for (call_id, declares) in &finals {
        if *declares && !issued.contains(call_id) {
            violations.push(format!("final {call_id} never issued its declarations"));
        }
        if !presented.contains(call_id) {
            violations.push(format!("final {call_id} was never presented"));
        }
    }
    violations
}

/// L18, L04 and L03: rank 1 is committed with a blocked protected
/// declaration, rank 2 is intent-free and seats at its decision, rank 3
/// declares. Rank 3 issues nothing until rank 1 seats, while an unrelated
/// effect issued before the drain completes. A cut after every record, after
/// rank 1's first intent and before the handler's output resumes the exact
/// prefix: durable attempts never rerun, every declared intent is realized
/// once, and every call is presented once more only when its own presentation
/// was lost. A cancellation after rank 3's final cannot abandon the drain; a
/// call decided after it is cancelled and declares nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_final_drains_every_lower_rank_before_it_declares_at_every_cut() {
    let kinds = [
        Kind::Declares(vec![
            ToolIntentKind::EmitTrigger,
            ToolIntentKind::StartProcess,
        ]),
        Kind::IntentFree,
        Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
        Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
    ];
    let calls: Arc<Vec<_>> = Arc::new(
        kinds
            .iter()
            .enumerate()
            .map(|(index, kind)| (call(&format!("rank-{}", index + 1), kind), kind.clone()))
            .collect(),
    );
    let ids: Vec<ToolCallId> = calls.iter().map(|(call, _)| call.call_id.clone()).collect();
    for cancel_after_rank_3 in [false, true] {
        let mut program = vec![Step::Decide(0), Step::Decide(1), Step::Decide(2)];
        let mut steps = vec![
            name(&ids[0], "admit"),
            name(&ids[0], "attempt:1"),
            schedule(1),
            name(&ids[1], "admit"),
            name(&ids[1], "attempt:1"),
            schedule(4),
            name(&ids[2], "admit"),
            name(&ids[2], "attempt:1"),
            schedule(7),
            UNRELATED.to_owned(),
            name(&ids[0], "declare"),
            name(&ids[0], "realization:issued"),
            // Each child receipt is accepted before V settles its declarations.
            schedule(12),
            name(&ids[0], "present"),
            name(&ids[1], "present"),
            name(&ids[2], "declare"),
            name(&ids[2], "realization:issued"),
            schedule(23),
            name(&ids[2], "present"),
        ];
        if cancel_after_rank_3 {
            program.push(Step::Cancel);
        }
        program.push(Step::DrainBesideUnrelated);
        if cancel_after_rank_3 {
            program.extend([Step::Decide(3), Step::Drain]);
            steps.extend([
                name(&ids[3], "admit"),
                name(&ids[3], "attempt:1"),
                schedule(29),
                name(&ids[3], "present"),
            ]);
        }
        let program = Arc::new(program);
        let mut cuts: Vec<Option<String>> = vec![None];
        cuts.extend(steps.iter().cloned().map(Some));
        cuts.push(Some("first intent".to_owned()));
        cuts.push(Some("output".to_owned()));
        for cut in cuts {
            let mut probe = Probe::new(&calls);
            probe.held.insert(ids[0].clone());
            let crashes = match cut.as_deref() {
                None => Vec::new(),
                Some("first intent") => {
                    probe.fault_after_first_intent = Some(ids[0].clone());
                    Vec::new()
                }
                Some("output") => vec![CrashPoint::BeforeFrame {
                    ty: MessageType::OutputCommand,
                }],
                Some(step) => vec![CrashPoint::BeforeRunResult {
                    name: Some(step.to_owned()),
                }],
            };
            let probe = Arc::new(probe);
            let label = format!("cancel={cancel_after_rank_3} cut={cut:?}");
            let driven = drive(
                0x4880,
                crashes,
                Arc::clone(&calls),
                Arc::clone(&program),
                Arc::clone(&probe),
            )
            .await;
            let records = driven.records();
            assert_eq!(driven.journal(), steps, "{label}: the exact record prefix");
            let violations =
                drain_violations(&records, &BTreeSet::new(), cancel_after_rank_3.then_some(3));
            assert!(violations.is_empty(), "{label}: {violations:#?}");

            // Rank 3 began realizing only after rank 1 seated, and the
            // unrelated effect progressed while rank 1 was held.
            let seen = probe.seen();
            let first_rank_1_end = seen
                .iter()
                .position(|seen| *seen == Seen::RealizeEnd(ids[0].clone()))
                .expect("rank 1 realized");
            let unrelated = seen
                .iter()
                .position(|seen| *seen == Seen::Unrelated)
                .expect("the unrelated effect ran");
            assert!(
                unrelated < first_rank_1_end,
                "{label}: the unrelated effect progressed while rank 1 drained: {seen:?}"
            );
            for (position, entry) in seen.iter().enumerate() {
                if *entry == Seen::RealizeBegin(ids[2].clone()) {
                    assert!(
                        position > first_rank_1_end,
                        "{label}: rank 3 issued before rank 1 seated: {seen:?}"
                    );
                }
            }

            for (index, id) in ids.iter().enumerate().take(3) {
                let lost = cut.as_deref() == Some(name(id, "attempt:1").as_str());
                assert_eq!(
                    probe.executions_of(id),
                    1 + usize::from(lost),
                    "{label}: durable X never repeats for rank {}",
                    index + 1
                );
            }
            assert!(
                probe
                    .executions
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(_, attempt)| *attempt == AttemptOrdinal::FIRST),
                "{label}: unrecorded work redelivers under its attempt ordinal"
            );
            let realized = probe.realized.lock().unwrap().clone();
            assert_eq!(
                realized,
                vec![
                    (ids[0].clone(), ToolIntentKind::EmitTrigger),
                    (ids[0].clone(), ToolIntentKind::StartProcess),
                    (ids[2].clone(), ToolIntentKind::EmitTrigger),
                ],
                "{label}: each declared intent once, in rank order, none after cancellation"
            );

            let terminals = driven.terminals.lock().unwrap().clone();
            for id in [&ids[0], &ids[1], &ids[2]] {
                assert!(
                    matches!(terminals.get(id), Some(SingletonTerminal::Final { .. })),
                    "{label}: {id} is final: {:?}",
                    terminals.get(id)
                );
            }
            if cancel_after_rank_3 {
                assert_eq!(
                    terminals.get(&ids[3]),
                    Some(&SingletonTerminal::Withheld {
                        decision: CallDecision::Cancelled
                    }),
                    "{label}: a call decided after cancellation is cancelled"
                );
            }
            let mut presented = probe.presentations.lock().unwrap().clone();
            presented.dedup();
            assert_eq!(
                presented,
                ids[..3].to_vec(),
                "{label}: presentation in rank order"
            );
        }
    }
}

/// A small seeded generator: the law's interleavings replay from its seed.
#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// The law's seed: `LASH_DRAIN_LAW_SEED`, or one drawn from the clock.
#[expect(
    clippy::disallowed_methods,
    reason = "a test seed is read from the environment and the clock, and printed to replay"
)]
fn drain_law_seed() -> u64 {
    std::env::var("LASH_DRAIN_LAW_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or_else(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default();
            u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or(1) | 1
        })
}

/// L18, the ported drain-transitivity oracle: seeded rounds of 3 to 7 calls
/// of random kinds — declaring, intent-free and Deferred — whose decisions
/// interleave with drains, with the Run cancelled after a random decision in
/// some rounds. Each round runs once whole and once crashed before a random
/// record's result; both must record the same Run, and neither may violate
/// the drain order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protected_drain_is_transitive_under_seeded_interleavings() {
    let seed = drain_law_seed();
    println!("Run drain transitivity law: seed {seed} (LASH_DRAIN_LAW_SEED)");
    let mut rng = Rng(seed);
    for round in 0..6 {
        let width = 3 + rng.below(5) as usize;
        let calls: Arc<Vec<_>> = Arc::new(
            (0..width)
                .map(|position| {
                    let kind = match rng.below(10) {
                        0..=3 => Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
                        4..=7 => Kind::IntentFree,
                        _ => Kind::Deferred,
                    };
                    (call(&format!("round-{round}-{position}"), &kind), kind)
                })
                .collect(),
        );
        let deferred: BTreeSet<ToolCallId> = calls
            .iter()
            .filter(|(_, kind)| *kind == Kind::Deferred)
            .map(|(call, _)| call.call_id.clone())
            .collect();
        let cancel_at = rng.chance(30).then(|| rng.below(width as u64) as usize);
        let mut program = Vec::new();
        for index in 0..width {
            if cancel_at == Some(index) {
                program.push(Step::Cancel);
            }
            program.push(Step::Decide(index));
            if rng.chance(50) {
                program.push(Step::Drain);
            }
        }
        program.push(Step::Drain);
        let program = Arc::new(program);
        // The decision index from which calls are cancelled: Deferred calls
        // take no decision.
        let cancelled_from = cancel_at.map(|at| {
            calls[..at]
                .iter()
                .filter(|(_, kind)| *kind != Kind::Deferred)
                .count()
        });

        let whole = drive(
            seed,
            Vec::new(),
            Arc::clone(&calls),
            Arc::clone(&program),
            Arc::new(Probe::new(&calls)),
        )
        .await;
        let records = whole.records();
        let journal = whole.journal();
        let label = format!("round {round} (seed {seed}), program {program:?}");
        let violations = drain_violations(&records, &deferred, cancelled_from);
        assert!(violations.is_empty(), "{label}: {violations:#?}");

        let cut = journal[rng.below(journal.len() as u64) as usize].clone();
        let probe = Arc::new(Probe::new(&calls));
        let crashed = drive(
            seed,
            vec![CrashPoint::BeforeRunResult {
                name: Some(cut.clone()),
            }],
            Arc::clone(&calls),
            Arc::clone(&program),
            Arc::clone(&probe),
        )
        .await;
        assert_eq!(crashed.journal(), journal, "{label}, cut {cut}");
        assert_eq!(
            crashed.records(),
            records,
            "{label}, cut {cut}: the same Run"
        );
        for (call, kind) in calls.iter() {
            let lost = cut == name(&call.call_id, "attempt:1");
            assert_eq!(
                probe.executions_of(&call.call_id),
                1 + usize::from(lost),
                "{label}, cut {cut}: durable X never repeats"
            );
            if *kind == Kind::Deferred {
                assert!(
                    matches!(
                        crashed.terminals.lock().unwrap().get(&call.call_id),
                        Some(SingletonTerminal::Deferred { .. })
                    ),
                    "{label}: a descriptor is handed to its source, never presented"
                );
            }
        }
    }
}

fn delta(content: &str) -> SessionStreamEvent {
    SessionStreamEvent::TextDelta {
        content: content.to_owned(),
        block: lash_sansio::llm::types::StreamBlockIdentity::new("fig4880-block", 0),
    }
}

#[tokio::test]
async fn l01_every_admitted_body_enters_before_any_completes() {
    let calls = Arc::new(
        (0..3)
            .map(|i| {
                (
                    call(&format!("parallel-{i}"), &Kind::IntentFree),
                    Kind::IntentFree,
                )
            })
            .collect::<Vec<_>>(),
    );
    let mut probe = Probe::new(&calls);
    probe.parallel = Some(Arc::new(tokio::sync::Barrier::new(3)));
    probe.parallel_order = [2, 0, 1]
        .map(|index| calls[index].0.call_id.clone())
        .to_vec();
    let probe = Arc::new(probe);
    let driven = drive(
        4879,
        Vec::new(),
        Arc::clone(&calls),
        Arc::new(vec![Step::Concurrent, Step::Drain]),
        Arc::clone(&probe),
    )
    .await;
    assert_eq!(
        driven
            .records()
            .iter()
            .flat_map(|record| &record.events)
            .filter(|event| matches!(event, RunEvent::AttemptRecorded { .. }))
            .count(),
        3
    );
    let completed: Vec<_> = driven
        .records()
        .iter()
        .flat_map(|record| &record.events)
        .filter_map(|event| match event {
            RunEvent::AttemptRecorded { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(completed, probe.parallel_order);
    assert_eq!(
        driven.journal().len(),
        1 + 3 * calls.len(),
        "one admission and an independent X/D/V per call"
    );
    for (call, _) in calls.iter() {
        assert_eq!(probe.executions_of(&call.call_id), 1);
        assert!(
            matches!(driven.terminals.lock().unwrap().get(&call.call_id), Some(SingletonTerminal::Final { capture: SingletonCapture::Done { output, .. }, .. }) if output == &output_of(&call.call_id))
        );
    }
}

fn retry_policy() -> lash_core::tool_run::RecordedRetryPolicy {
    lash_core::tool_run::RecordedRetryPolicy::Reported {
        max_attempts: std::num::NonZeroU32::new(2).unwrap(),
        base_delay_ms: 1,
        max_delay_ms: 100,
    }
}

#[tokio::test]
async fn l02_l17_replay_registers_b2_before_waiting_for_unfinished_a1() {
    let calls = Arc::new(vec![
        (
            call("retry-a", &Kind::Retry { after_ms: 3 }),
            Kind::Retry { after_ms: 3 },
        ),
        (
            call("retry-b", &Kind::Retry { after_ms: 1 }),
            Kind::Retry { after_ms: 1 },
        ),
    ]);
    let a = calls[0].0.call_id.clone();
    let b = calls[1].0.call_id.clone();
    let program = Arc::new(vec![Step::Concurrent, Step::Drain]);
    let cuts = vec![
        None,
        Some(CrashPoint::BeforeFrame {
            ty: MessageType::SleepCommand,
        }),
        Some(CrashPoint::BeforeRunResult {
            name: Some(name(&b, "attempt:2")),
        }),
        Some(CrashPoint::BeforeRun {
            name: "lash:run:schedule:6".to_owned(),
        }),
        Some(CrashPoint::BeforeRunResult {
            name: Some(name(&a, "attempt:2")),
        }),
        Some(CrashPoint::BeforeFrame {
            ty: MessageType::OutputCommand,
        }),
    ];
    let mut reference = None;
    for (index, cut) in cuts.into_iter().enumerate() {
        let mut probe = Probe::new(&calls);
        probe.retry = retry_policy();
        probe.gate = Some((a.clone(), b.clone()));
        probe.gate_after_crash = index == 3;
        let probe = Arc::new(probe);
        let driven = drive(
            487917,
            cut.into_iter().collect(),
            Arc::clone(&calls),
            Arc::clone(&program),
            Arc::clone(&probe),
        )
        .await;
        let records = driven.records();
        let attempts: Vec<_> = records
            .iter()
            .flat_map(|record| &record.events)
            .filter_map(|event| match event {
                RunEvent::AttemptRecorded {
                    call_id, attempt, ..
                } => Some((call_id.clone(), attempt.get())),
                _ => None,
            })
            .collect();
        assert_eq!(
            attempts,
            vec![
                (b.clone(), 1),
                (b.clone(), 2),
                (a.clone(), 1),
                (a.clone(), 2)
            ],
            "B2 becomes durable before A1 is released"
        );
        let mut expected = BTreeMap::from([
            ((a.clone(), 1), 1),
            ((a.clone(), 2), 1),
            ((b.clone(), 1), 1),
            ((b.clone(), 2), 1),
        ]);
        match index {
            1 | 3 => {
                expected.insert((a.clone(), 1), 2);
            }
            2 => {
                expected.insert((a.clone(), 1), 2);
                expected.insert((b.clone(), 2), 2);
            }
            4 => {
                expected.insert((a.clone(), 2), 2);
            }
            _ => {}
        }
        let mut actual = BTreeMap::new();
        for (call, attempt) in probe.executions.lock().unwrap().iter() {
            *actual.entry((call.clone(), attempt.get())).or_insert(0) += 1;
        }
        assert_eq!(
            actual, expected,
            "cut {index}: only the unfinished receipts redeliver, under the same ordinal"
        );
        if let Some(reference) = &reference {
            assert_eq!(
                &records, reference,
                "cut {index}: opposite replay readiness preserves the complete schedule"
            );
        } else {
            reference = Some(records);
        }
    }
}

#[tokio::test]
async fn l17_two_registered_timers_replay_the_recorded_wake_order() {
    let calls = Arc::new(vec![
        (
            call("timers-a", &Kind::Retry { after_ms: 100 }),
            Kind::Retry { after_ms: 100 },
        ),
        (
            call("timers-b", &Kind::Retry { after_ms: 1 }),
            Kind::Retry { after_ms: 1 },
        ),
    ]);
    let a = calls[0].0.call_id.clone();
    let b = calls[1].0.call_id.clone();
    for (cut, unfinished_b2) in [
        (None, false),
        (
            Some(CrashPoint::BeforeRun {
                name: "lash:run:schedule:8".to_owned(),
            }),
            false,
        ),
        (
            Some(CrashPoint::BeforeRunResult {
                name: Some(name(&b, "attempt:2")),
            }),
            true,
        ),
    ] {
        let mut probe = Probe::new(&calls);
        probe.retry = retry_policy();
        // Both old deadlines expire before a cold replay registers A's
        // timer first. The already recorded B wake must still issue B2 first.
        probe.replay_delay = Some(Duration::from_millis(150));
        let probe = Arc::new(probe);
        let driven = drive(
            4879172,
            cut.into_iter().collect(),
            Arc::clone(&calls),
            Arc::new(vec![Step::Concurrent, Step::Drain]),
            Arc::clone(&probe),
        )
        .await;
        let wakes: Vec<_> = driven
            .records()
            .iter()
            .flat_map(|record| &record.events)
            .filter_map(|event| match event {
                RunEvent::RetryScheduled {
                    call_id,
                    failed,
                    next,
                    backoff_ms,
                } => Some((call_id.clone(), failed.get(), next.get(), *backoff_ms)),
                _ => None,
            })
            .collect();
        assert_eq!(wakes, vec![(b.clone(), 1, 2, 1), (a.clone(), 1, 2, 100)]);
        let mut actual = BTreeMap::new();
        for (call, attempt) in probe.executions.lock().unwrap().iter() {
            *actual.entry((call.clone(), attempt.get())).or_insert(0) += 1;
        }
        assert_eq!(
            actual,
            BTreeMap::from([
                ((a.clone(), 1), 1),
                ((a.clone(), 2), 1),
                ((b.clone(), 1), 1),
                ((b.clone(), 2), 1 + usize::from(unfinished_b2))
            ])
        );
    }
}

#[tokio::test]
async fn l03_cancel_during_registered_backoff_starts_no_next_body() {
    let kind = Kind::Retry { after_ms: 50 };
    let calls = Arc::new(vec![(call("backoff-cancel", &kind), kind)]);
    let mut probe = Probe::new(&calls);
    probe.retry = retry_policy();
    probe.cancel_at_timer = true;
    let probe = Arc::new(probe);
    let driven = drive(
        487903,
        Vec::new(),
        Arc::clone(&calls),
        Arc::new(vec![Step::Concurrent, Step::Drain]),
        Arc::clone(&probe),
    )
    .await;
    assert_eq!(probe.executions_of(&calls[0].0.call_id), 1);
    assert!(matches!(
        driven.terminals.lock().unwrap().get(&calls[0].0.call_id),
        Some(SingletonTerminal::Withheld {
            decision: CallDecision::Cancelled
        })
    ));
    assert_eq!(
        driven
            .records()
            .iter()
            .flat_map(|record| &record.events)
            .filter(|event| matches!(event, RunEvent::RetryTimerRegistered { .. }))
            .count(),
        1
    );
    assert!(
        !driven
            .records()
            .iter()
            .flat_map(|record| &record.events)
            .any(|event| matches!(event, RunEvent::RetryScheduled { .. }))
    );
}

#[tokio::test]
async fn l19_only_the_durable_selected_final_publishes_body_commands_on_cold_replay() {
    for (same_key, separate_namespace) in [(false, false), (true, false), (true, true)] {
        for crash in [false, true] {
            let mut calls = vec![
                (
                    call("state-a", &Kind::Stateful { key: "a".into() }),
                    Kind::Stateful { key: "a".into() },
                ),
                (
                    call(
                        "state-b",
                        &Kind::Stateful {
                            key: if same_key { "a" } else { "b" }.into(),
                        },
                    ),
                    Kind::Stateful {
                        key: if same_key { "a" } else { "b" }.into(),
                    },
                ),
            ];
            let other = "fig4880-other";
            if separate_namespace {
                let revision = PluginRevision::new(other, revision().behavior_revision);
                calls[1].0.binding.executable.owner = revision.clone();
                calls[1].0.binding.preparation.owner = revision.clone();
                calls[1]
                    .0
                    .binding
                    .presentation
                    .presenter
                    .as_mut()
                    .unwrap()
                    .owner = revision.clone();
                calls[1].0.available.push(revision);
            }
            let calls = Arc::new(calls);
            let reductions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = Arc::clone(&reductions);
            let spec = lash_core::plugin::PluginSpec::new().with_state_reducer(
                "append",
                Arc::new(move |input: lash_core::plugin::StateReduction<'_>| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(serde_json::json!(format!(
                        "{}{}",
                        input
                            .current
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or(""),
                        input.input.as_str().unwrap()
                    ))))
                }),
            );
            let mut factories = lash_core::testing::test_standard_protocol_factories();
            factories.push(Arc::new(lash_core::plugin::StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial(PLUGIN),
                spec.clone(),
            )));
            factories.push(Arc::new(lash_core::plugin::StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial(other),
                spec,
            )));
            let host = lash_core::plugin::PluginHost::new(factories);
            let mut probe = Probe::new(&calls);
            probe.gate = Some((calls[0].0.call_id.clone(), calls[1].0.call_id.clone()));
            probe.cancel_at_gate = true;
            probe.gate_after_crash = crash;
            probe.plugin_host = Some(Arc::new(host));
            let probe = Arc::new(probe);
            let cut = crash.then(|| CrashPoint::BeforeRun {
                name: "lash:run:schedule:3".to_owned(),
            });
            let driven = drive(
                487919,
                cut.into_iter().collect(),
                Arc::clone(&calls),
                Arc::new(vec![Step::Concurrent, Step::Drain]),
                Arc::clone(&probe),
            )
            .await;
            let state = probe.plugin_session().unwrap().export_state();
            let values = &state.plugins[if separate_namespace { other } else { PLUGIN }].values;
            if separate_namespace {
                assert!(
                    state
                        .plugins
                        .get(PLUGIN)
                        .is_none_or(|namespace| namespace.values.is_empty())
                );
            }
            assert_eq!(values.len(), 1);
            assert_eq!(
                values[if same_key { "a" } else { "b" }],
                serde_json::json!(calls[1].0.call_id.to_string())
            );
            assert_eq!(
                reductions.load(Ordering::SeqCst),
                1,
                "a durable final replays its resolution without a reducer; cancellation discards the sibling's commands"
            );
            assert!(matches!(
                driven.terminals.lock().unwrap().get(&calls[0].0.call_id),
                Some(SingletonTerminal::Withheld {
                    decision: CallDecision::Cancelled
                })
            ));
            assert_eq!(probe.executions_of(&calls[1].0.call_id), 1);
            assert_eq!(
                probe.executions_of(&calls[0].0.call_id),
                1 + usize::from(crash)
            );
        }
    }
}

#[tokio::test]
async fn l05_empty_and_cached_rounds_admit_every_operand_before_a_decision() {
    for kinds in [vec![], vec![Kind::Cached, Kind::IntentFree, Kind::Deferred]] {
        let calls = Arc::new(
            kinds
                .into_iter()
                .enumerate()
                .map(|(i, kind)| (call(&format!("cached-{i}"), &kind), kind))
                .collect::<Vec<_>>(),
        );
        let probe = Arc::new(Probe::new(&calls));
        let driven = drive(
            487905,
            Vec::new(),
            Arc::clone(&calls),
            Arc::new(vec![Step::Concurrent, Step::Drain]),
            Arc::clone(&probe),
        )
        .await;
        let records = driven.records();
        if calls.is_empty() {
            assert!(records.is_empty());
            continue;
        }
        assert!(
            matches!(records[0].events.as_slice(), [RunEvent::Admitted { round }] if round.members.len() == calls.len())
        );
        assert_eq!(probe.executions_of(&calls[0].0.call_id), 0);
        assert_eq!(probe.executions_of(&calls[1].0.call_id), 1);
        assert_eq!(probe.executions_of(&calls[2].0.call_id), 1);
        assert!(matches!(
            driven.terminals.lock().unwrap().get(&calls[2].0.call_id),
            Some(SingletonTerminal::Deferred { .. })
        ));
    }
}

mod aggregate;
mod realization;

#[tokio::test]
async fn l12_recorded_admission_ignores_live_isolation_drift() {
    let calls = Arc::new(vec![(
        call("catalog-drift", &Kind::IntentFree),
        Kind::IntentFree,
    )]);
    let mut probe = Probe::new(&calls);
    probe.declaration_drift_on_replay = true;
    let probe = Arc::new(probe);
    let driven = drive(
        492601,
        // X is durable once its schedule record runs; lose that record's
        // result so replay decides again from the recorded admission.
        vec![CrashPoint::BeforeRunResult {
            name: Some(schedule(1)),
        }],
        Arc::clone(&calls),
        Arc::new(vec![Step::Decide(0), Step::Drain]),
        Arc::clone(&probe),
    )
    .await;
    // A, then X folded into its deciding schedule record, then V.
    assert_eq!(driven.records().len(), 3);
    assert_eq!(probe.executions_of(&calls[0].0.call_id), 1);
}

#[tokio::test]
async fn l04_stream_publishes_only_after_presentation_acceptance() {
    for withheld in [false, true] {
        let kind = Kind::IntentFree;
        let calls = Arc::new(vec![(call("stream-acceptance", &kind), kind)]);
        let mut probe = Probe::new(&calls);
        probe
            .streams
            .insert(calls[0].0.call_id.clone(), vec![delta("captured")]);
        let probe = Arc::new(probe);
        let program = if withheld {
            vec![Step::Cancel, Step::Decide(0), Step::Drain]
        } else {
            vec![Step::Decide(0), Step::Drain]
        };
        let driven = drive(
            492608,
            // V is the schedule record after the three per-call records.
            vec![CrashPoint::BeforeRunResult {
                name: Some("lash:run:schedule:3".to_owned()),
            }],
            Arc::clone(&calls),
            Arc::new(program),
            Arc::clone(&probe),
        )
        .await;
        driven.records();
        assert_eq!(
            probe.emitted.lock().unwrap().len(),
            1,
            "an unaccepted V never publishes its stream, including withheld calls"
        );
        assert_eq!(probe.executions_of(&calls[0].0.call_id), 1);
    }
}

#[tokio::test]
async fn l04_presentation_refusal_records_fallback_and_finishes_drain() {
    let kind = Kind::Declares(vec![ToolIntentKind::EmitTrigger]);
    let calls = Arc::new(vec![(call("presentation-fallback", &kind), kind)]);
    let mut probe = Probe::new(&calls);
    probe.presentation_failure = true;
    let probe = Arc::new(probe);
    let driven = drive(
        492609,
        vec![CrashPoint::BeforeFrame {
            ty: MessageType::OutputCommand,
        }],
        Arc::clone(&calls),
        Arc::new(vec![Step::Decide(0), Step::Drain]),
        Arc::clone(&probe),
    )
    .await;
    let records = driven.records();
    assert!(records.iter().flat_map(|record| &record.events).any(|event|
        matches!(event, RunEvent::Incorporated { call_id } if *call_id == calls[0].0.call_id)));
    assert_eq!(
        probe.presentations.lock().unwrap().len(),
        1,
        "a recorded fallback never repeats the failed presenter"
    );
    assert_eq!(probe.realized.lock().unwrap().len(), 1);
    assert!(records.iter().flat_map(|record| &record.events).any(|event|
        matches!(event, RunEvent::Presented { failure: Some(cause), .. }
            if cause.error_type == "fig4926.presentation_refused" && cause.payload["reason"] == "deterministic presentation refusal")));
    let terminals = driven.terminals.lock().unwrap();
    assert!(
        matches!(&terminals[&calls[0].0.call_id], SingletonTerminal::Final { presentation, capture, .. }
        if Some(presentation.as_str()) == capture.output())
    );
}

#[tokio::test]
async fn l15_admission_owns_one_namespace_image_for_a_wide_round() {
    let calls = Arc::new(
        (0..16)
            .map(|i| {
                (
                    call(&format!("snapshot-{i}"), &Kind::IntentFree),
                    Kind::IntentFree,
                )
            })
            .collect::<Vec<_>>(),
    );
    let mut factories = lash_core::testing::test_standard_protocol_factories();
    factories.push(Arc::new(lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial(PLUGIN),
        lash_core::plugin::PluginSpec::new(),
    )));
    let mut probe = Probe::new(&calls);
    probe.plugin_host = Some(Arc::new(lash_core::plugin::PluginHost::new(factories)));
    let mut namespace = lash_core::plugin::PluginNamespaceState::default();
    for index in 0..4 {
        namespace.values.insert(
            format!("payload-{index}"),
            serde_json::json!("x".repeat(31 * 1024)),
        );
    }
    probe.state_seed = Some(lash_core::plugin::PluginState {
        plugins: BTreeMap::from([(PLUGIN.into(), namespace)]),
    });
    let driven = drive(
        492607,
        Vec::new(),
        calls,
        Arc::new(vec![Step::Concurrent, Step::Drain]),
        Arc::new(probe),
    )
    .await;
    driven.records();
    let admissions: Vec<_> = driven
        .backend
        .server()
        .invocations()
        .iter()
        .flat_map(|view| driven.backend.server().journal(&view.id).unwrap())
        .filter_map(|entry| {
            let bytes = entry.run_completion()?.ok()?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            let record: RunRecord = serde_json::from_value(value.get("record")?.clone()).ok()?;
            record
                .events
                .iter()
                .any(|event| matches!(event, RunEvent::Admitted { .. }))
                .then_some(bytes)
        })
        .collect();
    assert_eq!(admissions.len(), 1);
    let text = String::from_utf8(admissions[0].to_vec()).unwrap();
    assert!(
        text.len() < 160 * 1024,
        "the admission byte cost contains one image and bounded references, got {}",
        text.len()
    );
    assert_eq!(
        text.matches("generation").count(),
        1,
        "sixteen members reference one canonical namespace image"
    );
}
pub(super) mod owner_park;
mod process_continuation;
mod turn_handover;

/// L03/L07, FIG-4924: a cancel arriving after a subscribed worker dies must
/// not replace its recorded subscription with a seal command on replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l03_deferred_replay_after_cancel_keeps_the_subscription_and_source_winner() {
    for resolved in [false, true] {
        let backend = lash_restate_test::backend(0x4924, ServerConfig::default())
            .await
            .unwrap();
        let call = call("cancel-after-subscribe", &Kind::Deferred);
        let mut probe = Probe::new(&[(call.clone(), Kind::Deferred)]);
        probe.materials = Some(backend.stores().process_env_store());
        let probe = Arc::new(probe);
        let replay = Arc::new(tokio::sync::Semaphore::new(0));
        let records = Arc::new(Mutex::new(Vec::new()));
        let attempt: lash_restate_test::HandlerAttempt = {
            let probe = Arc::clone(&probe);
            let replay = Arc::clone(&replay);
            let records = Arc::clone(&records);
            let call = call.clone();
            Arc::new(move |scoped| {
                let probe = Arc::clone(&probe);
                let replay = Arc::clone(&replay);
                let records = Arc::clone(&records);
                let call = call.clone();
                Box::pin(async move {
                    if probe.handler_attempts.fetch_add(1, Ordering::SeqCst) > 0 {
                        replay.acquire().await.unwrap().forget();
                    }
                    let mut run =
                        RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                    assert!(matches!(
                        super::decide_round(
                            &mut run,
                            std::slice::from_ref(&call),
                            Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                            Default::default(),
                        )
                        .await
                        .unwrap()
                        .as_slice(),
                        [DecidedCall::Deferred { .. }]
                    ));
                    run.await_deferred().await.unwrap();
                    run.drain().await.unwrap();
                    records.lock().unwrap().extend(run.into_records());
                })
            })
        };
        let cancel_after_crash = async {
            let invocation = loop {
                if let Some(view) = backend.server().invocations().into_iter().find(|view| {
                    view.target.starts_with("LashTestHandlerHost/")
                        && backend
                            .server()
                            .journal(&view.id)
                            .unwrap()
                            .iter()
                            .any(|entry| {
                                entry
                                    .call_command()
                                    .is_some_and(|call| call.handler_name == "subscribe_source")
                            })
                }) {
                    break view;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            };
            let prefix: Vec<_> = backend
                .server()
                .journal(&invocation.id)
                .unwrap()
                .into_iter()
                .filter(|entry| entry.ty.is_command())
                .collect();
            assert!(backend.server().crash(&invocation.id));
            while probe.handler_attempts.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            if resolved {
                use lash_core::tool_run::{
                    MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRole,
                    SealWriter, SourceSeal,
                };
                let source = probe.sources.lock().unwrap()[&call.call_id].clone();
                let capture = SingletonCapture::Done {
                    output: output_of(&call.call_id),
                    commands: Vec::new(),
                    intents: Vec::new(),
                    stream: Default::default(),
                    start: None,
                };
                let bundle = MaterialBundle::of([MaterialPayload::new(
                    MaterialOwner::Source {
                        source: source.clone(),
                    },
                    MaterialRole::AttemptOutput,
                    Some(revision()),
                    serde_json::to_string(&capture).unwrap(),
                )])
                .unwrap()
                .unwrap();
                let retained = probe
                    .materials
                    .as_ref()
                    .unwrap()
                    .retain_material(
                        &MaterialHolder::Source {
                            source: source.clone(),
                        },
                        &bundle,
                    )
                    .await
                    .unwrap();
                let reply: crate::Reply<crate::durable_wait::RestateSourceSealReply> = backend
                    .ingress()
                    .call_object_json(
                        "LashDurableWaitIndex",
                        "session",
                        "seal_source",
                        &crate::Call::new(crate::durable_wait::RestateSourceSealRequest {
                            source,
                            writer: SealWriter::External,
                            seal: SourceSeal::Resolved {
                                result: Box::new(retained.references[0].clone()),
                            },
                        }),
                    )
                    .await
                    .unwrap();
                assert!(matches!(
                    reply.into_body(),
                    crate::durable_wait::RestateSourceSealReply::Outcome { .. }
                ));
            }
            probe.cancel.store(true, Ordering::SeqCst);
            let host = backend.lash_backend().effect_host();
            let control = lash_core::runtime::turn_control::ActiveTurnControl::new(
                host.as_ref(),
                lash_core::runtime::TurnAddress::new("session", "turn"),
            )
            .await
            .unwrap();
            control
                .request_local_stop(host.as_ref(), lash_sansio::TurnCancelMode::Immediate, None)
                .await
                .unwrap();
            replay.add_permits(1);
            (invocation.id, prefix)
        };
        let ((), (id, prefix)) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(
                async {
                    backend
                        .run_in_handler(AdmittedScope::turn("session", "turn"), attempt)
                        .await
                        .unwrap()
                },
                cancel_after_crash,
            )
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "cancelled Deferred replay did not finish: {:#?}",
                backend.server().invocations()
            )
        });
        let journal: Vec<_> = backend
            .server()
            .journal(&id)
            .unwrap()
            .into_iter()
            .filter(|entry| entry.ty.is_command())
            .collect();
        assert_eq!(
            &journal[..prefix.len()],
            prefix,
            "replay preserves every recorded command"
        );
        let records = records.lock().unwrap();
        let decisions: Vec<_> = records
            .iter()
            .flat_map(|record| &record.events)
            .filter_map(|event| match event {
                RunEvent::Decided { decision, .. } => Some(decision),
                _ => None,
            })
            .collect();
        assert_eq!(decisions.len(), 1, "the source has one decision");
        assert_eq!(matches!(decisions[0], CallDecision::Final { .. }), resolved);
        assert_eq!(matches!(decisions[0], CallDecision::Cancelled), !resolved);
        assert_eq!(
            probe.executions_of(&call.call_id),
            1,
            "replay serves the Deferred attempt"
        );
        assert_eq!(
            probe.presentations.lock().unwrap().len(),
            usize::from(resolved)
        );
    }
}

/// L02: D must let the invocation suspend while its owned X awaits acknowledgment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l02_a_schedule_suspends_until_its_owned_attempt_is_acknowledged() {
    let backend = lash_restate_test::backend(0x4943, ServerConfig::default().always_replay(true))
        .await
        .unwrap();
    let call = call("schedule-ack", &Kind::IntentFree);
    let probe = Arc::new(Probe::new(&[(call.clone(), Kind::IntentFree)]));
    let records = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let probe = Arc::clone(&probe);
        let records = Arc::clone(&records);
        let call = call.clone();
        Arc::new(move |scoped| {
            let probe = Arc::clone(&probe);
            let records = Arc::clone(&records);
            let call = call.clone();
            Box::pin(async move {
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                super::decide_round(
                    &mut run,
                    std::slice::from_ref(&call),
                    probe,
                    Default::default(),
                )
                .await
                .unwrap();
                let terminals = run.drain().await.unwrap();
                assert_eq!(terminals.len(), 1);
                assert!(matches!(terminals[0].1, SingletonTerminal::Final { .. }));
                run.close().await.unwrap();
                *records.lock().unwrap() = run.into_records();
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .expect("a recorded schedule can suspend for X acknowledgment")
    .unwrap();
    assert_eq!(probe.executions_of(&call.call_id), 1);
    let records = records.lock().unwrap();
    let events: Vec<_> = records.iter().flat_map(|record| &record.events).collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RunEvent::AttemptRecorded { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RunEvent::Decided { .. }))
            .count(),
        1
    );
    assert!(
        backend
            .server()
            .invocations()
            .iter()
            .any(|view| view.suspensions > 0)
    );
}

/// L03/L17: Closing records a backoff cut itself; it never awaits the timer
/// or re-derives the cut from an in-memory stop on an always-replay handler.
#[tokio::test]
async fn l03_closing_records_backoff_cancellation_without_waiting_for_the_timer() {
    let kind = Kind::Retry { after_ms: 60_000 };
    let calls = vec![(call("closing-backoff", &kind), kind)];
    let id = calls[0].0.call_id.clone();
    let probe = Arc::new(Probe::new(&calls));
    let finished = Arc::new(Mutex::new(Vec::new()));
    let backend = lash_restate_test::backend(500903, ServerConfig::default().always_replay(true))
        .await
        .unwrap();
    let attempt: lash_restate_test::HandlerAttempt = {
        let probe = Arc::clone(&probe);
        let finished = Arc::clone(&finished);
        Arc::new(move |scoped| {
            let probe = Arc::clone(&probe);
            let finished = Arc::clone(&finished);
            let call = calls[0].0.clone();
            Box::pin(async move {
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                run.start_round(
                    &[call],
                    lash_core::tool_run::CapacityScope::Held,
                    probe as Arc<dyn SingletonToolHandlers>,
                    lash_core::tool_run::RecordedRetryPolicy::Reported {
                        max_attempts: std::num::NonZeroU32::new(2).unwrap(),
                        base_delay_ms: 60_000,
                        max_delay_ms: 60_000,
                    },
                )
                .await
                .unwrap();
                run.progress().await.unwrap();
                run.close().await.unwrap();
                finished.lock().unwrap().push(run.into_records());
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(2),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .expect("Closing must not wait for a backoff timer")
    .unwrap();
    assert_eq!(probe.executions_of(&id), 1);
    let finished = finished.lock().unwrap();
    let records = finished.last().unwrap();
    let closing = records
        .iter()
        .find(|record| {
            record.events.iter().any(|event| {
                matches!(
                    event,
                    RunEvent::Lifecycle {
                        state: RunLifecycle::Closing
                    }
                )
            })
        })
        .unwrap();
    assert!(
        closing.events.iter().any(|event| matches!(event,
            RunEvent::Decided { call_id, decision: CallDecision::Cancelled, .. } if *call_id == id
        )),
        "the Closing record owns the backoff cut"
    );
    assert!(
        !records
            .iter()
            .flat_map(|record| &record.events)
            .any(|event| { matches!(event, RunEvent::RetryScheduled { .. }) })
    );
}
mod probe;
