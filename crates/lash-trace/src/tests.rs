//! Unit tests for the trace record and event carriers.
//!
//! These live in their own file so `lib.rs` carries only the durable schema
//! types it defines.

use super::*;
/// V23: an intent completion must name its closed, typed intent kind.
#[test]
fn domain_completion_refuses_an_untyped_intent_subject() {
    let completion = serde_json::json!({
        "operation": "tool_intent", "started_at_ms": 0, "status": "completed"
    });
    assert!(serde_json::from_value::<TraceDomainCompletion>(completion).is_err());
    for subject in [
        serde_json::json!({"type": "tool_intent"}),
        serde_json::json!({"type": "tool_intent", "kind": "custom_intent"}),
    ] {
        assert!(
            serde_json::from_value::<TraceDomainCompletion>(serde_json::json!({
                "subject": subject, "started_at_ms": 0, "status": "completed"
            }))
            .is_err()
        );
    }
}

/// A sink that fails every call, standing in for a closed stderr.
struct FailingSink;

impl TraceSink for FailingSink {
    fn append(&self, _record: &TraceRecord) -> Result<(), TraceSinkError> {
        Err(TraceSinkError::Write {
            path: PathBuf::from("<failing>"),
            source: io::Error::from(io::ErrorKind::BrokenPipe),
        })
    }

    fn flush(&self) -> Result<(), TraceSinkError> {
        Err(TraceSinkError::Write {
            path: PathBuf::from("<failing>"),
            source: io::Error::from(io::ErrorKind::BrokenPipe),
        })
    }
}

#[test]
fn a_failing_sink_does_not_rob_later_sinks_in_a_tee() {
    // A host tees stderr first and its durable trace file second, so a
    // supervisor closing stderr must not cost the run its trace file.
    let recording = Arc::new(RecordingTraceSink::new());
    let tee = TeeTraceSink::new([
        Arc::new(FailingSink) as Arc<dyn TraceSink>,
        Arc::clone(&recording) as Arc<dyn TraceSink>,
    ]);

    let append = tee.append(&fixture_record(
        TraceContext::default().for_session("root"),
        TraceEvent::CompactionCompleted { summary_nodes: 1 },
    ));

    assert!(
        append.is_err(),
        "the first sink's failure is still reported"
    );
    assert!(tee.flush().is_err(), "a failing flush is reported too");
    assert!(
        matches!(
            recording.records().as_slice(),
            [TraceRecord {
                event: TraceEvent::CompactionCompleted { summary_nodes: 1 },
                ..
            }]
        ),
        "the later sink still received the record"
    );
}

#[test]
fn event_is_failed_identifies_all_failure_outcomes() {
    fn tool_completed(outcome: TraceToolCallOutcome) -> TraceEvent {
        TraceEvent::ToolCallCompleted {
            call_id: lash_protocol_types::ToolCallId::fixture("none"),
            provider_call_id: None,
            name: "tool".to_string().into(),
            args: Value::Null.into(),
            output: TraceToolCallOutput {
                outcome,
                control: None,
            },
            duration_ms: 1,
            issuing_node_id: None,
            attempts: None,
            span_attributes: Default::default(),
        }
    }

    fn language_execution(payload: TraceLanguageExecutionPayload) -> TraceEvent {
        TraceEvent::LanguageExecution {
            language: Some("lashvm".to_string()),
            event: TraceLanguageExecution {
                event_key: "event-key".to_string(),
                identity: TraceLanguageExecutionIdentity {
                    scope: TraceRuntimeScope::new("s1"),
                    document: crate::WorkflowDocumentRef {
                        document: lash_kernel_doc::DocumentId::from_bytes([1; 32]),
                        entry: crate::WorkflowDocumentEntry::Main,
                    },
                    entry_name: "main".to_string(),
                    engine_execution_id: None,
                    generation: None,
                },
                payload,
            },
        }
    }

    let failures = [
        (
            "llm call failed",
            TraceEvent::LlmCallFailed {
                model: None,
                request: Default::default(),
                error: TraceError {
                    retryable: false,
                    terminal_reason: crate::TraceLlmTerminalReason::Unknown,
                    failure_kind: crate::TraceProviderFailureKind::Unknown,
                    code: None,
                },
                stream_summary: None,
                attempts: None,
            },
        ),
        (
            "effect envelope diff",
            TraceEvent::EffectEnvelopeDiff {
                event: TraceEffectEnvelopeDiffEvent {
                    recorded_envelope_hash: "recorded".to_string(),
                    reconstructed_envelope_hash: "reconstructed".to_string(),
                    divergent_paths: Vec::new(),
                },
            },
        ),
        (
            "store error observed",
            TraceEvent::StoreErrorObserved {
                operation: "load".to_string(),
                error_class: crate::TraceStoreErrorClass::StoredDataCorrupt,
                message: "failed".to_string(),
            },
        ),
        (
            "tool call failed",
            tool_completed(TraceToolCallOutcome::Failure(Value::Null)),
        ),
        (
            "language node failed",
            language_execution(TraceLanguageExecutionPayload::Node {
                subject: TraceRuntimeSubject::Process {
                    process_id: lash_protocol_types::ProcessId::fixture("p1"),
                },
                at: lash_protocol_types::effect_identity_fixture("n1", 1),
                fact: TraceNodeFact::Failed {
                    call_id: None,
                    failure: TraceLanguageExecutionFailure::Runtime {
                        code: "test_failure".to_string(),
                        message: "failed".to_string(),
                    },
                },
            }),
        ),
        (
            "language execution failed",
            language_execution(TraceLanguageExecutionPayload::ExecutionFinished {
                subject: TraceRuntimeSubject::Process {
                    process_id: lash_protocol_types::ProcessId::fixture("p1"),
                },
                status: TraceLanguageExecutionStatus::Failed,
                error: Some("failed".to_string()),
            }),
        ),
    ];
    for (case, event) in failures {
        assert!(event.is_failed(), "{case} must be classified as failed");
    }

    for done_reason in [
        TraceTurnFailureReason::Incomplete,
        TraceTurnFailureReason::InvalidInput,
        TraceTurnFailureReason::MaxTurns,
        TraceTurnFailureReason::ToolFailure,
        TraceTurnFailureReason::ProviderError,
        TraceTurnFailureReason::ContextOverflow,
        TraceTurnFailureReason::PluginAbort,
        TraceTurnFailureReason::RuntimeError,
        TraceTurnFailureReason::AgentFrameSwitchLimit,
        TraceTurnFailureReason::SubmittedError,
        TraceTurnFailureReason::ToolError,
    ] {
        let case = done_reason.wire_tag();
        let event = TraceEvent::TurnCompleted {
            output: Vec::new(),
            input: Vec::new(),
            inference_calls: None,
            tool_calls: None,
            inputs: 1,
            outcome: TraceTurnOutcome::Failed { done_reason },
        };
        assert!(event.is_failed(), "failed turn reason {case} was missed");
    }

    let non_failures = [
        (
            "completed turn",
            TraceEvent::TurnCompleted {
                output: Vec::new(),
                input: Vec::new(),
                inference_calls: None,
                tool_calls: None,
                inputs: 1,
                outcome: TraceTurnOutcome::Completed {
                    done_reason: TraceTurnCompletionReason::AssistantMessage,
                },
            },
        ),
        (
            "cancelled turn",
            TraceEvent::TurnCompleted {
                output: Vec::new(),
                input: Vec::new(),
                inference_calls: None,
                tool_calls: None,
                inputs: 1,
                outcome: TraceTurnOutcome::Cancelled {
                    evidence: TraceTurnCancellationEvidence {
                        request_id: "request-1".to_string(),
                        origin: None,
                        reason: None,
                    },
                },
            },
        ),
        (
            "successful tool call",
            tool_completed(TraceToolCallOutcome::Success(Value::Null)),
        ),
        (
            "cancelled tool call",
            tool_completed(TraceToolCallOutcome::Cancelled(Value::Null)),
        ),
        (
            "started language node",
            language_execution(TraceLanguageExecutionPayload::Node {
                subject: TraceRuntimeSubject::Process {
                    process_id: lash_protocol_types::ProcessId::fixture("p1"),
                },
                at: lash_protocol_types::effect_identity_fixture("n1", 1),
                fact: TraceNodeFact::Started { call_id: None },
            }),
        ),
        (
            "completed language execution",
            language_execution(TraceLanguageExecutionPayload::ExecutionFinished {
                subject: TraceRuntimeSubject::Process {
                    process_id: lash_protocol_types::ProcessId::fixture("p1"),
                },
                status: TraceLanguageExecutionStatus::Completed,
                error: None,
            }),
        ),
    ];
    for (case, event) in non_failures {
        assert!(
            !event.is_failed(),
            "{case} must not be classified as failed"
        );
    }
}

/// A frame's guard restores the frame entered before it whatever the host
/// does: when the host's scope panics on entry, when the host's guard panics
/// on leaving, and on an ordinary nested exit, the prior frame (or no frame)
/// is current again after the one unwind is caught.
#[test]
fn a_panicking_host_scope_leaves_the_prior_frame_installed() {
    struct PanicsOnLeave;
    impl Drop for PanicsOnLeave {
        fn drop(&mut self) {
            panic!("host guard panics on leave");
        }
    }
    struct Host {
        panics_on_enter: bool,
    }
    impl AmbientScope for Host {
        fn enter(&self) -> AmbientGuard {
            if self.panics_on_enter {
                panic!("host scope panics on enter");
            }
            AmbientGuard::new(PanicsOnLeave)
        }
    }
    fn frame(scope: Option<Host>) -> std::sync::Arc<AmbientFrame> {
        AmbientFrame::new(
            scope.map(|scope| std::sync::Arc::new(scope) as std::sync::Arc<dyn AmbientScope>),
            std::sync::Arc::new(()),
        )
    }
    fn is_current(expected: Option<&std::sync::Arc<AmbientFrame>>) -> bool {
        match (AmbientFrame::current(), expected) {
            (None, None) => true,
            (Some(current), Some(expected)) => std::sync::Arc::ptr_eq(&current, expected),
            _ => false,
        }
    }
    std::thread::spawn(|| {
        let outer = frame(None);
        for prior in [None, Some(&outer)] {
            let _prior = prior.map(|prior| prior.enter());
            assert!(is_current(prior));
            for panics_on_enter in [true, false] {
                let failing = frame(Some(Host { panics_on_enter }));
                let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _entered = failing.enter();
                }));
                assert!(unwound.is_err(), "the host panics once");
                assert!(
                    is_current(prior),
                    "the prior frame is current after a host panic on {}",
                    if panics_on_enter { "enter" } else { "leave" }
                );
            }
            let inner = frame(None);
            {
                let _inner = inner.enter();
                assert!(is_current(Some(&inner)));
            }
            assert!(is_current(prior), "a nested exit restores the prior frame");
        }
        assert!(is_current(None));
    })
    .join()
    .expect("the law's thread");
}

#[cfg(test)]
fn fixture_record(context: crate::TraceContext, event: crate::TraceEvent) -> crate::TraceRecord {
    crate::TraceRecord {
        schema_version: crate::TRACE_SCHEMA_VERSION,
        id: "fixture-record".into(),
        timestamp: crate::TraceTimestamp::from_unix_ms(0),
        content: crate::ContentPolicy::captured().into(),
        context,
        position: None,
        omissions: Default::default(),
        event,
    }
}
