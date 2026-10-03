//! The singleton Run route (FIG-4877) through a real handler on the
//! in-process Restate server double.
//!
//! Each law runs one tool call through `run_singleton_tool` inside a
//! `LashTestHandlerHost` handler. The handler's own journal holds the Run's
//! records: admission (A), attempt (X), decision (D), the declaration boundary
//! when a final declares, and presentation with its incorporation (V). A crash
//! drops the attempt that hit it, and the double replays the invocation into
//! the same handler, which serves every durable record and runs only the step
//! that never became durable.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::plugin::{BehaviorRevision, PluginRevision};
use lash_core::store::plugin_writers::PluginCallbackIdentity;
use lash_core::tool_dispatch::{
    BeforeCheckReply, SingletonAttempt, SingletonBodyOutcome, SingletonCapture, SingletonDrift,
    SingletonPreparedRequest, SingletonRunError, SingletonRunOutcome, SingletonTerminal,
    SingletonToolCall, SingletonToolHandlers, run_singleton_tool,
};
use lash_core::tool_run::{
    AdmissionRefusal, AdmittedBinding, AfterCheckVerdict, AttemptOrdinal, AttributedVerdict,
    CallDecision, DeclarationRefusal, PresentationBinding, ResultSource, RunEvent, RunEventRefusal,
    RunRecord, SegmentOrdinal, ToolDeclaration,
};
use lash_core::{AdmittedScope, EffectOpener, ToolCallId};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, ServerConfig};
use lash_sansio::ToolIntentKind;

const PLUGIN: &str = "fig4877-tools";
const OUTPUT: &str = "fig4877 done";
const PRESENTATION: &str = "fig4877 presented";

fn revision(value: u32) -> PluginRevision {
    PluginRevision::new(PLUGIN, BehaviorRevision::new(value).unwrap())
}

fn binding(value: u32) -> AdmittedBinding {
    let callback = |key: &str| PluginCallbackIdentity {
        owner: revision(value),
        key: key.to_owned(),
    };
    AdmittedBinding {
        executable: callback("tool:probe"),
        preparation: callback("tool:probe"),
        presentation: PresentationBinding {
            presenter: callback("present:probe"),
            steps: Vec::new(),
        },
    }
}

fn call(label: &str) -> SingletonToolCall {
    SingletonToolCall {
        owner: EffectOpener::turn("session", "turn"),
        segment: SegmentOrdinal(0),
        call_id: ToolCallId::fixture(label),
        tool_name: "probe".to_owned(),
        arguments: serde_json::json!({ "label": label }),
        declaration: ToolDeclaration::default(),
        binding: binding(1),
        available: vec![revision(1)],
    }
}

/// When the Run's cancellation is requested, relative to the decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelAt {
    Never,
    /// Inside the body: before the decision is recorded.
    Body,
    /// Inside the first decision step, after it read no cancellation: its
    /// proposal can still be lost.
    AfterChecks,
    /// Inside the presentation: after the decision is durable.
    Presentation,
}

/// The callbacks one law's call runs, with a count of every execution.
struct Probe {
    body: SingletonBodyOutcome,
    cancel_at: CancelAt,
    cancel: AtomicBool,
    /// Set once the handler replays after a crash.
    replaying: AtomicBool,
    replay_executions: AtomicUsize,
    prepares: AtomicUsize,
    before_checks: AtomicUsize,
    executions: Mutex<Vec<(ToolCallId, AttemptOrdinal)>>,
    after_checks: AtomicUsize,
    /// Declaration realizations, and the effects their fence let through.
    realizations: AtomicUsize,
    realized: Mutex<Vec<(ToolCallId, ToolIntentKind)>>,
    presentations: AtomicUsize,
}

impl Probe {
    fn new(body: SingletonBodyOutcome, cancel_at: CancelAt) -> Arc<Self> {
        Arc::new(Self {
            body,
            cancel_at,
            cancel: AtomicBool::new(false),
            replaying: AtomicBool::new(false),
            replay_executions: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            before_checks: AtomicUsize::new(0),
            executions: Mutex::new(Vec::new()),
            after_checks: AtomicUsize::new(0),
            realizations: AtomicUsize::new(0),
            realized: Mutex::new(Vec::new()),
            presentations: AtomicUsize::new(0),
        })
    }

    fn done() -> SingletonBodyOutcome {
        SingletonBodyOutcome::Done {
            output: OUTPUT.to_owned(),
            intents: Vec::new(),
        }
    }

    fn executions(&self) -> usize {
        self.executions.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl SingletonToolHandlers for Probe {
    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({ "sealed": call.arguments }))
    }

    async fn before_checks(
        &self,
        _call: &SingletonToolCall,
        _request: &SingletonPreparedRequest,
    ) -> Vec<AttributedVerdict<BeforeCheckReply>> {
        self.before_checks.fetch_add(1, Ordering::SeqCst);
        vec![AttributedVerdict {
            callback: binding(1).executable,
            verdict: BeforeCheckReply::Allow,
        }]
    }

    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        self.executions
            .lock()
            .unwrap()
            .push((attempt.call_id.clone(), attempt.attempt));
        if self.replaying.load(Ordering::SeqCst) {
            self.replay_executions.fetch_add(1, Ordering::SeqCst);
        }
        if self.cancel_at == CancelAt::Body {
            self.cancel.store(true, Ordering::SeqCst);
        }
        Ok(self.body.clone())
    }

    async fn after_checks(
        &self,
        _call_id: &ToolCallId,
        _capture: &SingletonCapture,
    ) -> Vec<AttributedVerdict<AfterCheckVerdict>> {
        self.after_checks.fetch_add(1, Ordering::SeqCst);
        if self.cancel_at == CancelAt::AfterChecks {
            self.cancel.store(true, Ordering::SeqCst);
        }
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
        self.realizations.fetch_add(1, Ordering::SeqCst);
        let mut realized = self.realized.lock().unwrap();
        // The exactly-once fence, keyed by call and declaration.
        for kind in intents {
            if !realized.contains(&(call_id.clone(), *kind)) {
                realized.push((call_id.clone(), *kind));
            }
        }
        Ok(())
    }

    async fn present(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, String> {
        self.presentations.fetch_add(1, Ordering::SeqCst);
        let declared = match capture {
            SingletonCapture::Done { intents, .. } => intents.len(),
            SingletonCapture::Failed { .. } | SingletonCapture::Refused { .. } => 0,
        };
        assert_eq!(
            self.realized.lock().unwrap().len(),
            declared,
            "{call_id}'s declarations settle before its presentation"
        );
        if self.cancel_at == CancelAt::Presentation {
            self.cancel.store(true, Ordering::SeqCst);
        }
        Ok(PRESENTATION.to_owned())
    }
}

type Returned = Arc<Mutex<Vec<Result<SingletonRunOutcome, SingletonRunError>>>>;

struct Driven {
    backend: RestateTestBackend,
    returned: Returned,
}

impl Driven {
    /// What the attempt that finished the handler returned.
    fn finished(&self) -> Result<(SingletonTerminal, Vec<RunRecord>), SingletonRunError> {
        self.returned
            .lock()
            .unwrap()
            .pop()
            .expect("the handler finished")
            .map(|outcome| (outcome.terminal, outcome.records))
    }

    /// The names of the `ctx.run` records the handler's journal holds, in
    /// order, and every invocation's raw entries and bytes.
    fn journal(&self) -> (Vec<String>, usize, usize, usize) {
        let mut names = Vec::new();
        let (mut raw, mut bytes, mut calls) = (0, 0, 0);
        for view in self.backend.server().invocations() {
            let journal = self.backend.server().journal(&view.id).unwrap();
            raw += journal.len();
            for entry in journal {
                bytes += entry.payload.len();
                calls += usize::from(matches!(
                    entry.ty,
                    MessageType::CallCommand | MessageType::OneWayCallCommand
                ));
                if entry.ty == MessageType::RunCommand {
                    names.push(entry.name.unwrap_or_default());
                }
            }
        }
        (names, raw, bytes, calls)
    }
}

/// Run the singleton in a handler: its first attempt runs `first`, and every
/// replay after a crash runs `replay`.
async fn drive(
    seed: u64,
    crashes: Vec<CrashPoint>,
    first: SingletonToolCall,
    replay: SingletonToolCall,
    probe: Arc<Probe>,
) -> Driven {
    let backend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .unwrap();
    for point in crashes {
        backend.server().crash_on(CrashRule::new(point));
    }
    let returned: Returned = Arc::new(Mutex::new(Vec::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempt: lash_restate_test::HandlerAttempt = {
        let returned = Arc::clone(&returned);
        Arc::new(move |scoped| {
            let call = if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                first.clone()
            } else {
                probe.replaying.store(true, Ordering::SeqCst);
                replay.clone()
            };
            let probe = Arc::clone(&probe);
            let returned = Arc::clone(&returned);
            Box::pin(async move {
                let outcome = run_singleton_tool(&scoped, &call, probe.as_ref()).await;
                returned.lock().unwrap().push(outcome);
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
    Driven { backend, returned }
}

fn names(call_id: &ToolCallId, steps: &[&str]) -> Vec<String> {
    steps
        .iter()
        .map(|step| format!("lash:run:{call_id}:{step}"))
        .collect()
}

/// The events of each record, in journal order.
fn events(records: &[RunRecord]) -> Vec<Vec<&'static str>> {
    records
        .iter()
        .map(|record| {
            record
                .events
                .iter()
                .map(|event| match event {
                    RunEvent::Admitted { .. } => "admitted",
                    RunEvent::AttemptRecorded { .. } => "attempt",
                    RunEvent::RetryScheduled { .. } => "retry",
                    RunEvent::Decided { .. } => "decided",
                    RunEvent::DeclarationsIssued { .. } => "declarations_issued",
                    RunEvent::DeclarationsSettled { .. } => "declarations_settled",
                    RunEvent::Presented { .. } => "presented",
                    RunEvent::Consumed { .. } => "consumed",
                    RunEvent::Incorporated { .. } => "incorporated",
                    RunEvent::Lifecycle { .. } => "lifecycle",
                })
                .collect()
        })
        .collect()
}

fn decision(records: &[RunRecord]) -> Vec<CallDecision> {
    records
        .iter()
        .flat_map(|record| &record.events)
        .filter_map(|event| match event {
            RunEvent::Decided { decision, .. } => Some(decision.clone()),
            _ => None,
        })
        .collect()
}

/// L02 and L15: a simple Done call is four records — A, X, D and V with its
/// incorporation — and a lost proposal at any of them, or a crash before the
/// handler's output, reruns only that record's step under the same call id
/// and attempt ordinal. A durable record never runs its step again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_done_singleton_is_four_records_and_reruns_only_unrecorded_work_at_every_cut() {
    let call = call("four-records");
    let steps = ["admit", "attempt:1", "decide", "present"];
    for cut in [None, Some(0), Some(1), Some(2), Some(3), Some(4)] {
        let crash = match cut {
            None => Vec::new(),
            Some(4) => vec![CrashPoint::BeforeFrame {
                ty: MessageType::OutputCommand,
            }],
            Some(step) => vec![CrashPoint::BeforeRunResult {
                name: Some(names(&call.call_id, &steps)[step].clone()),
            }],
        };
        let probe = Probe::new(Probe::done(), CancelAt::Never);
        let started = std::time::Instant::now();
        let driven = drive(
            0x4877,
            crash,
            call.clone(),
            call.clone(),
            Arc::clone(&probe),
        )
        .await;
        let rerun = |step| 1 + usize::from(cut == Some(step));
        assert_eq!(
            probe.prepares.load(Ordering::SeqCst),
            rerun(0),
            "cut {cut:?}"
        );
        assert_eq!(probe.before_checks.load(Ordering::SeqCst), rerun(0));
        assert_eq!(
            probe.executions(),
            rerun(1),
            "cut {cut:?}: durable X never repeats"
        );
        assert!(
            probe
                .executions
                .lock()
                .unwrap()
                .iter()
                .all(|executed| executed == &(call.call_id.clone(), AttemptOrdinal::FIRST)),
            "unrecorded work redelivers under its call id and attempt ordinal"
        );
        assert_eq!(probe.after_checks.load(Ordering::SeqCst), rerun(2));
        assert_eq!(probe.presentations.load(Ordering::SeqCst), rerun(3));
        let (terminal, records) = driven.finished().expect("the singleton finished");
        assert_eq!(
            terminal,
            SingletonTerminal::Final {
                source: ResultSource::Attempt {
                    attempt: AttemptOrdinal::FIRST,
                },
                capture: SingletonCapture::Done {
                    output: OUTPUT.to_owned(),
                    intents: Vec::new(),
                },
                presentation: PRESENTATION.to_owned(),
            }
        );
        assert_eq!(
            events(&records),
            vec![
                vec!["admitted"],
                vec!["attempt"],
                vec!["decided"],
                vec!["presented", "consumed", "incorporated"],
            ]
        );
        let (journaled, raw, bytes, calls) = driven.journal();
        assert_eq!(
            journaled,
            names(&call.call_id, &steps),
            "four source records"
        );
        assert_eq!(calls, 0, "no child invocation or group call");
        eprintln!(
            "C01-double width=1 cut={cut:?} source_records={} raw_engine_records={raw} \
             journal_rpc_commands={calls} journal_bytes={bytes} application_transactions=0 \
             serial_record_waits={} elapsed_us={}",
            journaled.len(),
            journaled.len(),
            started.elapsed().as_micros()
        );
    }
}

/// L03: the decision chooses once. A cancellation before the decision is
/// durable cancels the call after its issued attempt settles; one whose first
/// decision step ran but whose proposal was lost is read again by the replay;
/// one after the durable decision cannot abandon the final's declarations or
/// presentation. No cancelled call issues a declaration.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn final_and_cancel_choose_one_terminal_around_the_durable_decision() {
    let declaring = SingletonBodyOutcome::Done {
        output: OUTPUT.to_owned(),
        intents: vec![ToolIntentKind::EmitTrigger],
    };
    let mut call = call("cancel-once");
    call.declaration = ToolDeclaration::default().with_intents([ToolIntentKind::EmitTrigger]);
    let decide = names(&call.call_id, &["decide"]).remove(0);
    for (cancel_at, crash, cancelled) in [
        (CancelAt::Body, None, true),
        (
            CancelAt::AfterChecks,
            Some(CrashPoint::BeforeRunResult {
                name: Some(decide.clone()),
            }),
            true,
        ),
        (CancelAt::AfterChecks, None, false),
        (CancelAt::Presentation, None, false),
    ] {
        let probe = Probe::new(declaring.clone(), cancel_at);
        let driven = drive(
            0x4877,
            crash.into_iter().collect(),
            call.clone(),
            call.clone(),
            Arc::clone(&probe),
        )
        .await;
        let (terminal, records) = driven.finished().expect("the singleton finished");
        assert_eq!(
            probe.executions(),
            1,
            "{cancel_at:?}: the issued X settled once"
        );
        let decisions = decision(&records);
        let (journaled, ..) = driven.journal();
        if cancelled {
            assert_eq!(decisions, vec![CallDecision::Cancelled], "{cancel_at:?}");
            assert_eq!(
                terminal,
                SingletonTerminal::Withheld {
                    decision: CallDecision::Cancelled
                }
            );
            assert_eq!(
                probe.realizations.load(Ordering::SeqCst),
                0,
                "no post-cancel declaration"
            );
            assert_eq!(probe.presentations.load(Ordering::SeqCst), 0);
            assert_eq!(
                journaled,
                names(&call.call_id, &["admit", "attempt:1", "decide", "present"])
            );
        } else {
            assert!(
                matches!(
                    decisions.as_slice(),
                    [CallDecision::Final { declares: true, .. }]
                ),
                "{cancel_at:?}: {decisions:?}"
            );
            assert!(
                matches!(terminal, SingletonTerminal::Final { .. }),
                "{terminal:?}"
            );
            assert_eq!(
                *probe.realized.lock().unwrap(),
                vec![(call.call_id.clone(), ToolIntentKind::EmitTrigger)],
                "post-final cancellation cannot abandon protected work"
            );
            assert_eq!(
                journaled,
                names(
                    &call.call_id,
                    &["admit", "attempt:1", "decide", "declare", "present"]
                )
            );
        }
        assert_eq!(decisions.len(), 1, "exactly one final-or-cancel");
    }
}

/// L04: a final's declarations are issued after its decision is durable and
/// settle before its presentation, which shares its record with the
/// incorporation. A cut after each boundary resumes the exact prefix: the
/// declarations' fence lets one realization through however often they are
/// asked again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_final_settles_its_declarations_before_presentation_at_every_cut() {
    let mut call = call("declares");
    call.declaration = ToolDeclaration::default().with_intents([ToolIntentKind::EmitTrigger]);
    let steps = ["admit", "attempt:1", "decide", "declare", "present"];
    for cut in [2, 3, 4] {
        let probe = Probe::new(
            SingletonBodyOutcome::Done {
                output: OUTPUT.to_owned(),
                intents: vec![ToolIntentKind::EmitTrigger],
            },
            CancelAt::Never,
        );
        let driven = drive(
            0x4877,
            vec![CrashPoint::BeforeRunResult {
                name: Some(names(&call.call_id, &steps)[cut].clone()),
            }],
            call.clone(),
            call.clone(),
            Arc::clone(&probe),
        )
        .await;
        let (terminal, records) = driven.finished().expect("the singleton finished");
        assert!(matches!(terminal, SingletonTerminal::Final { .. }));
        assert_eq!(
            events(&records),
            vec![
                vec!["admitted"],
                vec!["attempt"],
                vec!["decided"],
                vec!["declarations_issued"],
                vec![
                    "declarations_settled",
                    "presented",
                    "consumed",
                    "incorporated"
                ],
            ]
        );
        assert_eq!(probe.executions(), 1);
        assert_eq!(
            probe.realizations.load(Ordering::SeqCst),
            1 + usize::from(cut == 4),
            "cut {cut}: only an unsettled declaration boundary asks again"
        );
        assert_eq!(probe.realized.lock().unwrap().len(), 1, "one realization");
        assert_eq!(driven.journal().0, names(&call.call_id, &steps));
    }
}

/// L12: a replay whose call no longer matches its recorded admission refuses,
/// typed, before any body: another tool name, other arguments, another owner,
/// or a recorded binding whose plugin revision this build no longer executes.
/// A changed declaration is not consulted: the recorded one governs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replay_that_drifts_from_its_admission_refuses_typed_before_the_body() {
    let original = call("drift");
    let mut renamed = original.clone();
    renamed.tool_name = "other".to_owned();
    let mut reargued = original.clone();
    reargued.arguments = serde_json::json!({ "label": "other" });
    let mut moved = original.clone();
    moved.owner = EffectOpener::turn("session", "other-turn");
    let mut upgraded = original.clone();
    upgraded.binding = binding(2);
    upgraded.available = vec![revision(2)];
    for (change, replay) in [
        ("tool name", renamed),
        ("arguments", reargued),
        ("owner", moved),
        ("revision", upgraded),
    ] {
        let probe = Probe::new(Probe::done(), CancelAt::Never);
        // The admission is durable; the attempt's command never reached the
        // journal.
        let driven = drive(
            0x4877,
            vec![CrashPoint::BeforeRun {
                name: names(&original.call_id, &["attempt:1"]).remove(0),
            }],
            original.clone(),
            replay,
            Arc::clone(&probe),
        )
        .await;
        let refusal = driven.finished().expect_err("the replay refuses");
        let typed = match change {
            "tool name" => matches!(
                refusal,
                SingletonRunError::Drift {
                    drift: SingletonDrift::ToolName,
                    ..
                }
            ),
            "arguments" => matches!(
                refusal,
                SingletonRunError::Drift {
                    drift: SingletonDrift::Arguments,
                    ..
                }
            ),
            "owner" => matches!(
                refusal,
                SingletonRunError::Ledger(RunEventRefusal::ForeignOwner)
            ),
            _ => matches!(
                &refusal,
                SingletonRunError::Admission(AdmissionRefusal::BindingUnavailable { member: 0, cause })
                    if cause.recorded == vec![revision(1)]
            ),
        };
        assert!(typed, "{change}: {refusal:?}");
        // The first attempt may have started its eager body before its
        // command frame was lost; the replay runs none.
        assert_eq!(
            probe.replay_executions.load(Ordering::SeqCst),
            0,
            "{change}: the replay ran no body"
        );
        assert!(probe.executions() <= 1);
        assert_eq!(
            probe.prepares.load(Ordering::SeqCst),
            1,
            "{change}: admitted once"
        );
        assert_eq!(driven.journal().0, names(&original.call_id, &["admit"]));
    }

    // A changed declaration is not consulted: the recorded one still refuses
    // the undeclared Deferred the body returned, and the body does not run
    // again.
    let mut capable = original.clone();
    capable.declaration = ToolDeclaration::deferring();
    let probe = Probe::new(deferred(&original.call_id), CancelAt::Never);
    let driven = drive(
        0x4877,
        vec![CrashPoint::BeforeRunResult {
            name: Some(names(&original.call_id, &["decide"]).remove(0)),
        }],
        original.clone(),
        capable,
        Arc::clone(&probe),
    )
    .await;
    let (terminal, _) = driven.finished().expect("the recorded declaration governs");
    assert_eq!(probe.executions(), 1);
    assert!(
        matches!(
            terminal,
            SingletonTerminal::Final {
                capture: SingletonCapture::Refused {
                    refusal: DeclarationRefusal::UndeclaredDeferral
                },
                ..
            }
        ),
        "{terminal:?}"
    );
}

fn deferred(call_id: &ToolCallId) -> SingletonBodyOutcome {
    SingletonBodyOutcome::Deferred {
        source: lash_core::AwaitEventKey {
            scope: lash_core::ExecutionScope::turn("session", "turn"),
            wait: lash_core::AwaitEventWaitIdentity::tool_completion(call_id.clone()),
            key_id: format!("fig4877-{call_id}"),
            signature: "fig4877".to_owned(),
        },
    }
}

/// An outcome the admitted declaration does not admit is refused in its
/// attempt record before anything it declared is realized: an undeclared
/// intent and an undeclared Deferred each become the call's final refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_undeclared_outcome_is_refused_before_anything_it_declared_is_realized() {
    for (label, body, expected) in [
        (
            "undeclared-intent",
            SingletonBodyOutcome::Done {
                output: OUTPUT.to_owned(),
                intents: vec![ToolIntentKind::StartProcess],
            },
            DeclarationRefusal::UndeclaredIntent {
                kind: ToolIntentKind::StartProcess,
            },
        ),
        (
            "undeclared-deferral",
            deferred(&ToolCallId::fixture("undeclared-deferral")),
            DeclarationRefusal::UndeclaredDeferral,
        ),
    ] {
        let call = call(label);
        let probe = Probe::new(body, CancelAt::Never);
        let driven = drive(
            0x4877,
            Vec::new(),
            call.clone(),
            call.clone(),
            Arc::clone(&probe),
        )
        .await;
        let (terminal, records) = driven.finished().expect("the singleton finished");
        assert_eq!(
            terminal,
            SingletonTerminal::Final {
                source: ResultSource::Attempt {
                    attempt: AttemptOrdinal::FIRST,
                },
                capture: SingletonCapture::Refused { refusal: expected },
                presentation: PRESENTATION.to_owned(),
            }
        );
        assert!(
            matches!(
                decision(&records).as_slice(),
                [CallDecision::Final {
                    declares: false,
                    ..
                }]
            ),
            "{label}: a refused outcome declares nothing"
        );
        assert_eq!(probe.realizations.load(Ordering::SeqCst), 0, "{label}");
    }
}
