//! L03, L18 and K6: intent commands belong to an independently admitted invocation.

use super::aggregate::aggregate_plan;
use super::*;
use lash_core::facade_support::SystemClock;
use lash_core::tool_dispatch::RunAggregateOutcome;
use lash_core::tool_run::{AggregateConsumer, RunTransfer};
use lash_restate_test::JournalEntryView;
use std::sync::atomic::AtomicUsize;

/// A flag a journaled step, a body or a watching task waits on.
struct Gate {
    open: AtomicBool,
    wake: tokio::sync::Notify,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            open: AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
        })
    }

    fn release(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.open.load(Ordering::SeqCst) {
                break;
            }
            notified.await;
        }
    }
}

/// One journaled command issued in the realization invocation.
struct Nested {
    name: &'static str,
    /// The command is not issued until this opens.
    issue_after: Option<Arc<Gate>>,
    /// Its step holds on this before producing its external mutation.
    step_wait: Option<Arc<Gate>>,
    /// Released when the step begins waiting: the in-flight signal.
    signal: Option<Arc<Gate>>,
    /// The external mutation count: each execution of the step.
    mutations: Arc<AtomicUsize>,
}

/// Owned fixture state shared by the Run handlers and its independent realizer.
struct NestedRealization {
    probe: Arc<Probe>,
    /// First-attempt body holds beside `probe.gate`.
    hold_bodies: BTreeMap<ToolCallId, Arc<Gate>>,
    /// The journaled commands each call's realization issues, in order.
    nested: BTreeMap<ToolCallId, Vec<Nested>>,
    /// How many realization invocations entered their body.
    realize_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SingletonToolHandlers for NestedRealization {
    fn tool_material_store(&self) -> Option<&dyn lash_core::store::ToolMaterialStore> {
        self.probe.tool_material_store()
    }

    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        self.probe.prepare(call).await
    }

    async fn before_checks(
        &self,
        call: &SingletonToolCall,
        request: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        self.probe.before_checks(call, request).await
    }

    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        if attempt.attempt == AttemptOrdinal::FIRST
            && let Some(gate) = self.hold_bodies.get(attempt.call_id)
        {
            gate.wait().await;
        }
        self.probe.execute(attempt).await
    }

    async fn after_checks(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        self.probe.after_checks(call_id, capture).await
    }

    async fn run_cancel_requested(&self) -> Result<bool, String> {
        self.probe.run_cancel_requested().await
    }

    async fn cancel_call(
        &self,
        call_id: &ToolCallId,
        source: Option<&lash_core::AwaitEventKey>,
    ) -> Result<(), String> {
        self.probe.cancel_call(call_id, source).await
    }

    async fn present(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, lash_core::tool_dispatch::SingletonPresentationError> {
        self.probe.present(call_id, capture).await
    }

    fn emit_stream(&self, call_id: &ToolCallId, stream: &AttemptStream) {
        self.probe.emit_stream(call_id, stream);
    }

    async fn launch_start(
        &self,
        obligation: &DeclaredStartObligation,
    ) -> Result<lash_core::ProcessId, String> {
        self.probe.launch_start(obligation).await
    }

    async fn discharge_start(
        &self,
        obligation: &DeclaredStartObligation,
        process_id: &lash_core::ProcessId,
        cancel: bool,
    ) -> Result<(), String> {
        self.probe
            .discharge_start(obligation, process_id, cancel)
            .await
    }
}
#[async_trait::async_trait]
impl lash_core::tool_dispatch::ToolRealizer for NestedRealization {
    async fn realize(
        &self,
        request: lash_core::tool_dispatch::RealizationRequest,
        scoped: lash_core::ScopedEffectController<'_>,
    ) -> Result<lash_core::tool_dispatch::RealizationReceipt, lash_core::RuntimeEffectControllerError>
    {
        let call_id = &request.call_id;
        let intents = match self.probe.kinds.get(call_id) {
            Some(Kind::Declares(intents)) => intents.clone(),
            _ => Vec::new(),
        };
        let operation = async {
            self.realize_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(nested) = self.nested.get(call_id) {
                for command in nested {
                    if let Some(gate) = &command.issue_after {
                        gate.wait().await;
                    }
                    let step_wait = command.step_wait.clone();
                    let signal = command.signal.clone();
                    let mutations = Arc::clone(&command.mutations);
                    scoped
                        .controller()
                        .record_run_record(
                            command.name.to_owned(),
                            Box::pin(async move {
                                if let Some(signal) = signal {
                                    signal.release();
                                }
                                if let Some(gate) = step_wait {
                                    gate.wait().await;
                                }
                                mutations.fetch_add(1, Ordering::SeqCst);
                                Ok(RunJournalEntry {
                                    state: Vec::new(),
                                    materials: Vec::new(),
                                    record: RunRecord {
                                        segment: SegmentOrdinal(0),
                                        first: RunEventOrdinal(0),
                                        events: Vec::new(),
                                        trace: None,
                                    },
                                })
                            }),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
            self.probe.record_intents(call_id, &intents).await
        };
        operation.await.map_err(|message| {
            lash_core::RuntimeEffectControllerError::from(lash_core::PluginError::attempt_fault(
                message,
            ))
        })?;
        Ok(Default::default())
    }
}

/// Every journal entry of every invocation, in order.
fn journal_entries(server: &lash_restate_test::RestateTestServer) -> Vec<JournalEntryView> {
    server
        .invocations()
        .iter()
        .flat_map(|view| server.journal(&view.id).unwrap_or_default())
        .collect()
}

/// The `RunRecord` a `RunCompletionNotification` carries, when it is one of
/// the Run's journaled records.
fn notification_record(entry: &JournalEntryView) -> Option<RunRecord> {
    let bytes = entry.run_completion().and_then(Result::ok)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    serde_json::from_value::<RunRecord>(value.get("record")?.clone()).ok()
}

/// The `ctx.run` commands of every invocation, with each command's recorded
/// record when its completion notification carries one.
fn journaled_commands(
    server: &lash_restate_test::RestateTestServer,
) -> Vec<(String, Option<RunRecord>)> {
    server
        .invocations()
        .iter()
        .flat_map(|view| {
            let entries = server.journal(&view.id).unwrap_or_default();
            let completions: BTreeMap<u32, RunRecord> = entries
                .iter()
                .filter(|entry| entry.ty == MessageType::RunCompletionNotification)
                .filter_map(|entry| {
                    Some((
                        entry.completion_id().unwrap_or_default(),
                        notification_record(entry)?,
                    ))
                })
                .collect();
            entries
                .iter()
                .filter(|entry| entry.ty == MessageType::RunCommand)
                .map(|entry| {
                    let record = entry
                        .completion_id()
                        .and_then(|id| completions.get(&id).cloned());
                    (entry.name.clone().unwrap_or_default(), record)
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Yield until `condition` holds of the journaled `ctx.run` commands.
async fn until_commands(
    server: &lash_restate_test::RestateTestServer,
    mut condition: impl FnMut(&[(String, Option<RunRecord>)]) -> bool,
) {
    loop {
        if condition(&journaled_commands(server)) {
            return;
        }
        tokio::task::yield_now().await;
    }
}

/// Yield until `condition` holds of the full journal entry stream.
async fn until_entries(
    server: &lash_restate_test::RestateTestServer,
    mut condition: impl FnMut(&[JournalEntryView]) -> bool,
) {
    loop {
        if condition(&journal_entries(server)) {
            return;
        }
        tokio::task::yield_now().await;
    }
}

/// The test host invocation the law's handler runs on.
fn host_invocation_id(server: &lash_restate_test::RestateTestServer) -> String {
    server
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with("LashTestHandlerHost/"))
        .map(|view| view.id)
        .expect("the test handler invocation exists")
}

/// L18: a held command in the realization journal does not block a higher
/// decision in the Run journal. Crashing the Run at that boundary replays
/// without meeting the held command, and the mutation remains exactly once.
#[tokio::test]
async fn l18_a_realization_command_in_flight_at_a_crash_replays_after_a_higher_decision() {
    const NESTED: &str = "fig4987a:external-intent";
    let calls = Arc::new(vec![
        (
            call(
                "a-rank-one",
                &Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
            ),
            Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
        ),
        (call("a-rank-two", &Kind::IntentFree), Kind::IntentFree),
    ]);
    let ids: Vec<_> = calls.iter().map(|(call, _)| call.call_id.clone()).collect();
    let mut probe = Probe::new(&calls);
    probe.gate = Some((ids[1].clone(), ids[0].clone()));
    let probe = Arc::new(probe);
    let backend = lash_restate_test::backend(4987, ServerConfig::default())
        .await
        .unwrap();
    let crashes = lash_restate_test::CrashCount::new();
    assert!(backend.server().on_crash(crashes.listener()));
    let release = Gate::new();
    let mutations = Arc::new(AtomicUsize::new(0));
    let realize_calls = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let realization_slot = backend.process_worker_slot();
        let probe = Arc::clone(&probe);
        let calls = Arc::clone(&calls);
        let finished = Arc::clone(&finished);
        let release = Arc::clone(&release);
        let mutations = Arc::clone(&mutations);
        let realize_calls = Arc::clone(&realize_calls);
        let nested_call = ids[0].clone();
        Arc::new(move |scoped| {
            let realization_slot = realization_slot.clone();
            let probe = Arc::clone(&probe);
            let calls = Arc::clone(&calls);
            let finished = Arc::clone(&finished);
            let release = Arc::clone(&release);
            let mutations = Arc::clone(&mutations);
            let realize_calls = Arc::clone(&realize_calls);
            let nested_call = nested_call.clone();
            Box::pin(async move {
                let handlers = Arc::new(NestedRealization {
                    probe,
                    hold_bodies: BTreeMap::new(),
                    nested: BTreeMap::from([(
                        nested_call,
                        vec![Nested {
                            name: NESTED,
                            issue_after: None,
                            step_wait: Some(release),
                            signal: None,
                            mutations,
                        }],
                    )]),
                    realize_calls,
                });
                realization_slot.install_tool_realizer(handlers.clone());
                let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                let plan = aggregate_plan("fig4987a", &round, vec![0, 1]);
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                run.start_aggregate(
                    &plan,
                    &round,
                    lash_core::tool_run::CapacityScope::Held,
                    handlers,
                    Default::default(),
                    &SystemClock,
                )
                .await
                .unwrap();
                assert!(matches!(
                    run.consume_aggregate(&plan.key, AggregateConsumer::All)
                        .await
                        .unwrap(),
                    RunAggregateOutcome::AllResults(results)
                        if results.len() == 2 && results.iter().all(Option::is_some)
                ));
                run.close().await.unwrap();
                finished.lock().unwrap().push(run.into_records());
            })
        })
    };
    let side = {
        let server = backend.server().clone();
        let probe = Arc::clone(&probe);
        let release = Arc::clone(&release);
        async move {
            // The realization's nested command is journaled: release rank
            // 2's X so its decision is journaled after it.
            until_commands(&server, |commands| {
                commands.iter().any(|(name, _)| name == NESTED)
            })
            .await;
            probe.gate_open.store(true, Ordering::SeqCst);
            probe.gate_wake.notify_waiters();
            // A later schedule command is journaled while the nested
            // command is still unresolved: the crash drops the attempt
            // with the realization in flight.
            until_commands(&server, |commands| commands.iter().any(|(_, record)| record.as_ref().is_some_and(|record| record.events.iter().any(|event| matches!(event, RunEvent::Decided { call_id, .. } if *call_id == probe.gate.as_ref().unwrap().0))))).await;
            let id = host_invocation_id(&server);
            assert!(server.crash(&id), "the running attempt crashes");
            // The step may complete only on a replay that reaches it.
            release.release();
        }
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        let (answer, ()) = tokio::join!(
            backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
            side
        );
        answer.unwrap();
    })
    .await
    .expect("L18: a realization command in flight at a crash replays after a higher decision");
    assert_eq!(crashes.get(), 1, "the handler attempt crashed once");
    assert_eq!(
        mutations.load(Ordering::SeqCst),
        1,
        "the external mutation ran exactly once"
    );
    assert_isolated(backend.server(), &[NESTED]);
    assert!(
        journaled_commands(backend.server())
            .iter()
            .any(|(_, record)| record
                .as_ref()
                .is_some_and(|record| record.events.iter().any(
                    |event| matches!(event, RunEvent::Decided { call_id, .. } if *call_id == ids[1])
                )))
    );
    let finished = finished.lock().unwrap();
    assert!(drain_violations(finished.last().unwrap(), &BTreeSet::new(), None).is_empty());
}

/// L18: realization commands span two windows while higher decisions keep
/// progressing. A lost Run output replays the receipt without repeating either
/// mutation or interleaving either command into the Run's journal.
#[tokio::test]
async fn l18_a_realization_spanning_two_windows_replays() {
    const FIRST: &str = "fig4987b:intent-one";
    const SECOND: &str = "fig4987b:intent-two";
    let calls = Arc::new(vec![
        (
            call(
                "b-rank-one",
                &Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
            ),
            Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
        ),
        (call("b-rank-two", &Kind::IntentFree), Kind::IntentFree),
        (call("b-rank-three", &Kind::IntentFree), Kind::IntentFree),
    ]);
    let ids: Vec<_> = calls.iter().map(|(call, _)| call.call_id.clone()).collect();
    let mut probe = Probe::new(&calls);
    probe.gate = Some((ids[1].clone(), ids[0].clone()));
    let probe = Arc::new(probe);
    let backend = lash_restate_test::backend(49871, ServerConfig::default())
        .await
        .unwrap();
    let crashes = lash_restate_test::CrashCount::new();
    assert!(backend.server().on_crash(crashes.listener()));
    // V and both nested commands are durable; the lost output replays the
    // whole journal.
    backend.server().crash_on(
        CrashRule::new(CrashPoint::BeforeFrame {
            ty: MessageType::OutputCommand,
        })
        .service("LashTestHandlerHost"),
    );
    let higher_decided = Gate::new();
    let later_decided = Gate::new();
    let rank_three_gate = Gate::new();
    let mutations_one = Arc::new(AtomicUsize::new(0));
    let mutations_two = Arc::new(AtomicUsize::new(0));
    let realize_calls = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let realization_slot = backend.process_worker_slot();
        let probe = Arc::clone(&probe);
        let calls = Arc::clone(&calls);
        let finished = Arc::clone(&finished);
        let higher_decided = Arc::clone(&higher_decided);
        let later_decided = Arc::clone(&later_decided);
        let rank_three_gate = Arc::clone(&rank_three_gate);
        let mutations_one = Arc::clone(&mutations_one);
        let mutations_two = Arc::clone(&mutations_two);
        let realize_calls = Arc::clone(&realize_calls);
        let nested_call = ids[0].clone();
        let held_call = ids[2].clone();
        Arc::new(move |scoped| {
            let realization_slot = realization_slot.clone();
            let probe = Arc::clone(&probe);
            let calls = Arc::clone(&calls);
            let finished = Arc::clone(&finished);
            let higher_decided = Arc::clone(&higher_decided);
            let later_decided = Arc::clone(&later_decided);
            let rank_three_gate = Arc::clone(&rank_three_gate);
            let mutations_one = Arc::clone(&mutations_one);
            let mutations_two = Arc::clone(&mutations_two);
            let realize_calls = Arc::clone(&realize_calls);
            let nested_call = nested_call.clone();
            let held_call = held_call.clone();
            Box::pin(async move {
                let handlers = Arc::new(NestedRealization {
                    probe,
                    hold_bodies: BTreeMap::from([(held_call, rank_three_gate)]),
                    nested: BTreeMap::from([(
                        nested_call,
                        vec![
                            Nested {
                                name: FIRST,
                                issue_after: None,
                                step_wait: None,
                                signal: None,
                                mutations: mutations_one,
                            },
                            Nested {
                                name: SECOND,
                                issue_after: Some(higher_decided),
                                step_wait: Some(later_decided),
                                signal: None,
                                mutations: mutations_two,
                            },
                        ],
                    )]),
                    realize_calls,
                });
                realization_slot.install_tool_realizer(handlers.clone());
                let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                let plan = aggregate_plan("fig4987b", &round, vec![0, 1, 2]);
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                run.start_aggregate(
                    &plan,
                    &round,
                    lash_core::tool_run::CapacityScope::Held,
                    handlers,
                    Default::default(),
                    &SystemClock,
                )
                .await
                .unwrap();
                assert!(matches!(
                    run.consume_aggregate(&plan.key, AggregateConsumer::All)
                        .await
                        .unwrap(),
                    RunAggregateOutcome::AllResults(results)
                        if results.len() == 3 && results.iter().all(Option::is_some)
                ));
                run.close().await.unwrap();
                finished.lock().unwrap().push(run.into_records());
            })
        })
    };
    let side = {
        let server = backend.server().clone();
        let probe = Arc::clone(&probe);
        let higher_decided = Arc::clone(&higher_decided);
        let later_decided = Arc::clone(&later_decided);
        let rank_three_gate = Arc::clone(&rank_three_gate);
        let rank_two = ids[1].clone();
        let rank_three = ids[2].clone();
        async move {
            // The first nested command is journaled: release rank 2's X so
            // its decision lands after it.
            until_commands(&server, |commands| {
                commands.iter().any(|(name, _)| name == FIRST)
            })
            .await;
            probe.gate_open.store(true, Ordering::SeqCst);
            probe.gate_wake.notify_waiters();
            // Rank 2's decision is journaled after the first command: the
            // second command may be issued.
            until_commands(&server, |commands| commands.iter().any(|(_, record)| record.as_ref().is_some_and(|record| record.events.iter().any(|event| matches!(event, RunEvent::Decided { call_id, .. } if *call_id == rank_two))))).await;
            higher_decided.release();
            // The second command is journaled: release rank 3's X so it
            // wins the schedule window the journal put between them.
            until_commands(&server, |commands| {
                commands.iter().any(|(name, _)| name == SECOND)
            })
            .await;
            rank_three_gate.release();
            until_entries(&server, |entries| {
                entries.iter().any(|entry| {
                    notification_record(entry).is_some_and(|record| {
                        record.events.iter().any(|event| {
                            matches!(
                                event,
                                RunEvent::Decided { call_id, .. } if *call_id == rank_three
                            )
                        })
                    })
                })
            })
            .await;
            later_decided.release();
        }
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        let (answer, ()) = tokio::join!(
            backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
            side
        );
        answer.unwrap();
    })
    .await
    .expect("L18: a realization spanning two schedule windows replays");
    assert_eq!(crashes.get(), 1, "the handler output was lost once");
    assert_eq!(
        mutations_one.load(Ordering::SeqCst),
        1,
        "the first intent's mutation ran exactly once"
    );
    assert_eq!(
        mutations_two.load(Ordering::SeqCst),
        1,
        "the second intent's mutation ran exactly once"
    );
    assert_isolated(backend.server(), &[FIRST, SECOND]);
    let finished = finished.lock().unwrap();
    assert!(drain_violations(finished.last().unwrap(), &BTreeSet::new(), None).is_empty());
}

/// FIG-4987 (c): the Run's cancellation is accepted while a declaring
/// final's X is still in flight. When the X finishes, its D must withhold
/// the call — no `declare` record, no realization, no nested command.
#[tokio::test]
async fn l03_a_cancel_accepted_before_the_decision_admits_no_realization() {
    const NESTED: &str = "fig4987c:external-intent";
    let calls = Arc::new(vec![
        (
            call(
                "c-cancelled",
                &Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
            ),
            Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
        ),
        (call("c-release", &Kind::IntentFree), Kind::IntentFree),
    ]);
    let ids: Vec<_> = calls.iter().map(|(call, _)| call.call_id.clone()).collect();
    let mut probe = Probe::new(&calls);
    probe.gate = Some((ids[0].clone(), ids[1].clone()));
    let probe = Arc::new(probe);
    let backend = lash_restate_test::backend(49872, ServerConfig::default())
        .await
        .unwrap();
    let realize_calls = Arc::new(AtomicUsize::new(0));
    let mutations = Arc::new(AtomicUsize::new(0));
    let terminals = Arc::new(Mutex::new(BTreeMap::new()));
    let records = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let realization_slot = backend.process_worker_slot();
        let probe = Arc::clone(&probe);
        let calls = Arc::clone(&calls);
        let terminals = Arc::clone(&terminals);
        let records = Arc::clone(&records);
        let realize_calls = Arc::clone(&realize_calls);
        let mutations = Arc::clone(&mutations);
        let nested_call = ids[0].clone();
        Arc::new(move |scoped| {
            let realization_slot = realization_slot.clone();
            let probe = Arc::clone(&probe);
            let calls = Arc::clone(&calls);
            let terminals = Arc::clone(&terminals);
            let records = Arc::clone(&records);
            let realize_calls = Arc::clone(&realize_calls);
            let mutations = Arc::clone(&mutations);
            let nested_call = nested_call.clone();
            Box::pin(async move {
                let handlers = Arc::new(NestedRealization {
                    probe: Arc::clone(&probe),
                    hold_bodies: BTreeMap::new(),
                    nested: BTreeMap::from([(
                        nested_call,
                        vec![Nested {
                            name: NESTED,
                            issue_after: None,
                            step_wait: None,
                            signal: None,
                            mutations,
                        }],
                    )]),
                    realize_calls,
                });
                realization_slot.install_tool_realizer(handlers.clone());
                let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                crate::tests::decide_round(&mut run, &round, handlers, Default::default())
                    .await
                    .unwrap();
                let drained = run.drain().await.unwrap();
                terminals.lock().unwrap().extend(drained);
                run.close().await.unwrap();
                records.lock().unwrap().extend(run.into_records());
            })
        })
    };
    let side = {
        let server = backend.server().clone();
        let probe = Arc::clone(&probe);
        let release_call = ids[1].clone();
        async move {
            // The release call's final is durable: the Run's cancellation
            // lands while the declaring call's X is still held, and the X
            // then finishes.
            until_entries(&server, |entries| {
                entries.iter().any(|entry| {
                    notification_record(entry).is_some_and(|record| {
                        record.events.iter().any(|event| {
                            matches!(
                                event,
                                RunEvent::Decided {
                                    call_id,
                                    decision: CallDecision::Final { .. },
                                    ..
                                } if *call_id == release_call
                            )
                        })
                    })
                })
            })
            .await;
            probe.cancel.store(true, Ordering::SeqCst);
            probe.gate_open.store(true, Ordering::SeqCst);
            probe.gate_wake.notify_waiters();
        }
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        let (answer, ()) = tokio::join!(
            backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
            side
        );
        answer.unwrap();
    })
    .await
    .expect("L03: a cancel accepted before the decision admits no realization");
    assert!(
        matches!(
            terminals.lock().unwrap().get(&ids[0]),
            Some(SingletonTerminal::Withheld {
                decision: CallDecision::Cancelled
            })
        ),
        "the declaring call is withheld by the Run's cancellation"
    );
    assert!(
        matches!(
            terminals.lock().unwrap().get(&ids[1]),
            Some(SingletonTerminal::Final { .. })
        ),
        "the release call's earlier final stands"
    );
    assert_eq!(
        realize_calls.load(Ordering::SeqCst),
        0,
        "realization never ran"
    );
    assert!(probe.realized.lock().unwrap().is_empty());
    assert_eq!(mutations.load(Ordering::SeqCst), 0);
    let journal = journaled_commands(backend.server());
    assert!(
        !journal
            .iter()
            .any(|(entry, _)| entry == &name(&ids[0], "declare")),
        "no declare record was journaled"
    );
    assert!(
        !journal.iter().any(|(entry, _)| entry == NESTED),
        "no nested command was journaled"
    );
    let events: Vec<_> = records
        .lock()
        .unwrap()
        .iter()
        .flat_map(|record| record.events.clone())
        .collect();
    assert!(
        !events.iter().any(|event| matches!(
            event,
            RunEvent::DeclarationsIssued { call_id } if *call_id == ids[0]
        )),
        "no declarations were issued"
    );
}

/// FIG-4987 (d): a declaring final's realization is in flight — its nested
/// command held — when a physical cut is requested. Today's API carries the
/// in-flight presentation inside the drain frame, so the only way to reach
/// `request_cut` is to drop that frame; the coordinator is then wedged
/// (`active_frame` is never cleared) and `quiesce` refuses the capture.
/// The law is red with `InvocationFailed`: no transfer exists for a
/// successor to adopt, and a successor that drains the same records would
/// realize the intents a second time.
#[tokio::test]
async fn k6_a_cut_with_realization_in_flight_hands_the_receipt_over_once() {
    const NESTED: &str = "fig4987d:external-intent";
    let calls = Arc::new(vec![(
        call("d-cut", &Kind::Declares(vec![ToolIntentKind::EmitTrigger])),
        Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
    )]);
    let id = calls[0].0.call_id.clone();
    let mut probe = Probe::new(&calls);
    let backend = lash_restate_test::backend(49873, ServerConfig::default())
        .await
        .unwrap();
    probe.materials = Some(backend.stores().process_env_store());
    let probe = Arc::new(probe);
    let held = Gate::new();
    let release = Gate::new();
    let mutations = Arc::new(AtomicUsize::new(0));
    let realize_calls = Arc::new(AtomicUsize::new(0));
    let transfers: Arc<Mutex<Vec<RunTransfer>>> = Arc::new(Mutex::new(Vec::new()));
    let refusals = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let realization_slot = backend.process_worker_slot();
        let probe = Arc::clone(&probe);
        let calls = Arc::clone(&calls);
        let held = Arc::clone(&held);
        let release = Arc::clone(&release);
        let mutations = Arc::clone(&mutations);
        let realize_calls = Arc::clone(&realize_calls);
        let transfers = Arc::clone(&transfers);
        let refusals = Arc::clone(&refusals);
        let nested_call = id.clone();
        Arc::new(move |scoped| {
            let realization_slot = realization_slot.clone();
            let probe = Arc::clone(&probe);
            let calls = Arc::clone(&calls);
            let held = Arc::clone(&held);
            let release = Arc::clone(&release);
            let mutations = Arc::clone(&mutations);
            let realize_calls = Arc::clone(&realize_calls);
            let transfers = Arc::clone(&transfers);
            let refusals = Arc::clone(&refusals);
            let nested_call = nested_call.clone();
            Box::pin(async move {
                let handlers = Arc::new(NestedRealization {
                    probe: Arc::clone(&probe),
                    hold_bodies: BTreeMap::new(),
                    nested: BTreeMap::from([(
                        nested_call,
                        vec![Nested {
                            name: NESTED,
                            issue_after: None,
                            step_wait: Some(release),
                            signal: Some(Arc::clone(&held)),
                            mutations,
                        }],
                    )]),
                    realize_calls,
                });
                realization_slot.install_tool_realizer(handlers.clone());
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                crate::tests::decide_round(
                    &mut run,
                    &calls
                        .iter()
                        .map(|(call, _)| call.clone())
                        .collect::<Vec<_>>(),
                    handlers,
                    Default::default(),
                )
                .await
                .unwrap();
                run.begin_drain().await.unwrap();
                held.wait().await;
                run.request_cut(lash_core::BoundaryReason::HandOver);
                match run.quiesce().await {
                    Ok(mut transfer) => {
                        let store = probe.materials.as_ref().unwrap();
                        run.retain_cut(&mut transfer, store.as_ref()).await.unwrap();
                        transfers.lock().unwrap().push(transfer);
                    }
                    Err(error) => {
                        refusals.lock().unwrap().push(error.to_string());
                    }
                }
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .expect("K6: the cut capture does not wedge behind an in-flight realization")
    .unwrap();
    release.release();
    let transfer = transfers
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "K6: no transfer for the successor to adopt: {:?}",
                refusals.lock().unwrap()
            )
        });
    // The successor adopts the captured transfer and drains the owed final.
    let terminals = Arc::new(Mutex::new(BTreeMap::new()));
    let successor: lash_restate_test::HandlerAttempt = {
        let realization_slot = backend.process_worker_slot();
        let probe = Arc::clone(&probe);
        let release = Arc::clone(&release);
        let mutations = Arc::clone(&mutations);
        let realize_calls = Arc::clone(&realize_calls);
        let terminals = Arc::clone(&terminals);
        let nested_call = id.clone();
        Arc::new(move |scoped| {
            let realization_slot = realization_slot.clone();
            let probe = Arc::clone(&probe);
            let release = Arc::clone(&release);
            let mutations = Arc::clone(&mutations);
            let realize_calls = Arc::clone(&realize_calls);
            let terminals = Arc::clone(&terminals);
            let nested_call = nested_call.clone();
            let transfer = transfer.clone();
            Box::pin(async move {
                let handlers = Arc::new(NestedRealization {
                    probe,
                    hold_bodies: BTreeMap::new(),
                    nested: BTreeMap::from([(
                        nested_call,
                        vec![Nested {
                            name: NESTED,
                            issue_after: None,
                            step_wait: Some(release),
                            signal: None,
                            mutations,
                        }],
                    )]),
                    realize_calls,
                });
                realization_slot.install_tool_realizer(handlers.clone());
                let mut run = RunCoordinator::adopt(
                    &scoped,
                    owner(),
                    SegmentOrdinal(1),
                    vec![revision()],
                    transfer,
                    handlers,
                    &SystemClock,
                )
                .await
                .unwrap();
                let drained = run.drain().await.unwrap();
                terminals.lock().unwrap().extend(drained);
                run.close().await.unwrap();
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        backend.run_in_handler(AdmittedScope::turn("session", "successor"), successor),
    )
    .await
    .expect("K6: the adopted run drains the transferred final")
    .unwrap();
    assert_eq!(
        mutations.load(Ordering::SeqCst),
        1,
        "the realization's external mutation ran once across both invocations"
    );
    assert!(
        matches!(
            terminals.lock().unwrap().get(&id),
            Some(SingletonTerminal::Final { presentation, .. })
                if presentation == &format!("fig4880 presented {id}")
        ),
        "the successor's terminal is the call's presentation"
    );
}

fn assert_isolated(server: &lash_restate_test::RestateTestServer, commands: &[&str]) {
    let invocations = server.invocations();
    let owner = invocations
        .iter()
        .find(|view| view.target.starts_with("LashTestHandlerHost/"))
        .unwrap();
    let realization = invocations
        .iter()
        .find(|view| view.target.starts_with("LashToolRealization/"))
        .unwrap();
    assert_ne!(owner.id, realization.id);
    let owner_journal = server.journal(&owner.id).unwrap();
    let realization_journal = server.journal(&realization.id).unwrap();
    for command in commands {
        assert!(
            !owner_journal
                .iter()
                .any(|entry| entry.name.as_deref() == Some(command))
        );
        assert!(
            realization_journal
                .iter()
                .any(|entry| entry.name.as_deref() == Some(command))
        );
    }
}
