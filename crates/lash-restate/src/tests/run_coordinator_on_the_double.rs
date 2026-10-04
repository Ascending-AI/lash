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
use lash_core::runtime::{
    ATTEMPT_STREAM_BYTE_BUDGET, AttemptStream, AttemptStreamTruncation, DecodedStreamEvent,
};
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
use lash_core::{AdmittedScope, EffectOpener, ScopedEffectController, ToolCallId};
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
        environment: None,
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
    cancelled_calls: Mutex<Vec<ToolCallId>>,
    parallel: Option<Arc<tokio::sync::Barrier>>,
    body_barrier: Option<Arc<tokio::sync::Barrier>>,
    parallel_order: Vec<ToolCallId>,
    parallel_completed: std::sync::atomic::AtomicUsize,
    parallel_wake: tokio::sync::Notify,
    retry: lash_core::tool_run::RecordedRetryPolicy,
    gate: Option<(ToolCallId, ToolCallId)>,
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

impl Probe {
    fn new(calls: &[(SingletonToolCall, Kind)]) -> Self {
        Self {
            kinds: calls
                .iter()
                .map(|(call, kind)| (call.call_id.clone(), kind.clone()))
                .collect(),
            materials: None,
            sources: Mutex::new(BTreeMap::new()),
            complete_sources: false,
            streams: BTreeMap::new(),
            cancel: AtomicBool::new(false),
            cancelled_calls: Mutex::new(Vec::new()),
            parallel: None,
            body_barrier: None,
            parallel_order: Vec::new(),
            parallel_completed: Default::default(),
            parallel_wake: Default::default(),
            retry: Default::default(),
            gate: None,
            gate_open: AtomicBool::new(false),
            gate_after_crash: false,
            gate_wake: Default::default(),
            cancel_at_timer: false,
            handler_attempts: Default::default(),
            replay_delay: None,
            cancel_at_gate: false,
            plugin_host: None,
            state_seed: None,
            plugins: Mutex::new(None),
            executions: Mutex::new(Vec::new()),
            realized: Mutex::new(Vec::new()),
            held: BTreeSet::new(),
            unrelated: AtomicBool::new(false),
            unrelated_ran: tokio::sync::Notify::new(),
            fault_after_first_intent: None,
            faulted: AtomicBool::new(false),
            seen: Mutex::new(Vec::new()),
            presentations: Mutex::new(Vec::new()),
            presentation_failure: false,
            declaration_drift_on_replay: false,
            emitted: Mutex::new(Vec::new()),
        }
    }

    fn executions_of(&self, call_id: &ToolCallId) -> usize {
        self.executions
            .lock()
            .unwrap()
            .iter()
            .filter(|(executed, _)| executed == call_id)
            .count()
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// The unrelated effect's step: it runs while the Run drains.
    fn run_unrelated(&self) {
        self.seen.lock().unwrap().push(Seen::Unrelated);
        self.unrelated.store(true, Ordering::SeqCst);
        self.unrelated_ran.notify_waiters();
    }
}

#[async_trait::async_trait]
impl SingletonToolHandlers for Probe {
    fn tool_material_store(&self) -> Option<&dyn lash_core::store::ToolMaterialStore> {
        self.materials.as_deref()
    }

    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({ "sealed": call.arguments }))
    }

    async fn before_checks(
        &self,
        _call: &SingletonToolCall,
        _request: &SingletonPreparedRequest,
    ) -> Vec<AttributedVerdict<BeforeCheckReply>> {
        vec![AttributedVerdict {
            callback: binding().executable,
            verdict: if matches!(self.kinds[&_call.call_id], Kind::Cached) {
                BeforeCheckReply::Cached {
                    output: output_of(&_call.call_id),
                }
            } else {
                BeforeCheckReply::Allow
            },
        }]
    }

    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        self.executions
            .lock()
            .unwrap()
            .push((attempt.call_id.clone(), attempt.attempt));
        if let Some(barrier) = &self.body_barrier
            && self.executions_of(attempt.call_id) == 1
        {
            barrier.wait().await;
        }
        for (ordinal, event) in self
            .streams
            .get(attempt.call_id)
            .into_iter()
            .flatten()
            .enumerate()
        {
            attempt.stream.observe(ShiftObservation {
                key: ReplayKey::new(format!("{}", attempt.call_id)),
                ordinal: u32::try_from(ordinal).unwrap(),
                event: ObservedEvent::Session(event.clone()),
            });
        }
        if let Some(barrier) = &self.parallel {
            tokio::time::timeout(Duration::from_secs(1), barrier.wait())
                .await
                .expect("L01: every body reaches its barrier before any can finish");
            loop {
                let wake = self.parallel_wake.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                if self.parallel_order[self.parallel_completed.load(Ordering::SeqCst)]
                    == *attempt.call_id
                {
                    break;
                }
                wake.await;
            }
        }
        if attempt.attempt == AttemptOrdinal::FIRST
            && self
                .gate
                .as_ref()
                .is_some_and(|(held, _)| held == attempt.call_id)
        {
            loop {
                let wake = self.gate_wake.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                if self.gate_open.load(Ordering::SeqCst) {
                    break;
                }
                wake.await;
            }
        }
        if let Some(seed) = &self.state_seed {
            assert_eq!(
                attempt.request.state_snapshot.as_ref().unwrap().values,
                seed.plugins[PLUGIN].values
            );
        }
        let call_id = attempt.call_id;
        Ok(match &self.kinds[call_id] {
            Kind::Declares(intents) => SingletonBodyOutcome::Done {
                commands: Default::default(),
                output: output_of(call_id),
                intents: intents.clone(),
                start: None,
            },
            Kind::Retry { after_ms } if attempt.attempt == AttemptOrdinal::FIRST => {
                SingletonBodyOutcome::RetryableFailure {
                    output: format!("failed {call_id}@1"),
                    after_ms: Some(*after_ms),
                }
            }
            Kind::Stateful { key } => {
                assert!(
                    attempt
                        .request
                        .state_snapshot
                        .as_ref()
                        .unwrap()
                        .values
                        .is_empty(),
                    "admission fixes the body's snapshot even after a sibling publishes"
                );
                SingletonBodyOutcome::Done {
                    output: output_of(call_id),
                    commands: lash_core::plugin::StateCommands::new().apply(
                        key,
                        "append",
                        serde_json::json!(call_id.to_string()),
                    ),
                    intents: Vec::new(),
                    start: None,
                }
            }
            Kind::Failed => SingletonBodyOutcome::Failed {
                output: format!("rejected {call_id}"),
            },
            Kind::Cached => panic!("a cached admission executes no body"),
            Kind::IntentFree | Kind::Retry { .. } => SingletonBodyOutcome::Done {
                commands: Default::default(),
                output: output_of(call_id),
                intents: Vec::new(),
                start: None,
            },
            Kind::Deferred => {
                let source = attempt
                    .completion_key
                    .expect("admission armed the source")
                    .clone();
                self.sources
                    .lock()
                    .unwrap()
                    .insert(call_id.clone(), source.clone());
                SingletonBodyOutcome::Deferred { source }
            }
        })
    }

    async fn after_checks(
        &self,
        call_id: &ToolCallId,
        _capture: &SingletonCapture,
    ) -> Vec<AttributedVerdict<AfterCheckVerdict>> {
        if !self.parallel_order.is_empty() {
            let index = self.parallel_completed.fetch_add(1, Ordering::SeqCst);
            assert_eq!(self.parallel_order[index], *call_id);
            self.parallel_wake.notify_waiters();
        }
        Vec::new()
    }

    fn plugin_session(&self) -> Option<Arc<lash_core::plugin::PluginSession>> {
        self.plugins.lock().unwrap().clone()
    }

    fn run_cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    async fn cancel_call(
        &self,
        call_id: &ToolCallId,
        _source: Option<&lash_core::AwaitEventKey>,
    ) -> Result<(), String> {
        let mut calls = self.cancelled_calls.lock().unwrap();
        if !calls.contains(call_id) {
            calls.push(call_id.clone());
        }
        self.gate_open.store(true, Ordering::SeqCst);
        self.gate_wake.notify_waiters();
        Ok(())
    }

    async fn realize_declarations(
        &self,
        call_id: &ToolCallId,
        intents: &[ToolIntentKind],
    ) -> Result<(), String> {
        self.seen
            .lock()
            .unwrap()
            .push(Seen::RealizeBegin(call_id.clone()));
        if self.held.contains(call_id) {
            // A blocked protected declaration: it holds until the unrelated
            // effect has made progress, which it must while this drains.
            loop {
                let ran = self.unrelated_ran.notified();
                if self.unrelated.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::timeout(Duration::from_millis(50), ran)
                    .await
                    .ok();
            }
        }
        for (position, kind) in intents.iter().enumerate() {
            {
                let mut realized = self.realized.lock().unwrap();
                if !realized.contains(&(call_id.clone(), *kind)) {
                    realized.push((call_id.clone(), *kind));
                }
            }
            if position == 0
                && self.fault_after_first_intent.as_ref() == Some(call_id)
                && !self.faulted.swap(true, Ordering::SeqCst)
            {
                return Err("a fault after the first intent".to_owned());
            }
        }
        self.seen
            .lock()
            .unwrap()
            .push(Seen::RealizeEnd(call_id.clone()));
        Ok(())
    }

    async fn present(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, lash_core::tool_dispatch::SingletonPresentationError> {
        let declared = match capture {
            SingletonCapture::Done { intents, .. } => intents.clone(),
            _ => Vec::new(),
        };
        let realized = self.realized.lock().unwrap();
        for kind in declared {
            assert!(
                realized.contains(&(call_id.clone(), kind)),
                "{call_id}'s declarations settle before its presentation"
            );
        }
        drop(realized);
        self.presentations.lock().unwrap().push(call_id.clone());
        if self.presentation_failure {
            return Err(
                lash_core::tool_dispatch::SingletonPresentationError::Refused {
                    cause: lash_core::tool_run::HookCause {
                        error_type: "fig4926.presentation_refused".into(),
                        error_version: std::num::NonZeroU32::MIN,
                        payload: serde_json::json!({"reason": "deterministic presentation refusal"}),
                    },
                },
            );
        }
        Ok(format!("fig4880 presented {call_id}"))
    }

    fn emit_stream(&self, call_id: &ToolCallId, stream: &AttemptStream) {
        self.emitted
            .lock()
            .unwrap()
            .push((call_id.clone(), stream.clone()));
    }

    async fn launch_start(
        &self,
        _obligation: &DeclaredStartObligation,
    ) -> Result<lash_core::ProcessId, String> {
        Err("these laws declare no start".to_owned())
    }

    async fn discharge_start(
        &self,
        _obligation: &DeclaredStartObligation,
        _process_id: &lash_core::ProcessId,
        _cancel: bool,
    ) -> Result<(), String> {
        Err("these laws declare no start".to_owned())
    }
}

/// One step of a law's program.
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
        for view in self.backend.server().invocations() {
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
                let handlers: &dyn SingletonToolHandlers = probe.as_ref();
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
                        Step::Decide(index) => {
                            run.decide(&calls[*index].0, handlers).await.map(|decided| {
                                if let DecidedCall::Deferred { source } = decided {
                                    terminals.lock().unwrap().insert(
                                        calls[*index].0.call_id.clone(),
                                        SingletonTerminal::Deferred { source },
                                    );
                                }
                            })
                        }
                        Step::Concurrent => run
                            .decide_round(
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
            name(&ids[0], "decide"),
            name(&ids[1], "admit"),
            name(&ids[1], "attempt:1"),
            name(&ids[1], "decide"),
            name(&ids[2], "admit"),
            name(&ids[2], "attempt:1"),
            name(&ids[2], "decide"),
            UNRELATED.to_owned(),
            name(&ids[0], "declare"),
            name(&ids[0], "present"),
            name(&ids[1], "present"),
            name(&ids[2], "declare"),
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
                name(&ids[3], "decide"),
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

/// A body's bounded stream rides its attempt capture (X) to its presentation
/// (V), with no tool-child settlement: deltas of one block coalesce into one
/// entry, the byte budget cuts the rest with a typed truncation, and a replay
/// that serves the attempt emits the same stream without running the body.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attempt_capture_bounds_its_stream_and_its_presentation_emits_it() {
    let kind = Kind::IntentFree;
    let calls = Arc::new(vec![(call("streams", &kind), kind)]);
    let id = calls[0].0.call_id.clone();
    let mut events: Vec<SessionStreamEvent> =
        (0..64).map(|index| delta(&format!("{index},"))).collect();
    let big = "z".repeat(ATTEMPT_STREAM_BYTE_BUDGET / 3);
    events.extend(
        (0..5).map(|index| SessionStreamEvent::StreamBlockCompleted {
            kind: lash_sansio::llm::types::StreamBlockKind::AssistantText,
            block: lash_sansio::llm::types::StreamBlockIdentity::new("fig4880-block", 0),
            content: format!("{index}{big}"),
        }),
    );
    let coalesced: String = (0..64).map(|index| format!("{index},")).collect();
    for cut in [None, Some("attempt:1"), Some("present")] {
        let mut probe = Probe::new(&calls);
        probe.streams.insert(id.clone(), events.clone());
        let probe = Arc::new(probe);
        let driven = drive(
            0x4880,
            cut.map(|step| CrashPoint::BeforeRunResult {
                name: Some(name(&id, step)),
            })
            .into_iter()
            .collect(),
            Arc::clone(&calls),
            Arc::new(vec![Step::Decide(0), Step::Drain]),
            Arc::clone(&probe),
        )
        .await;
        assert_eq!(
            probe.executions_of(&id),
            1 + usize::from(cut == Some("attempt:1")),
            "cut {cut:?}: a served attempt does not rerun its body"
        );
        let Some(SingletonTerminal::Final { capture, .. }) =
            driven.terminals.lock().unwrap().get(&id).cloned()
        else {
            panic!("cut {cut:?}: the call is final");
        };
        let stream = capture.stream().expect("the body ran").clone();
        let (decoded, undecodable) = stream.decode(&serde_json::Value::Null);
        assert_eq!(undecodable, 0);
        assert!(
            matches!(
                decoded.first(),
                Some(DecodedStreamEvent::Session(SessionStreamEvent::TextDelta { content, .. }))
                    if *content == coalesced
            ),
            "cut {cut:?}: a block's deltas coalesce into one entry: {:?}",
            decoded.first()
        );
        assert_eq!(
            decoded.len(),
            1 + 2,
            "cut {cut:?}: the budget holds two of the large events"
        );
        let Some(AttemptStreamTruncation {
            dropped_events,
            dropped_bytes,
        }) = stream.truncated
        else {
            panic!("cut {cut:?}: the budget cut the stream with a typed marker");
        };
        assert_eq!(dropped_events, 3);
        assert!(dropped_bytes > 3 * (big.len() as u64));
        let recorded: usize = stream
            .events
            .iter()
            .map(|event| serde_json::to_vec(&event.payload).unwrap().len())
            .sum();
        assert!(
            recorded <= ATTEMPT_STREAM_BYTE_BUDGET,
            "cut {cut:?}: bounded"
        );
        let emitted = probe.emitted.lock().unwrap().clone();
        assert!(
            !emitted.is_empty()
                && emitted
                    .iter()
                    .all(|(emitted_id, emitted)| *emitted_id == id && *emitted == stream),
            "cut {cut:?}: the presentation emits the captured stream"
        );
        assert_eq!(
            emitted.len(),
            1 + usize::from(cut == Some("present")),
            "cut {cut:?}: only a lost presentation emits again"
        );
        assert_eq!(
            driven.journal(),
            ["admit", "attempt:1", "decide", "present"]
                .iter()
                .map(|step| name(&id, step))
                .collect::<Vec<_>>()
        );
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
        vec![CrashPoint::BeforeRun {
            name: name(&calls[0].0.call_id, "decide"),
        }],
        Arc::clone(&calls),
        Arc::new(vec![Step::Decide(0), Step::Drain]),
        Arc::clone(&probe),
    )
    .await;
    assert_eq!(driven.records().len(), 4);
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
            vec![CrashPoint::BeforeRunResult {
                name: Some(name(&calls[0].0.call_id, "present")),
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
mod process_continuation;
mod turn_handover;
