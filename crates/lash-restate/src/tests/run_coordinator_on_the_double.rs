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
use lash_core::{AdmittedScope, AwaitEventKey, EffectOpener, ScopedEffectController, ToolCallId};
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
    /// Parked on a Deferred source.
    Deferred,
}

fn call(label: &str, kind: &Kind) -> SingletonToolCall {
    let declaration = match kind {
        Kind::Declares(intents) => ToolDeclaration::default().with_intents(intents.iter().copied()),
        Kind::IntentFree => ToolDeclaration::default(),
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
    /// Stream events each body observes into its attempt's stream.
    streams: BTreeMap<ToolCallId, Vec<SessionStreamEvent>>,
    cancel: AtomicBool,
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
    emitted: Mutex<Vec<(ToolCallId, AttemptStream)>>,
}

impl Probe {
    fn new(calls: &[(SingletonToolCall, Kind)]) -> Self {
        Self {
            kinds: calls
                .iter()
                .map(|(call, kind)| (call.call_id.clone(), kind.clone()))
                .collect(),
            streams: BTreeMap::new(),
            cancel: AtomicBool::new(false),
            executions: Mutex::new(Vec::new()),
            realized: Mutex::new(Vec::new()),
            held: BTreeSet::new(),
            unrelated: AtomicBool::new(false),
            unrelated_ran: tokio::sync::Notify::new(),
            fault_after_first_intent: None,
            faulted: AtomicBool::new(false),
            seen: Mutex::new(Vec::new()),
            presentations: Mutex::new(Vec::new()),
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
            verdict: BeforeCheckReply::Allow,
        }]
    }

    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        self.executions
            .lock()
            .unwrap()
            .push((attempt.call_id.clone(), attempt.attempt));
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
        let call_id = attempt.call_id;
        Ok(match &self.kinds[call_id] {
            Kind::Declares(intents) => SingletonBodyOutcome::Done {
                output: output_of(call_id),
                intents: intents.clone(),
                start: None,
            },
            Kind::IntentFree => SingletonBodyOutcome::Done {
                output: output_of(call_id),
                intents: Vec::new(),
                start: None,
            },
            Kind::Deferred => SingletonBodyOutcome::Deferred {
                source: AwaitEventKey {
                    scope: lash_core::ExecutionScope::turn("session", "turn"),
                    wait: lash_core::AwaitEventWaitIdentity::tool_completion(call_id.clone()),
                    key_id: format!("fig4880-{call_id}"),
                    signature: "fig4880".to_owned(),
                },
            },
        })
    }

    async fn after_checks(
        &self,
        _call_id: &ToolCallId,
        _capture: &SingletonCapture,
    ) -> Vec<AttributedVerdict<AfterCheckVerdict>> {
        Vec::new()
    }

    fn run_cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
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
    ) -> Result<String, String> {
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
            record: RunRecord {
                segment: SegmentOrdinal(0),
                first: RunEventOrdinal(0),
                events: vec![RunEvent::Lifecycle {
                    state: RunLifecycle::Closing,
                }],
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
    let finished: Arc<Mutex<Vec<Finished>>> = Arc::new(Mutex::new(Vec::new()));
    let terminals = Arc::new(Mutex::new(BTreeMap::new()));
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
                let handlers: &dyn SingletonToolHandlers = probe.as_ref();
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
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
