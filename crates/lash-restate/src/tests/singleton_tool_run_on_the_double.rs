//! The singleton Run route (FIG-4877) through a real handler on the
//! in-process Restate server double.
//!
//! Each law runs one tool call as a one-member round inside a
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
use lash_core::runtime::AttemptStream;
use lash_core::store::plugin_writers::PluginCallbackIdentity;
use lash_core::tool_dispatch::{
    BeforeCheckReply, DeclaredStartObligation, SingletonAttempt, SingletonBodyOutcome,
    SingletonCapture, SingletonDrift, SingletonPreparedRequest, SingletonRunError,
    SingletonTerminal, SingletonToolCall, SingletonToolHandlers,
};
use lash_core::tool_run::{
    AdmissionRefusal, AdmittedBinding, AfterCheckVerdict, AttemptOrdinal, AttributedVerdict,
    CallDecision, DeclarationRefusal, ExternalCancelPolicy, PresentationBinding, ResultSource,
    RunEvent, RunEventRefusal, RunRecord, SegmentOrdinal, ToolDeclaration,
};
use lash_core::{AdmittedScope, EffectOpener, ToolCallId};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, ServerConfig};
use lash_sansio::ToolIntentKind;

use super::{SingletonRunOutcome, run_singleton};

/// Q2/K3: singleton, parallel and deferred tools settle in their operation Run on replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_plugin_task_records_its_tool_in_the_operation_run() {
    use lash_core::facade_support::{PluginOperation, PluginTask, SessionParam};
    struct Task;
    impl PluginOperation for Task {
        const NAME: &'static str = "probe.task";
        const DESCRIPTION: &'static str = "Operation Run tool law";
        const SESSION_PARAM: SessionParam = SessionParam::Required;
        type Args = String;
        type Output = String;
        type Error = String;
        const ERROR_TYPE: &'static str = "probe.task";
        const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
        fn error_class(_: &String) -> lash_sansio::PluginFailureClass {
            lash_sansio::PluginFailureClass::Terminal
        }
    }
    impl PluginTask for Task {}
    let probe = Probe::new(Probe::done(), CancelAt::Never);
    let task_probe = probe.clone();
    let spec = lash_core::facade_support::PluginSpec::new().with_plugin_task_typed::<Task, _, _>(
        move |ctx, label| {
            let probe = task_probe.clone();
            async move {
                let mut call = call(&label);
                let lash_core::ExecutionScope::SessionOperation {
                    session_id,
                    operation_id,
                } = ctx.scoped_effect_controller.execution_scope()
                else {
                    panic!("operation scope")
                };
                call.owner =
                    EffectOpener::session_operation(session_id.clone(), operation_id.clone());
                if label == "singleton" {
                    // Admission, decision and presentation borrow their call and handlers.
                    let result = run_singleton(
                        &ctx.scoped_effect_controller,
                        &call,
                        Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    assert!(matches!(result.terminal, SingletonTerminal::Final { .. }));
                } else {
                    use lash_core::tool_dispatch::RunCoordinator;
                    use lash_core::tool_run::{
                        RecordedRetryPolicy, SourceAuthority, SourceDescriptor, SourceSeal,
                    };
                    call.declaration = ToolDeclaration::deferring();
                    let mut run = RunCoordinator::open(
                        &ctx.scoped_effect_controller,
                        call.owner.clone(),
                        call.segment,
                        call.available.clone(),
                    );
                    if label == "parallel" {
                        let mut sibling = call.clone();
                        sibling.call_id = ToolCallId::fixture("parallel-sibling");
                        super::decide_round(
                            &mut run,
                            &[call, sibling],
                            probe,
                            RecordedRetryPolicy::Never,
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                        ctx.scoped_effect_controller
                            .controller()
                            .start_run_retry(1)
                            .await
                            .map_err(|error| error.to_string())?;
                        assert_eq!(
                            run.drain().await.map_err(|error| error.to_string())?.len(),
                            2
                        );
                    } else {
                        let source = ctx
                            .scoped_effect_controller
                            .controller()
                            .await_event_key(
                                call.owner.admitted_scope().scope(),
                                lash_core::AwaitEventWaitIdentity::tool_completion(
                                    call.call_id.clone(),
                                ),
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                        let deferred = Probe::new(
                            SingletonBodyOutcome::Deferred {
                                source: source.clone(),
                            },
                            CancelAt::Never,
                        );
                        super::decide_round(
                            &mut run,
                            std::slice::from_ref(&call),
                            deferred,
                            RecordedRetryPolicy::Never,
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                        let seal = ctx
                            .scoped_effect_controller
                            .controller()
                            .cancel_run_source(SourceDescriptor {
                                source,
                                call_id: call.call_id.clone(),
                                owner: call.owner.clone(),
                                resolver: revision(1),
                                authority: SourceAuthority::ExternalCompletion,
                                cancel: call.cancel,
                            })
                            .await
                            .map_err(|error| error.to_string())?;
                        assert_eq!(seal, SourceSeal::Cancelled);
                        run.await_deferred()
                            .await
                            .map_err(|error| error.to_string())?;
                        let terminals = run.drain().await.map_err(|error| error.to_string())?;
                        assert!(matches!(
                            &terminals[0].1,
                            SingletonTerminal::Withheld {
                                decision: CallDecision::Cancelled
                            }
                        ));
                    }
                    run.close().await.map_err(|error| error.to_string())?;
                }
                Ok(lash_core::plugin::PluginOperationOutcome::new(
                    PRESENTATION.to_owned(),
                ))
            }
        },
    );
    let backend = lash_restate_test::backend(
        0x4941,
        ServerConfig {
            protocol: lash_restate_test::protocol::ProtocolVersion::V7,
            ..ServerConfig::default()
        }
        .always_replay(true),
    )
    .await
    .unwrap();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("operation-tool-law")
        .complete(|_| async {
            Ok::<_, lash_core::llm::transport::LlmTransportError>(
                lash_core::llm::types::LlmResponse::default(),
            )
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend.lash_backend())
        .serve_test_llm_profile(
            provider,
            lash_core::testing::test_llm_profile_metadata("mock-model"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .plugin(Arc::new(lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial(PLUGIN),
            spec,
        )))
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "operation-tool-law",
            "owner",
        ))
        .unwrap();
    core.session("operation-tool-law")
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "mock-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .unwrap();
    let session = core.session("operation-tool-law").open().await.unwrap();
    let mut runs = Vec::new();
    for label in ["singleton", "parallel", "deferred"] {
        let task = session
            .plugin_operations()
            .start_task::<Task>(label.into(), label)
            .await
            .unwrap();
        runs.push((label, task.run().clone()));
        let result = tokio::time::timeout(Duration::from_secs(5), task.result())
            .await
            .unwrap_or_else(|error| {
                let journals: Vec<_> = backend
                    .server()
                    .invocations()
                    .into_iter()
                    .map(|invocation| {
                        (
                            invocation.target,
                            backend
                                .server()
                                .journal(&invocation.id)
                                .unwrap_or_default()
                                .into_iter()
                                .map(|entry| (entry.ty, entry.name))
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect();
                panic!("{label} must settle its tool: {error:?}; {journals:?}");
            })
            .expect("native operation tool call succeeds");
        assert_eq!(result.output, serde_json::json!(PRESENTATION));
    }
    assert_eq!(probe.executions(), 3, "replay serves the accepted attempts");
    assert_eq!(
        probe.prepares.load(Ordering::SeqCst),
        3,
        "replay serves admission"
    );
    let journals: Vec<_> = backend
        .server()
        .invocations()
        .into_iter()
        .filter_map(|invocation| {
            let entries = backend.server().journal(&invocation.id).unwrap_or_default();
            let names: Vec<_> = entries
                .iter()
                .filter_map(|entry| entry.name.as_ref())
                .filter(|name| name.starts_with("lash:run:"))
                .cloned()
                .collect();
            let records: Vec<RunRecord> = entries
                .iter()
                .filter_map(|entry| {
                    let Ok(bytes) = entry.run_completion()? else {
                        return None;
                    };
                    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    value.get("record").cloned().map(|record| {
                        serde_json::from_value(record).expect("a completed Run records its events")
                    })
                })
                .collect();
            (!names.is_empty()).then_some((invocation.target, names, records))
        })
        .collect();
    assert_eq!(
        journals.len(),
        3,
        "one operation journal owns each task's entire call"
    );
    for (label, run) in runs {
        let key = crate::recorded_turn_invocation_key(
            backend.stores().session_store_factory().as_ref(),
            &lash_core::SessionId::from("operation-tool-law"),
            &run,
        )
        .await
        .unwrap()
        .expect("the operation has a physical executor");
        let journal = journals
            .iter()
            .find(|(target, _, _)| target.ends_with(&format!("/{key}/run")))
            .expect("the journal belongs to the public task Run");
        assert!(
            journal.0.starts_with("LashTurn/"),
            "operation Run uses the turn service: {journal:?}"
        );
        let steps = match label {
            // Every decision and every V is a `lash:run:schedule:` record.
            "singleton" | "parallel" | "deferred" => &["admit", "attempt", "schedule"][..],
            _ => unreachable!(),
        };
        let events: Vec<_> = journal.2.iter().flat_map(|record| &record.events).collect();
        let admitted: std::collections::BTreeSet<_> = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::Admitted { round } => Some(&round.members),
                _ => None,
            })
            .flatten()
            .map(|member| &member.call_id)
            .collect();
        let decided: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::Decided {
                    call_id, decision, ..
                } => Some((call_id, decision)),
                _ => None,
            })
            .collect();
        assert_eq!(admitted.len(), if label == "parallel" { 2 } else { 1 });
        assert_eq!(
            decided.len(),
            admitted.len(),
            "one decision per selected call"
        );
        assert_eq!(
            decided
                .iter()
                .map(|(id, _)| *id)
                .collect::<std::collections::BTreeSet<_>>(),
            admitted,
            "the recorded decisions belong to the admitted calls",
        );
        for (_, decision) in decided {
            if label == "deferred" {
                assert!(matches!(decision, CallDecision::Cancelled));
            } else {
                assert!(matches!(decision, CallDecision::Final { .. }));
            }
        }
        for name in steps {
            assert!(
                journal.1.iter().any(|entry| entry.contains(name)),
                "missing {name}: {journal:?}"
            );
        }
    }
}

const PLUGIN: &str = "fig4877-tools";
const OUTPUT: &str = "fig4877 done";
const PRESENTATION: &str = "fig4877 presented";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logical_receipts_follow_recorded_admission_and_protected_presentation() {
    for always_replay in [false, true] {
        for (label, admitted, owner, cancel_at) in [
            (
                "turn",
                AdmittedScope::turn("session", "turn"),
                EffectOpener::turn("session", "turn"),
                CancelAt::Never,
            ),
            (
                "run",
                AdmittedScope::turn("session", "run"),
                EffectOpener::turn("session", "run"),
                CancelAt::Never,
            ),
            (
                "process",
                AdmittedScope::session_operation("session", "process-receipt-fixture"),
                EffectOpener::process(lash_sansio::ProcessId::fixture("receipt-process")),
                CancelAt::Never,
            ),
            (
                "operation",
                AdmittedScope::session_operation("session", "operation"),
                EffectOpener::session_operation("session", "operation"),
                CancelAt::Never,
            ),
            (
                "cancelled",
                AdmittedScope::turn("session", "cancelled"),
                EffectOpener::turn("session", "cancelled"),
                CancelAt::Body,
            ),
        ] {
            let backend = lash_restate_test::backend(
                0x4830,
                ServerConfig::default().always_replay(always_replay),
            )
            .await
            .unwrap();
            let sink = Arc::new(super::RecordingTraceSink::default());
            let tracing =
                lash_core::facade_support::TraceRuntime::default().with_trace_sink(sink.clone());
            let tracing = if label == "run" {
                let core = lash::LashCore::standard_builder(backend.lash_backend())
                    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .trace_runtime(tracing)
                    .build(lash_core::LeaseOwnerIdentity::opaque(
                        "receipt-fixture",
                        "custom-tracing",
                    ))
                    .unwrap();
                core.durable_process_worker_config()
                    .unwrap()
                    .runtime_host
                    .tracing
            } else {
                tracing.with_tool_receipts(backend.engine_stores().clone())
            };
            let probe = Probe::new(Probe::done(), cancel_at);
            let mut call = call(label);
            call.owner = owner;
            let original = call.call_id.clone();
            let handler_runs = Arc::new(AtomicUsize::new(0));
            let attempt: lash_restate_test::HandlerAttempt = {
                let probe = probe.clone();
                let handler_runs = handler_runs.clone();
                Arc::new(move |scoped| {
                    let tracing = tracing.clone();
                    let probe = probe.clone();
                    let call = call.clone();
                    let handler_runs = handler_runs.clone();
                    Box::pin(async move {
                        handler_runs.fetch_add(1, Ordering::SeqCst);
                        let process_scope;
                        let scoped = if label == "process" {
                            // The record-only fixture lends the engine to the
                            // process owner; it dispatches no process effect.
                            process_scope = lash_core::ScopedEffectController::borrowed(
                                scoped.controller(),
                                AdmittedScope::process(lash_sansio::ProcessId::fixture(
                                    "receipt-process",
                                )),
                            )
                            .unwrap()
                            .in_drive_of(&scoped);
                            &process_scope
                        } else {
                            &scoped
                        };
                        let scoped = if label == "run" {
                            (*scoped)
                                .clone()
                                .with_trace_scope(lash_trace::DurableTraceScope {
                                    scope: lash_trace::TraceScopeId::admission(
                                        lash_trace::TraceScopeOwner::Run {
                                            session_id: "session".into(),
                                            run: "run".into(),
                                        },
                                    ),
                                    cause: lash_trace::TraceCause::Root,
                                    anchor: lash_trace::TraceAnchor::Untraced,
                                    started_at_ms: 1,
                                })
                        } else {
                            (*scoped).clone()
                        };
                        tracing.turn_execution(&scoped);
                        run_singleton(
                            &scoped,
                            &call,
                            Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                        )
                        .await
                        .unwrap();
                    })
                })
            };
            backend.run_in_handler(admitted, attempt).await.unwrap();
            let records = sink.records.lock().unwrap();
            let receipts = records
                .iter()
                .filter_map(|record| match &record.event {
                    lash_trace::TraceEvent::ToolReceipt {
                        call_id, terminal, ..
                    } => Some((call_id, terminal)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let terminal = if cancel_at == CancelAt::Body {
                lash_trace::TraceToolTerminal::Cancelled
            } else {
                lash_trace::TraceToolTerminal::Final
            };
            assert_eq!(
                receipts,
                [(&original, &None), (&original, &Some(terminal))],
                "{label} replay={always_replay}"
            );
            assert_eq!(
                probe.executions(),
                1,
                "serving recorded X never reruns its body"
            );
            assert_eq!(
                probe.presentations.load(Ordering::SeqCst),
                usize::from(cancel_at == CancelAt::Never)
            );
            if always_replay {
                assert!(handler_runs.load(Ordering::SeqCst) > 1);
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_spending_calls_and_unreturned_losers_keep_recorded_provider_results() {
    use lash_core::llm::types::{LlmRequest, LlmRequestScope, LlmResponse, LlmUsage};
    use lash_core::tool_run::{
        AdmittedCall, AttemptResult, CheckRecord, MaterialEntry, MaterialLocation, MaterialOwner,
        MaterialPayload, MaterialRole, RoundAdmission, RunJournalEntry, RunLedger,
        RuntimeCallPolicy,
    };
    let backend = lash_restate_test::backend(0x4833, ServerConfig::default().always_replay(true))
        .await
        .unwrap();
    let sink = Arc::new(super::RecordingTraceSink::default());
    let tracing = lash_core::facade_support::TraceRuntime::default()
        .with_tool_receipts(backend.engine_stores().clone())
        .with_trace_sink(sink.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let retained = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let calls = calls.clone();
        let retained = retained.clone();
        Arc::new(move |scoped| {
            let calls = calls.clone();
            let retained = retained.clone();
            let tracing = tracing.clone();
            Box::pin(async move {
                tracing.turn_execution(&scoped);
                let owner = EffectOpener::turn("receipt-spend", "turn");
                let request = MaterialPayload::new(
                    MaterialOwner::Run {
                        opener: owner.clone(),
                    },
                    MaterialRole::PreparedRequest,
                    None,
                    "{}".into(),
                )
                .reference(MaterialLocation::JournalLocal)
                .unwrap();
                let ids = [
                    ToolCallId::fixture("cancelled-spender"),
                    ToolCallId::fixture("unreturned-loser"),
                ];
                let round = RoundAdmission {
                    owner: owner.clone(),
                    members: ids
                        .iter()
                        .map(|call_id| AdmittedCall {
                            call_id: call_id.clone(),
                            tool_name: "spend".into(),
                            request: request.clone(),
                            declaration: ToolDeclaration::default(),
                            binding: binding(1),
                            policy: RuntimeCallPolicy::default(),
                            checks: CheckRecord::reduce(Vec::new()),
                        })
                        .collect(),
                    operands: vec![0, 1],
                    capacity: lash_core::tool_run::CapacityScope::Held,
                };
                round.clone().admit(&[revision(1)], |_| true).unwrap();
                let mut ledger = RunLedger::new(owner.clone());
                let entry = scoped
                    .controller()
                    .record_run_record(
                        "spend-admission".into(),
                        Box::pin(async move {
                            Ok(RunJournalEntry {
                                state: Vec::new(),
                                record: RunRecord {
                                    segment: SegmentOrdinal(0),
                                    first: lash_core::tool_run::RunEventOrdinal(0),
                                    events: vec![RunEvent::Admitted { round }],
                                    trace: None,
                                },
                                materials: Vec::new(),
                            })
                        }),
                    )
                    .await
                    .unwrap();
                ledger.append(SegmentOrdinal(0), &entry.record).unwrap();
                let mut results = Vec::new();
                for call_id in &ids {
                    let call_id = call_id.clone();
                    let owner = owner.clone();
                    let calls = calls.clone();
                    let first = ledger.next_ordinal();
                    let entry = scoped.controller().record_run_record(format!("spend-output-{call_id}"), Box::pin(async move {
                        let mut provider = lash_core::testing::TestProvider::builder().kind("recorded-spend").complete(move |request: LlmRequest| {
                            calls.fetch_add(1, Ordering::SeqCst);
                            let response = LlmResponse {
                                usage: LlmUsage { input_tokens: 11, output_tokens: 7, ..Default::default() },
                                provider_usage: Some(serde_json::json!({"reported_tokens": 18})),
                                response_metadata: std::collections::BTreeMap::from([("header:x-request-id".into(), serde_json::json!(format!("provider:{}", request.request_id())))]),
                                ..Default::default()
                            };
                            async move { Ok::<_, lash_core::llm::transport::LlmTransportError>(response) }
                        }).build().into_handle();
                        let metadata = lash_core::LlmProfileMetadata::builder("spend-model").context_window_tokens(1024).build().unwrap();
                        let completion = provider.complete(LlmRequest {
                            instructions: None,
                            model: lash_core::testing::test_llm_profile_config("spend-model", metadata),
                            messages: Vec::new(), resolved_stored: Default::default(), tools: Default::default(), tool_choice: Default::default(), attachment_acceptance: Default::default(), generation: Default::default(),
                            scope: LlmRequestScope::new("receipt-spend", "frame", call_id.to_string()),
                            output_spec: None, stream_events: None, provider_trace: None,
                        }).await.unwrap();
                        let payload = MaterialPayload::new(MaterialOwner::Run { opener: owner }, MaterialRole::AttemptOutput, None, serde_json::json!({"response": completion.response, "call_record": completion.call_record}).to_string());
                        let output = payload.reference(MaterialLocation::JournalLocal).unwrap();
                        Ok(RunJournalEntry {
                            state: Vec::new(),
                            record: RunRecord { segment: SegmentOrdinal(0), first, events: vec![RunEvent::AttemptRecorded { call_id, attempt: AttemptOrdinal::FIRST, result: AttemptResult::Done { output: output.clone() } }], trace: None },
                            materials: vec![MaterialEntry::Available { reference: output, payload: Box::new(payload) }],
                        })
                    })).await.unwrap();
                    ledger.append(SegmentOrdinal(0), &entry.record).unwrap();
                    results.push(entry);
                }
                let first = ledger.next_ordinal();
                let entry = scoped
                    .controller()
                    .record_run_record(
                        "spend-cancel-and-abort".into(),
                        Box::pin(async move {
                            Ok(RunJournalEntry {
                                state: Vec::new(),
                                record: RunRecord {
                                    segment: SegmentOrdinal(0),
                                    first,
                                    events: vec![
                                        RunEvent::Decided {
                                            call_id: ids[0].clone(),
                                            rank: 1,
                                            decision: CallDecision::Cancelled,
                                            after: None,
                                        },
                                        RunEvent::Decided {
                                            call_id: ids[1].clone(),
                                            rank: 2,
                                            decision: CallDecision::Aborted,
                                            after: Some(CheckRecord::reduce(vec![AttributedVerdict {
                                                callback: binding(1).executable,
                                                verdict: AfterCheckVerdict::AbortRun {
                                                    cause: lash_core::tool_run::HookCause {
                                                        error_type: "spend-abort".into(),
                                                        error_version: std::num::NonZeroU32::new(1).unwrap(),
                                                        payload: serde_json::json!({"withhold": true}),
                                                    },
                                                },
                                            }])),
                                        },
                                    ],
                                    trace: None,
                                },
                                materials: Vec::new(),
                            })
                        }),
                    )
                    .await
                    .unwrap();
                ledger.append(SegmentOrdinal(0), &entry.record).unwrap();
                assert!(ledger.aborted());
                *retained.lock().unwrap() = results;
            })
        })
    };
    backend
        .run_in_handler(AdmittedScope::turn("receipt-spend", "turn"), attempt)
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "recorded X never dispatches its provider again"
    );
    let retained = retained.lock().unwrap();
    assert_eq!(retained.len(), 2, "both withheld calls retain their X");
    for (entry, id) in retained
        .iter()
        .zip(["cancelled-spender", "unreturned-loser"])
    {
        assert!(
            !entry
                .record
                .events
                .iter()
                .any(|event| matches!(event, RunEvent::Presented { .. }))
        );
        let MaterialEntry::Available { payload, .. } = &entry.materials[0] else {
            panic!("recorded provider result is retained")
        };
        let result: serde_json::Value = serde_json::from_str(&payload.text).unwrap();
        assert_eq!(result["response"]["usage"]["input_tokens"], 11);
        assert_eq!(result["response"]["provider_usage"]["reported_tokens"], 18);
        assert_eq!(
            result["response"]["response_metadata"]["header:x-request-id"],
            format!("provider:{}", ToolCallId::fixture(id))
        );
        assert_eq!(
            result["call_record"]["call_id"],
            ToolCallId::fixture(id).to_string()
        );
        assert_eq!(
            result["call_record"]["attempts"][0]["usage"]["output_tokens"],
            7
        );
    }
    let records = sink.records.lock().unwrap();
    let terminals = records
        .iter()
        .filter_map(|record| match &record.event {
            lash_trace::TraceEvent::ToolReceipt {
                call_id,
                terminal: Some(terminal),
                ..
            } => Some((call_id.to_string(), *terminal)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        terminals,
        [
            (
                ToolCallId::fixture("cancelled-spender").to_string(),
                lash_trace::TraceToolTerminal::Cancelled
            ),
            (
                ToolCallId::fixture("unreturned-loser").to_string(),
                lash_trace::TraceToolTerminal::Aborted
            )
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grouped_isolated_and_deferred_receipts_survive_prefix_restoration() {
    use lash_core::tool_run::{
        AdmittedCall, AttemptResult, CheckRecord, MaterialLocation, MaterialOwner, MaterialPayload,
        MaterialRole, RoundAdmission, RunJournalEntry, RunLedger, RuntimeCallPolicy,
    };
    let backend = lash_restate_test::backend(0x4831, ServerConfig::default().always_replay(true))
        .await
        .unwrap();
    let sink = Arc::new(super::RecordingTraceSink::default());
    let tracing = lash_core::facade_support::TraceRuntime::default()
        .with_tool_receipts(backend.engine_stores().clone())
        .with_trace_sink(sink.clone());
    let attempt: lash_restate_test::HandlerAttempt = {
        let sink = sink.clone();
        Arc::new(move |scoped| {
            let tracing = tracing.clone();
            let sink = sink.clone();
            Box::pin(async move {
                tracing.turn_execution(&scoped);
                let owner = EffectOpener::turn("receipt-group", "turn");
                let payload = MaterialPayload::new(
                    MaterialOwner::Run {
                        opener: owner.clone(),
                    },
                    MaterialRole::PreparedRequest,
                    None,
                    "{}".into(),
                );
                let request = payload.reference(MaterialLocation::JournalLocal).unwrap();
                let members = ["grouped", "isolated", "deferred"]
                    .into_iter()
                    .map(|label| {
                        let mut declaration = ToolDeclaration::default();
                        declaration.isolated = label == "isolated";
                        declaration.may_defer = !declaration.isolated;
                        AdmittedCall {
                            call_id: ToolCallId::fixture(label),
                            tool_name: "probe".into(),
                            request: request.clone(),
                            declaration,
                            binding: binding(1),
                            policy: RuntimeCallPolicy {
                                retry: if label == "grouped" {
                                    lash_core::tool_run::RecordedRetryPolicy::Reported {
                                        max_attempts: std::num::NonZeroU32::new(2).unwrap(),
                                        base_delay_ms: 0,
                                        max_delay_ms: 0,
                                    }
                                } else {
                                    Default::default()
                                },
                                ..Default::default()
                            },
                            checks: CheckRecord::reduce(Vec::new()),
                        }
                    })
                    .collect::<Vec<_>>();
                let source = scoped
                    .controller()
                    .await_event_key(
                        scoped.execution_scope(),
                        lash_core::AwaitEventWaitIdentity::Custom {
                            key: "receipt-deferred".into(),
                        },
                    )
                    .await
                    .unwrap();
                let a = members[0].call_id.clone();
                let b = members[1].call_id.clone();
                let c = members[2].call_id.clone();
                let isolated_output = MaterialPayload::new(
                    MaterialOwner::Run {
                        opener: owner.clone(),
                    },
                    MaterialRole::AttemptOutput,
                    None,
                    "isolated done".into(),
                )
                .reference(MaterialLocation::JournalLocal)
                .unwrap();
                let resolved = MaterialPayload::new(
                    MaterialOwner::Source {
                        source: source.clone(),
                    },
                    MaterialRole::AttemptOutput,
                    None,
                    "resolved".into(),
                )
                .reference(MaterialLocation::JournalLocal)
                .unwrap();
                let batches = vec![
                    vec![RunEvent::Admitted {
                        round: RoundAdmission {
                            owner: owner.clone(),
                            members,
                            operands: vec![0, 1, 2],
                            capacity: lash_core::tool_run::CapacityScope::Held,
                        },
                    }],
                    vec![
                        RunEvent::AttemptRecorded {
                            call_id: a.clone(),
                            attempt: AttemptOrdinal::new(1).unwrap(),
                            result: AttemptResult::Failed {
                                output: isolated_output.clone(),
                                retryable: true,
                            },
                        },
                        RunEvent::AttemptRecorded {
                            call_id: b.clone(),
                            attempt: AttemptOrdinal::new(1).unwrap(),
                            result: AttemptResult::Done {
                                output: isolated_output,
                            },
                        },
                        RunEvent::AttemptRecorded {
                            call_id: c.clone(),
                            attempt: AttemptOrdinal::FIRST,
                            result: AttemptResult::Deferred {
                                source: source.clone(),
                            },
                        },
                    ],
                    vec![
                        RunEvent::RetryTimerRegistered {
                            call_id: a.clone(),
                            failed: AttemptOrdinal::FIRST,
                            next: AttemptOrdinal::new(2).unwrap(),
                            backoff_ms: 0,
                        },
                        RunEvent::RetryScheduled {
                            call_id: a.clone(),
                            failed: AttemptOrdinal::FIRST,
                            next: AttemptOrdinal::new(2).unwrap(),
                            backoff_ms: 0,
                        },
                        RunEvent::AttemptRecorded {
                            call_id: a.clone(),
                            attempt: AttemptOrdinal::new(2).unwrap(),
                            result: AttemptResult::Deferred {
                                source: source.clone(),
                            },
                        },
                    ],
                    vec![
                        RunEvent::Decided {
                            call_id: a.clone(),
                            rank: 1,
                            decision: CallDecision::Cancelled,
                            after: None,
                        },
                        RunEvent::Decided {
                            call_id: b.clone(),
                            rank: 2,
                            decision: CallDecision::Final {
                                source: ResultSource::Attempt {
                                    attempt: AttemptOrdinal::FIRST,
                                },
                                declares: false,
                            },
                            after: Some(CheckRecord::reduce(Vec::new())),
                        },
                        RunEvent::Decided {
                            call_id: c.clone(),
                            rank: 3,
                            decision: CallDecision::Final {
                                source: ResultSource::DeferredCompletion {
                                    attempt: AttemptOrdinal::new(1).unwrap(),
                                    resolved: Box::new(resolved),
                                },
                                declares: true,
                            },
                            after: Some(CheckRecord::reduce(Vec::new())),
                        },
                    ],
                    vec![
                        RunEvent::DeclarationsIssued { call_id: c.clone() },
                        RunEvent::DeclarationsSettled { call_id: c.clone() },
                    ],
                    vec![RunEvent::Presented {
                        call_id: b.clone(),
                        presentation: None,
                        failure: None,
                    }],
                    vec![RunEvent::Presented {
                        call_id: c.clone(),
                        presentation: None,
                        failure: None,
                    }],
                ];
                let mut ledger = RunLedger::new(owner);
                let mut prefix = Vec::new();
                for (index, events) in batches.into_iter().enumerate() {
                    let record = RunRecord {
                        segment: SegmentOrdinal(0),
                        first: ledger.next_ordinal(),
                        events,
                        trace: None,
                    };
                    for event in &record.events {
                        if let RunEvent::Admitted { round } = event {
                            round.clone().admit(&[revision(1)], |_| true).unwrap();
                        }
                    }
                    let boundary_sink = sink.clone();
                    let entry = scoped
                        .controller()
                        .record_run_record(
                            format!("receipt-fixture-{index}"),
                            Box::pin(async move {
                                if index == 1 || index == 4 {
                                    let count = boundary_sink
                                        .records
                                        .lock()
                                        .unwrap()
                                        .iter()
                                        .filter(|record| record.event.kind() == "tool_receipt")
                                        .count();
                                    assert_eq!(
                                        count,
                                        if index == 1 { 3 } else { 4 },
                                        "no terminal from X or protected declaration work"
                                    );
                                }
                                Ok(RunJournalEntry {
                                    state: Vec::new(),
                                    record,
                                    materials: Vec::new(),
                                })
                            }),
                        )
                        .await
                        .unwrap();
                    ledger.append(SegmentOrdinal(0), &entry.record).unwrap();
                    prefix.push(entry.record);
                    if index == 1 {
                        // Re-adopted material retains the same call identity;
                        // only SQL owns emission rights on prefix restoration.
                        for record in &mut prefix {
                            if let Some(RunEvent::Admitted { round }) = record.events.first_mut() {
                                for member in &mut round.members {
                                    member.request.location = MaterialLocation::RetainedArtifact {
                                        artifact: lash_core::ArtifactName {
                                            store: lash_core::ArtifactStoreId::ToolMaterial,
                                            artifact_ref: "retained-receipt-request".into(),
                                        },
                                    };
                                }
                            }
                            let record = record.clone();
                            scoped
                                .controller()
                                .record_run_record(
                                    format!("retained-prefix-{}", record.first.0),
                                    Box::pin(async move {
                                        Ok(RunJournalEntry {
                                            state: Vec::new(),
                                            record,
                                            materials: Vec::new(),
                                        })
                                    }),
                                )
                                .await
                                .unwrap();
                        }
                    }
                }
            })
        })
    };
    backend
        .run_in_handler(AdmittedScope::turn("receipt-group", "turn"), attempt)
        .await
        .unwrap();
    let records = sink.records.lock().unwrap();
    let receipts = records
        .iter()
        .filter_map(|record| match &record.event {
            lash_trace::TraceEvent::ToolReceipt {
                call_id, terminal, ..
            } => Some((call_id.clone(), *terminal)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        receipts,
        [
            (ToolCallId::fixture("grouped"), None),
            (ToolCallId::fixture("isolated"), None),
            (ToolCallId::fixture("deferred"), None),
            (
                ToolCallId::fixture("grouped"),
                Some(lash_trace::TraceToolTerminal::Cancelled)
            ),
            (
                ToolCallId::fixture("isolated"),
                Some(lash_trace::TraceToolTerminal::Final)
            ),
            (
                ToolCallId::fixture("deferred"),
                Some(lash_trace::TraceToolTerminal::Final)
            ),
        ]
    );
}

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
            presenter: Some(callback("present:probe")),
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
        cancel: ExternalCancelPolicy::Ignore,
        environment: None,
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
            commands: Default::default(),
            output: OUTPUT.to_owned(),
            intents: Vec::new(),
            start: None,
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
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        self.before_checks.fetch_add(1, Ordering::SeqCst);
        Ok(vec![AttributedVerdict {
            callback: binding(1).executable,
            verdict: BeforeCheckReply::Allow,
        }])
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
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        self.after_checks.fetch_add(1, Ordering::SeqCst);
        if self.cancel_at == CancelAt::AfterChecks {
            self.cancel.store(true, Ordering::SeqCst);
        }
        Ok(Vec::new())
    }

    async fn run_cancel_requested(&self) -> Result<bool, String> {
        Ok(self.cancel.load(Ordering::SeqCst))
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
    ) -> Result<String, lash_core::tool_dispatch::SingletonPresentationError> {
        self.presentations.fetch_add(1, Ordering::SeqCst);
        let declared = match capture {
            SingletonCapture::Isolated { .. } => {
                panic!("an isolated result bypasses the ordinary presenter")
            }
            SingletonCapture::Done { intents, .. } => intents.len(),
            SingletonCapture::Failed { .. }
            | SingletonCapture::RetryableFailure { .. }
            | SingletonCapture::Refused { .. }
            | SingletonCapture::StartRefused { .. } => 0,
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

    fn emit_stream(&self, _call_id: &ToolCallId, _stream: &AttemptStream) {}

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
                let outcome = run_singleton(
                    &scoped,
                    &call,
                    Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                )
                .await;
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

/// The record names of `steps`. A one-member round records its decision
/// in the Run's schedule record, named by its first event ordinal.
fn names(call_id: &ToolCallId, steps: &[&str]) -> Vec<String> {
    steps
        .iter()
        .map(|step| match *step {
            "decide" => "lash:run:schedule:1".to_owned(),
            step => format!("lash:run:{call_id}:{step}"),
        })
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
                    RunEvent::CutChecked { .. } => "cut_checked",
                    RunEvent::CutRetained { .. } => "cut_retained",
                    RunEvent::AdmissionRefused { .. } => "admission_refused",
                    RunEvent::IsolationRefused { .. } => "isolation_refused",
                    RunEvent::Admitted { .. } => "admitted",
                    RunEvent::AttemptRecorded { .. } => "attempt",
                    RunEvent::SourceCaptured { .. } => "source_captured",
                    RunEvent::CheckContributions { .. } => "check_contributions",
                    RunEvent::RetryScheduled { .. } => "retry",
                    RunEvent::RetryTimerRegistered { .. } => "retry_timer",
                    RunEvent::Decided { .. } => "decided",
                    RunEvent::DeclarationsIssued { .. } => "declarations_issued",
                    RunEvent::DeclarationsSettled { .. } => "declarations_settled",
                    RunEvent::StartAdmitted { .. } => "start_admitted",
                    RunEvent::StartLaunched { .. } => "start_launched",
                    RunEvent::StartDischarged { .. } => "start_discharged",
                    RunEvent::Presented { .. } => "presented",
                    RunEvent::Consumed { .. } => "consumed",
                    RunEvent::Incorporated { .. } => "incorporated",
                    RunEvent::Lifecycle { .. } => "lifecycle",
                    RunEvent::AggregateAdmitted { .. } => "aggregate",
                    RunEvent::TimerElapsed { .. } => "timer",
                    RunEvent::CancelDischarged { .. } => "cancel",
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

/// L02 and L15: a simple Done call is four journal records — A, X, D (its Run
/// record folds the recorded X) and V with its incorporation — and a lost
/// proposal at any of them, or a crash before the
/// handler's output, reruns only that record's step under the same call id
/// and attempt ordinal. A durable record never runs its step again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_done_singleton_is_four_records_and_reruns_only_unrecorded_work_at_every_cut() {
    let call = call("four-records");
    // V is the Run's schedule record: the three per-call records carry one
    // event each, so V is `lash:run:schedule:3`.
    let journaled = [
        names(&call.call_id, &["admit", "attempt:1", "decide"]),
        vec!["lash:run:schedule:3".to_owned()],
    ]
    .concat();
    for cut in [None, Some(0), Some(1), Some(2), Some(3), Some(4)] {
        let crash = match cut {
            None => Vec::new(),
            Some(4) => vec![CrashPoint::BeforeFrame {
                ty: MessageType::OutputCommand,
            }],
            Some(step) => vec![CrashPoint::BeforeRunResult {
                name: Some(journaled[step].clone()),
            }],
        };
        let probe = Probe::new(Probe::done(), CancelAt::Never);
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
                    stream: AttemptStream::default(),
                    start: None,
                    commands: Vec::new(),
                },
                presentation: PRESENTATION.to_owned(),
                launched: None,
            }
        );
        assert_eq!(
            events(&records),
            vec![
                vec!["admitted"],
                vec!["attempt", "decided"],
                vec!["presented", "consumed", "incorporated"],
            ]
        );
        let (found, .., calls) = driven.journal();
        assert_eq!(found, journaled, "four source records");
        assert_eq!(calls, 0, "no child invocation or group call");
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
        commands: Default::default(),
        output: OUTPUT.to_owned(),
        intents: vec![ToolIntentKind::EmitTrigger],
        start: None,
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
                // A withheld V is the schedule record at ordinal 3.
                [
                    names(&call.call_id, &["admit", "attempt:1", "decide"]),
                    vec!["lash:run:schedule:3".to_owned()]
                ]
                .concat()
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
                // The declaring call's V follows its declare record.
                [
                    names(&call.call_id, &["admit", "attempt:1", "decide", "declare"]),
                    vec!["lash:run:schedule:4".to_owned()]
                ]
                .concat()
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
    // The declaring call's V is the schedule record after its declare.
    let steps = [
        names(&call.call_id, &["admit", "attempt:1", "decide", "declare"]),
        vec!["lash:run:schedule:4".to_owned()],
    ]
    .concat();
    for cut in [2, 3, 4] {
        let probe = Probe::new(
            SingletonBodyOutcome::Done {
                commands: Default::default(),
                output: OUTPUT.to_owned(),
                intents: vec![ToolIntentKind::EmitTrigger],
                start: None,
            },
            CancelAt::Never,
        );
        let driven = drive(
            0x4877,
            vec![CrashPoint::BeforeRunResult {
                name: Some(steps[cut].clone()),
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
                vec!["attempt", "decided"],
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
            // The realization lives in the protected preparation: only a
            // dropped V record replays an attempt that already ran it.
            1 + usize::from(cut == 4),
            "cut {cut}: only an unsettled declaration boundary asks again"
        );
        assert_eq!(probe.realized.lock().unwrap().len(), 1, "one realization");
        assert_eq!(driven.journal().0, steps);
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
                commands: Default::default(),
                output: OUTPUT.to_owned(),
                intents: vec![ToolIntentKind::StartProcess],
                start: None,
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
        (
            "unarmed-source",
            deferred(&ToolCallId::fixture("unarmed-source")),
            DeclarationRefusal::UnarmedSource,
        ),
    ] {
        let mut call = call(label);
        call.declaration.may_defer = label == "unarmed-source";
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
                launched: None,
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

/// L04: a protected intent's own journal command is replayed even after V
/// became durable. Serving V must not bypass the nested command it issued.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l04_a_journaled_intent_replays_before_its_protected_presentation() {
    struct JournaledIntent<'a> {
        scoped: &'a lash_core::ScopedEffectController<'a>,
        probe: Arc<Probe>,
        mutations: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl SingletonToolHandlers for JournaledIntent<'_> {
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
        async fn execute(
            &self,
            attempt: SingletonAttempt<'_>,
        ) -> Result<SingletonBodyOutcome, String> {
            self.probe.execute(attempt).await
        }
        async fn after_checks(
            &self,
            call: &ToolCallId,
            capture: &SingletonCapture,
        ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
            self.probe.after_checks(call, capture).await
        }
        async fn run_cancel_requested(&self) -> Result<bool, String> {
            Ok(false)
        }

        async fn realize_declarations(
            &self,
            call: &ToolCallId,
            intents: &[ToolIntentKind],
        ) -> Result<(), String> {
            self.scoped
                .controller()
                .record_run_record(
                    "l04:external-intent".to_owned(),
                    Box::pin(async {
                        self.mutations.fetch_add(1, Ordering::SeqCst);
                        Ok(lash_core::tool_run::RunJournalEntry {
                            state: Vec::new(),
                            materials: Vec::new(),
                            record: RunRecord {
                                segment: SegmentOrdinal(0),
                                first: lash_core::tool_run::RunEventOrdinal(0),
                                events: Vec::new(),
                                trace: None,
                            },
                        })
                    }),
                )
                .await
                .map_err(|error| error.to_string())?;
            self.probe.realize_declarations(call, intents).await
        }
        async fn present(
            &self,
            call: &ToolCallId,
            capture: &SingletonCapture,
        ) -> Result<String, lash_core::tool_dispatch::SingletonPresentationError> {
            self.probe.present(call, capture).await
        }
        fn emit_stream(&self, _: &ToolCallId, _: &AttemptStream) {}
        async fn launch_start(
            &self,
            _: &DeclaredStartObligation,
        ) -> Result<lash_core::ProcessId, String> {
            Err("the witness declares no start".into())
        }
        async fn discharge_start(
            &self,
            _: &DeclaredStartObligation,
            _: &lash_core::ProcessId,
            _: bool,
        ) -> Result<(), String> {
            Err("the witness declares no start".into())
        }
    }
    let backend = lash_restate_test::backend(0x493309, ServerConfig::default())
        .await
        .unwrap();
    // V is accepted; a lost handler output then replays the entire journal,
    // including the intent command V issued before it finished.
    backend
        .server()
        .crash_on(CrashRule::new(CrashPoint::BeforeFrame {
            ty: MessageType::OutputCommand,
        }));
    let mut call = call("journaled-intent");
    call.declaration = ToolDeclaration::default().with_intents([ToolIntentKind::EmitProcessEvent]);
    let probe = Probe::new(
        SingletonBodyOutcome::Done {
            commands: Default::default(),
            output: OUTPUT.to_owned(),
            intents: vec![ToolIntentKind::EmitProcessEvent],
            start: None,
        },
        CancelAt::Never,
    );
    let mutations = Arc::new(AtomicUsize::new(0));
    let returned: Returned = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let mutations = mutations.clone();
        let returned = returned.clone();
        Arc::new(move |scoped| {
            let call = call.clone();
            let probe = probe.clone();
            let mutations = mutations.clone();
            let returned = returned.clone();
            Box::pin(async move {
                let handlers = JournaledIntent {
                    scoped: &scoped,
                    probe,
                    mutations,
                };
                let result = run_singleton(&scoped, &call, Arc::new(handlers)).await;
                returned.lock().unwrap().push(result);
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .expect("protected replay settles")
    .expect("handler replay succeeds");
    let result = returned
        .lock()
        .unwrap()
        .pop()
        .expect("the replay returned")
        .expect("the replay completed");
    assert!(matches!(result.terminal, SingletonTerminal::Final { .. }));
    assert_eq!(mutations.load(Ordering::SeqCst), 1, "one external mutation");
}
