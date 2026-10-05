//! Codec, refusal and transition witnesses of the tool-run contract.

use lash_core_ids::BehaviorRevision;
use lash_sansio::{ToolCallId, ToolIntentKind};
use serde_json::json;

use super::*;
use crate::artifact_referrer::{ArtifactName, ArtifactStoreId};
use crate::await_event_identity::{AwaitEventKey, AwaitEventWaitIdentity};
use crate::effect_opener::EffectOpener;
use crate::process_identity::StartKey;
use crate::store::plugin_writers::{PluginCallbackIdentity, PluginRevision};
use crate::{ExecutionScope, ProcessId};

const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn opener() -> EffectOpener {
    EffectOpener::turn("session-1", "turn-1")
}

fn revision(plugin: &str) -> PluginRevision {
    PluginRevision::new(plugin, BehaviorRevision::ONE)
}

fn callback(plugin: &str, key: &str) -> PluginCallbackIdentity {
    PluginCallbackIdentity {
        owner: revision(plugin),
        key: key.into(),
    }
}

fn digest(text: &str) -> MaterialDigest {
    MaterialDigest::parse(text).unwrap()
}

fn run_material(role: MaterialRole) -> MaterialRef {
    MaterialRef {
        owner: MaterialOwner::Run { opener: opener() },
        role,
        location: MaterialLocation::JournalLocal,
        digest: digest(DIGEST_A),
    }
}

fn source_key(call_id: &ToolCallId) -> AwaitEventKey {
    AwaitEventKey {
        scope: ExecutionScope::turn("session-1", "turn-1"),
        wait: AwaitEventWaitIdentity::tool_completion(call_id.clone()),
        key_id: "completion-1".into(),
        signature: "signature-1".into(),
    }
}

fn binding() -> AdmittedBinding {
    AdmittedBinding {
        executable: callback("tools", "tool_provider:0"),
        preparation: callback("tools", "tool_provider:0"),
        presentation: PresentationBinding {
            presenter: Some(callback("standard", "presentation_presenter")),
            steps: vec![callback("render", "presentation_step:0")],
        },
    }
}

fn available() -> Vec<PluginRevision> {
    vec![revision("tools"), revision("standard"), revision("render")]
}

fn call(label: &str) -> AdmittedCall {
    AdmittedCall {
        call_id: ToolCallId::fixture(label),
        tool_name: "read_file".into(),
        request: run_material(MaterialRole::PreparedRequest),
        declaration: ToolDeclaration::default(),
        binding: binding(),
        policy: RuntimeCallPolicy::default(),
        checks: CheckRecord::reduce(Vec::new()),
    }
}

fn round(members: Vec<AdmittedCall>) -> RoundAdmission {
    let operands = (0..u32::try_from(members.len()).unwrap()).collect();
    RoundAdmission {
        owner: opener(),
        members,
        operands,
        capacity: CapacityScope::Held,
    }
}

fn cause(error_type: &str) -> HookCause {
    HookCause {
        error_type: error_type.into(),
        error_version: std::num::NonZeroU32::MIN,
        payload: json!({"why": error_type}),
    }
}

fn allow_all() -> Option<CheckRecord<AfterCheckVerdict>> {
    Some(CheckRecord::reduce(vec![AttributedVerdict {
        callback: callback("policy", "tool_result_check:first"),
        verdict: AfterCheckVerdict::Allow,
    }]))
}

fn attempt(n: u32) -> AttemptOrdinal {
    AttemptOrdinal::new(n).unwrap()
}

/// Appends single-event records from segment 0.
struct Log {
    ledger: RunLedger,
}

impl Log {
    fn new() -> Self {
        Self {
            ledger: RunLedger::new(opener()),
        }
    }

    fn push(&mut self, event: RunEvent) -> Result<(), RunEventRefusal> {
        let record = RunRecord {
            trace: None,
            segment: SegmentOrdinal(0),
            first: self.ledger.next_ordinal(),
            events: vec![event],
        };
        self.ledger.append(SegmentOrdinal(0), &record)
    }
}

fn decided(call_id: &ToolCallId, decision: CallDecision) -> RunEvent {
    RunEvent::Decided {
        call_id: call_id.clone(),
        decision,
        after: allow_all(),
    }
}

fn final_of(n: u32, declares: bool) -> CallDecision {
    CallDecision::Final {
        source: ResultSource::Attempt {
            attempt: attempt(n),
        },
        declares,
    }
}

fn done(call_id: &ToolCallId, n: u32) -> RunEvent {
    RunEvent::AttemptRecorded {
        call_id: call_id.clone(),
        attempt: attempt(n),
        result: AttemptResult::Done {
            output: run_material(MaterialRole::AttemptOutput),
        },
    }
}

fn failed(call_id: &ToolCallId, n: u32, retryable: bool) -> RunEvent {
    RunEvent::AttemptRecorded {
        call_id: call_id.clone(),
        attempt: attempt(n),
        result: AttemptResult::Failed {
            output: run_material(MaterialRole::AttemptOutput),
            retryable,
        },
    }
}

#[test]
fn the_singleton_route_is_four_records_and_its_codec_is_pinned() {
    let call = call("singleton");
    let id = call.call_id.clone();
    let admitted = round(vec![call]);
    let records = vec![
        vec![RunEvent::Admitted {
            round: admitted.clone(),
        }],
        vec![done(&id, 1)],
        vec![decided(&id, final_of(1, false))],
        vec![
            RunEvent::Presented {
                call_id: id.clone(),
                presentation: None,
                failure: None,
            },
            RunEvent::Incorporated {
                call_id: id.clone(),
            },
        ],
    ];
    let mut ledger = RunLedger::new(opener());
    for events in &records {
        let record = RunRecord {
            trace: None,
            segment: SegmentOrdinal(0),
            first: ledger.next_ordinal(),
            events: events.clone(),
        };
        ledger.append(SegmentOrdinal(0), &record).unwrap();
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(
            serde_json::from_value::<RunRecord>(encoded).unwrap(),
            record
        );
    }
    assert_eq!(ledger.next_ordinal(), RunEventOrdinal(5));
    ledger
        .append(
            SegmentOrdinal(0),
            &RunRecord {
                trace: None,
                segment: SegmentOrdinal(0),
                first: RunEventOrdinal(5),
                events: vec![RunEvent::Lifecycle {
                    state: RunLifecycle::Settled,
                }],
            },
        )
        .unwrap();

    let material = json!({
        "owner": {"owner": "run", "opener": {"kind": "turn", "session_id": "session-1", "turn_id": "turn-1"}},
        "role": "attempt_output",
        "location": {"location": "journal_local"},
        "digest": DIGEST_A,
    });
    assert_eq!(
        serde_json::to_value(done(&id, 1)).unwrap(),
        json!({
            "event": "attempt_recorded",
            "call_id": id.as_str(),
            "attempt": 1,
            "result": {"result": "done", "output": material},
        })
    );
    assert_eq!(
        serde_json::to_value(decided(&id, final_of(1, false))).unwrap(),
        json!({
            "event": "decided",
            "call_id": id.as_str(),
            "decision": {"decision": "final", "source": {"source": "attempt", "attempt": 1}, "declares": false},
            "after": [{"callback": {"owner": {"plugin": "policy", "behavior_revision": 1}, "key": "tool_result_check:first"}, "verdict": {"verdict": "allow"}}],
        })
    );
    assert_eq!(
        serde_json::to_value(&admitted.members[0].declaration).unwrap(),
        json!({"may_defer": false, "intents": [], "isolated": false})
    );
}

#[test]
fn a_declaration_has_exactly_three_capabilities_and_no_duration() {
    for field in [
        "timeout_ms",
        "deadline",
        "duration_ms",
        "idempotent",
        "budget",
    ] {
        let mut value = json!({"may_defer": false, "intents": [], "isolated": false});
        value[field] = json!(1);
        assert!(
            serde_json::from_value::<ToolDeclaration>(value).is_err(),
            "`{field}` must not decode"
        );
    }
    assert!(
        serde_json::from_value::<RecordedRetryPolicy>(
            json!({"retry": "idempotent", "max_attempts": 2, "base_delay_ms": 1, "max_delay_ms": 2})
        )
        .is_err()
    );
}

#[test]
fn declarations_refuse_typed() {
    let duplicate = ToolDeclaration {
        intents: vec![ToolIntentKind::StartProcess, ToolIntentKind::StartProcess],
        ..ToolDeclaration::default()
    };
    assert_eq!(
        duplicate.validate(),
        Err(DeclarationRefusal::DuplicateIntent {
            kind: ToolIntentKind::StartProcess
        })
    );
    let reordered = ToolDeclaration {
        intents: vec![ToolIntentKind::CancelProcess, ToolIntentKind::StartProcess],
        ..ToolDeclaration::default()
    };
    assert_eq!(reordered.validate(), Err(DeclarationRefusal::IntentOrder));
    let isolated = ToolDeclaration {
        isolated: true,
        may_defer: true,
        ..ToolDeclaration::default()
    };
    assert_eq!(
        isolated.validate(),
        Err(DeclarationRefusal::IsolatedInlineCapability)
    );

    let plain = ToolDeclaration::default();
    assert_eq!(
        plain.admits(OutcomeShape::Deferred),
        Err(DeclarationRefusal::UndeclaredDeferral)
    );
    assert_eq!(
        plain.admits(OutcomeShape::Done {
            intents: &[ToolIntentKind::EmitTrigger]
        }),
        Err(DeclarationRefusal::UndeclaredIntent {
            kind: ToolIntentKind::EmitTrigger
        })
    );
    let deferring = ToolDeclaration {
        may_defer: true,
        intents: vec![ToolIntentKind::EmitTrigger],
        isolated: false,
    };
    assert_eq!(deferring.admits(OutcomeShape::Deferred), Ok(()));
    assert_eq!(
        deferring.admits(OutcomeShape::Done {
            intents: &[ToolIntentKind::EmitTrigger]
        }),
        Ok(())
    );
    let isolated = ToolDeclaration {
        isolated: true,
        ..ToolDeclaration::default()
    };
    assert_eq!(
        isolated.admits(OutcomeShape::Done { intents: &[] }),
        Err(DeclarationRefusal::InlineOutcomeFromIsolated)
    );
}

#[test]
fn one_invalid_member_admits_no_member() {
    let ok = round(vec![call("a"), call("b")]);
    ok.clone().admit(&available(), |_| false).unwrap();

    let mut duplicate = round(vec![call("a"), call("a")]);
    duplicate.operands = vec![0, 1];
    assert_eq!(
        duplicate.admit(&available(), |_| false),
        Err(AdmissionRefusal::DuplicateCall {
            call_id: ToolCallId::fixture("a")
        })
    );

    let mut unreferenced = round(vec![call("a"), call("b")]);
    unreferenced.operands = vec![0];
    assert_eq!(
        unreferenced.admit(&available(), |_| false),
        Err(AdmissionRefusal::UnreferencedMember { member: 1 })
    );

    let mut out_of_range = round(vec![call("a")]);
    out_of_range.operands = vec![0, 3];
    assert_eq!(
        out_of_range.admit(&available(), |_| false),
        Err(AdmissionRefusal::OperandOutOfRange { slot: 1, member: 3 })
    );

    let mut foreign = call("b");
    foreign.request.owner = MaterialOwner::Run {
        opener: EffectOpener::turn("session-2", "turn-1"),
    };
    assert_eq!(
        round(vec![call("a"), foreign]).admit(&available(), |_| false),
        Err(AdmissionRefusal::RequestNotOwned { member: 1 })
    );

    let mut isolated = call("b");
    isolated.declaration.isolated = true;
    assert_eq!(
        round(vec![call("a"), isolated.clone()]).admit(&available(), |_| false),
        Err(AdmissionRefusal::UnsupportedIsolation { member: 1 })
    );
    assert!(
        round(vec![call("a"), isolated])
            .admit(&available(), |tool| tool == "read_file")
            .is_ok()
    );

    let mut cached = call("b");
    cached.checks = CheckRecord::reduce(vec![AttributedVerdict {
        callback: callback("cache", "tool_args_check:first"),
        verdict: BeforeCheckVerdict::Cached {
            result: run_material(MaterialRole::Presentation),
        },
    }]);
    assert_eq!(
        round(vec![call("a"), cached]).admit(&available(), |_| false),
        Err(AdmissionRefusal::CachedNotOwned { member: 1 })
    );
}

#[test]
fn an_unavailable_bound_revision_refuses_with_the_fig_4854_refusal() {
    let newer = vec![
        revision("tools"),
        PluginRevision::new("standard", BehaviorRevision::new(2).unwrap()),
        revision("render"),
    ];
    let refusal = round(vec![call("a")]).admit(&newer, |_| false).unwrap_err();
    let AdmissionRefusal::BindingUnavailable { member, cause } = refusal else {
        panic!("expected a binding refusal, got {refusal:?}");
    };
    assert_eq!(member, 0);
    assert_eq!(cause.recorded, vec![revision("standard")]);
    assert_eq!(cause.available, newer);
    assert_eq!(
        cause.callback,
        Some(callback("standard", "presentation_presenter"))
    );
    assert_eq!(
        cause.into_runtime_error().code,
        crate::RuntimeErrorCode::PluginRevisionUnavailable
    );
}

fn before(
    plugin: &str,
    key: &str,
    verdict: BeforeCheckVerdict,
) -> AttributedVerdict<BeforeCheckVerdict> {
    AttributedVerdict {
        callback: callback(plugin, key),
        verdict,
    }
}

#[test]
fn checks_reduce_by_strength_then_plugin_then_key_in_every_order() {
    let replies = vec![
        before(
            "a-cache",
            "tool_args_check:first",
            BeforeCheckVerdict::Cached {
                result: run_material(MaterialRole::AttemptOutput),
            },
        ),
        before(
            "b-policy",
            "tool_args_check:second",
            BeforeCheckVerdict::Deny {
                cause: cause("deny-1"),
            },
        ),
        before(
            "b-policy",
            "tool_args_check:first",
            BeforeCheckVerdict::Cancel {
                cause: cause("cancel"),
            },
        ),
        before(
            "z-guard",
            "tool_args_check:first",
            BeforeCheckVerdict::AbortRun {
                cause: cause("abort"),
            },
        ),
        before(
            "a-allow",
            "tool_args_check:first",
            BeforeCheckVerdict::Allow,
        ),
    ];
    let expected = CheckRecord::reduce(replies.clone());
    let order: Vec<_> = expected
        .replies()
        .iter()
        .map(|reply| {
            (
                reply.callback.owner.plugin.as_str(),
                reply.callback.key.as_str(),
            )
        })
        .collect();
    assert_eq!(
        order,
        vec![
            ("z-guard", "tool_args_check:first"),
            ("b-policy", "tool_args_check:first"),
            ("b-policy", "tool_args_check:second"),
            ("a-cache", "tool_args_check:first"),
            ("a-allow", "tool_args_check:first"),
        ]
    );
    assert_eq!(expected.selection(), BeforeSelection::AbortRun);
    for rotation in 0..replies.len() {
        let mut permuted = replies.clone();
        permuted.rotate_left(rotation);
        permuted.reverse();
        assert_eq!(CheckRecord::reduce(permuted), expected);
    }
    assert!(expected.is_reduced());

    let cache_wins = CheckRecord::reduce(vec![replies[4].clone(), replies[0].clone()]);
    assert_eq!(cache_wins.selection(), BeforeSelection::Cached);
    assert_eq!(
        CheckRecord::<BeforeCheckVerdict>::reduce(Vec::new()).selection(),
        BeforeSelection::Execute
    );
    let unreduced: CheckRecord<BeforeCheckVerdict> = serde_json::from_value(
        serde_json::to_value(vec![replies[4].clone(), replies[3].clone()]).unwrap(),
    )
    .unwrap();
    assert!(!unreduced.is_reduced());
    let mut disordered = call("a");
    disordered.checks = unreduced;
    assert_eq!(
        round(vec![disordered]).admit(&available(), |_| false),
        Err(AdmissionRefusal::UnreducedChecks { member: 0 })
    );
}

#[test]
fn an_after_check_cannot_replace_a_result() {
    for verdict in [
        json!({"verdict": "cached", "result": serde_json::to_value(run_material(MaterialRole::AttemptOutput)).unwrap()}),
        json!({"verdict": "replace", "result": {}}),
    ] {
        assert!(serde_json::from_value::<AfterCheckVerdict>(verdict).is_err());
    }
    assert_eq!(
        serde_json::to_value(AfterCheckVerdict::AbortRun {
            cause: cause("stop")
        })
        .unwrap(),
        json!({"verdict": "abort_run", "cause": {"error_type": "stop", "error_version": 1, "payload": {"why": "stop"}}})
    );
    assert!(ToolHookPhase::ResultCheck.may_propose_state_commands());
    for phase in [
        ToolHookPhase::ArgsTransform,
        ToolHookPhase::ArgsCheck,
        ToolHookPhase::ResultTransform,
    ] {
        assert!(!phase.may_propose_state_commands());
    }
    assert!(ToolHookOccurrence::Admission.admits(ToolHookPhase::ArgsCheck));
    assert!(!ToolHookOccurrence::Admission.admits(ToolHookPhase::ResultCheck));
    assert!(ToolHookOccurrence::Cached.admits(ToolHookPhase::ResultTransform));
    assert!(
        !ToolHookOccurrence::Attempt {
            attempt: attempt(2)
        }
        .admits(ToolHookPhase::ArgsCheck)
    );
    let occurrence = HookOccurrence {
        call_id: ToolCallId::fixture("a"),
        callback: callback("policy", "tool_result_check:first"),
        phase: ToolHookPhase::ResultCheck,
        occurrence: ToolHookOccurrence::DeferredCompletion {
            attempt: attempt(1),
        },
    };
    let encoded = serde_json::to_value(&occurrence).unwrap();
    assert_eq!(
        encoded["occurrence"],
        json!({"occurrence": "deferred_completion", "attempt": 1})
    );
    assert_eq!(
        serde_json::from_value::<HookOccurrence>(encoded).unwrap(),
        occurrence
    );
}

#[test]
fn the_ledger_refuses_gaps_and_appends_from_other_segments() {
    let mut ledger = RunLedger::new(opener());
    let admitted = RunEvent::Admitted {
        round: round(vec![call("a")]),
    };
    let record = |segment: u32, first: u64| RunRecord {
        trace: None,
        segment: SegmentOrdinal(segment),
        first: RunEventOrdinal(first),
        events: vec![admitted.clone()],
    };
    assert_eq!(
        ledger.append(SegmentOrdinal(0), &record(0, 1)),
        Err(RunEventRefusal::OrdinalGap {
            expected: 0,
            found: 1
        })
    );
    assert_eq!(
        ledger.append(SegmentOrdinal(1), &record(0, 0)),
        Err(RunEventRefusal::NotActiveSegment {
            active: 1,
            found: 0
        })
    );
    ledger.append(SegmentOrdinal(1), &record(1, 0)).unwrap();
    assert_eq!(
        ledger.append(SegmentOrdinal(0), &record(0, 1)),
        Err(RunEventRefusal::StaleSegment {
            latest: 1,
            found: 0
        })
    );
    assert_eq!(
        ledger.append(
            SegmentOrdinal(1),
            &RunRecord {
                trace: None,
                segment: SegmentOrdinal(1),
                first: RunEventOrdinal(1),
                events: Vec::new(),
            }
        ),
        Err(RunEventRefusal::EmptyRecord)
    );
    let foreign = RunEvent::Admitted {
        round: RoundAdmission {
            owner: EffectOpener::turn("session-2", "turn-1"),
            ..round(vec![call("b")])
        },
    };
    let before = ledger.next_ordinal();
    assert_eq!(
        ledger.append(
            SegmentOrdinal(1),
            &RunRecord {
                trace: None,
                segment: SegmentOrdinal(1),
                first: before,
                events: vec![done(&ToolCallId::fixture("a"), 1), foreign],
            }
        ),
        Err(RunEventRefusal::ForeignOwner)
    );
    assert_eq!(
        ledger.next_ordinal(),
        before,
        "a refused record applies nothing"
    );
}

#[test]
fn l05_check_cancellation_cannot_be_recorded_as_run_control() {
    let mut log = Log::new();
    let call_id = ToolCallId::fixture("check-cancel");
    let mut member = call("check-cancel");
    member.policy.cancel = ExternalCancelPolicy::CancelExternalWork;
    log.push(RunEvent::Admitted {
        round: round(vec![member]),
    })
    .unwrap();
    log.push(done(&call_id, 1)).unwrap();
    let after = Some(CheckRecord::reduce(vec![AttributedVerdict {
        callback: callback("guard", "tool_result_check:cancel"),
        verdict: AfterCheckVerdict::Cancel {
            cause: cause("cancel only this call"),
        },
    }]));
    assert_eq!(
        log.push(RunEvent::Decided {
            call_id: call_id.clone(),
            decision: CallDecision::Cancelled,
            after: after.clone(),
        }),
        Err(RunEventRefusal::DecisionUnsupported {
            call_id: call_id.clone()
        }),
        "L05: recorded check evidence cannot authorize Run cancellation"
    );
    assert_eq!(
        log.push(RunEvent::Decided {
            call_id: call_id.clone(),
            decision: CallDecision::CheckCancelled,
            after: allow_all(),
        }),
        Err(RunEventRefusal::DecisionUnsupported {
            call_id: call_id.clone()
        }),
        "L05: check cancellation needs the winning cancellation verdict"
    );
    let decided = RunEvent::Decided {
        call_id: call_id.clone(),
        decision: CallDecision::CheckCancelled,
        after,
    };
    log.push(decided.clone()).unwrap();
    assert_eq!(
        BusinessReceipt::for_event(&decided),
        vec![BusinessReceipt::Terminal {
            call_id: call_id.clone(),
            terminal: LogicalTerminal::Cancelled,
        }]
    );
    assert!(
        log.ledger.eligible_cancellations().is_empty(),
        "a check-cancelled completed body owes no external cancellation"
    );
    log.push(RunEvent::Lifecycle {
        state: RunLifecycle::Closing,
    })
    .unwrap();
    log.push(RunEvent::Presented {
        call_id: call_id.clone(),
        presentation: None,
        failure: None,
    })
    .unwrap();
    log.push(RunEvent::Incorporated { call_id }).unwrap();
    log.push(RunEvent::Lifecycle {
        state: RunLifecycle::Settled,
    })
    .unwrap();
}

#[test]
fn k3_decision_ranks_are_dense_in_fold_order() {
    let mut ledger = RunLedger::new(opener());
    let (a, b, c) = (
        ToolCallId::fixture("a"),
        ToolCallId::fixture("b"),
        ToolCallId::fixture("c"),
    );
    let admission = RunRecord {
        trace: None,
        segment: SegmentOrdinal(0),
        first: RunEventOrdinal(0),
        events: vec![
            RunEvent::Admitted {
                round: round(vec![call("a"), call("b"), call("c")]),
            },
            done(&b, 1),
        ],
    };
    ledger.append(SegmentOrdinal(0), &admission).unwrap();
    assert_eq!(ledger.decision_rank(&b), None, "an attempt takes no rank");
    let first = RunRecord {
        events: vec![decided(&b, final_of(1, false))],
        first: ledger.next_ordinal(),
        ..admission.clone()
    };
    ledger.append(SegmentOrdinal(0), &first).unwrap();
    assert_eq!(
        ledger.decision_rank(&b),
        Some(1),
        "K3: the first decision has rank 1"
    );
    assert_eq!(ledger.decision_rank(&a), None);
    let refused = RunRecord {
        events: vec![
            RunEvent::Decided {
                call_id: a.clone(),
                decision: CallDecision::Cancelled,
                after: None,
            },
            decided(&c, final_of(1, false)),
        ],
        first: ledger.next_ordinal(),
        ..admission.clone()
    };
    assert_eq!(
        ledger.append(SegmentOrdinal(0), &refused),
        Err(RunEventRefusal::DecisionUnsupported { call_id: c.clone() })
    );
    assert_eq!(ledger.next_ordinal(), refused.first);
    assert_eq!(
        ledger.decision_rank(&a),
        None,
        "a refused batch reserves no rank"
    );
    let duplicate = RunRecord {
        events: vec![RunEvent::Decided {
            call_id: b.clone(),
            decision: CallDecision::Cancelled,
            after: None,
        }],
        ..refused.clone()
    };
    assert_eq!(
        ledger.append(SegmentOrdinal(0), &duplicate),
        Err(RunEventRefusal::DecidedTwice { call_id: b.clone() })
    );
    let repeated_attempt = RunRecord {
        events: vec![done(&b, 1)],
        ..refused.clone()
    };
    assert_eq!(
        ledger.append(SegmentOrdinal(0), &repeated_attempt),
        Err(RunEventRefusal::AttemptNotIssued {
            call_id: b.clone(),
            attempt: attempt(1)
        })
    );
    // A cold successor derives the same ranks solely from accepted records.
    let mut successor = RunLedger::new(opener());
    for record in [&admission, &first] {
        successor.append(record.segment, record).unwrap();
    }
    successor.admit_successor(SegmentOrdinal(1));
    let closing = RunRecord {
        trace: None,
        segment: SegmentOrdinal(1),
        first: successor.next_ordinal(),
        events: vec![
            RunEvent::Lifecycle {
                state: RunLifecycle::Closing,
            },
            RunEvent::Decided {
                call_id: c.clone(),
                decision: CallDecision::Cancelled,
                after: None,
            },
            RunEvent::Decided {
                call_id: a.clone(),
                decision: CallDecision::Cancelled,
                after: None,
            },
            done(&c, 1),
            done(&a, 1),
        ],
    };
    successor.append(SegmentOrdinal(1), &closing).unwrap();
    assert_eq!(successor.decision_rank(&b), Some(1));
    assert_eq!(successor.decision_rank(&c), Some(2));
    assert_eq!(
        successor.decision_rank(&a),
        Some(3),
        "batch order wins over call-id order"
    );
    let mut replay = RunLedger::new(opener());
    for record in [&admission, &first, &closing] {
        replay.append(record.segment, record).unwrap();
    }
    for id in [&a, &b, &c] {
        assert_eq!(replay.decision_rank(id), successor.decision_rank(id));
    }
}

#[test]
fn reported_retries_follow_one_recorded_schedule() {
    let retrying = |label: &str| {
        let mut call = call(label);
        call.policy.retry = RecordedRetryPolicy::Reported {
            max_attempts: std::num::NonZeroU32::new(2).unwrap(),
            base_delay_ms: 10,
            max_delay_ms: 100,
        };
        call
    };
    let (a, b) = (ToolCallId::fixture("a"), ToolCallId::fixture("b"));
    let retry = |call_id: &ToolCallId, failed: u32| RunEvent::RetryScheduled {
        call_id: call_id.clone(),
        failed: attempt(failed),
        next: attempt(failed + 1),
        backoff_ms: 10,
    };
    // Both wake orders are valid schedules; replay follows the recorded one.
    for a_first in [true, false] {
        let mut log = Log::new();
        log.push(RunEvent::Admitted {
            round: round(vec![retrying("a"), retrying("b")]),
        })
        .unwrap();
        log.push(failed(&a, 1, true)).unwrap();
        log.push(failed(&b, 1, true)).unwrap();
        for id in [&a, &b] {
            let registered = RunEvent::RetryTimerRegistered {
                call_id: id.clone(),
                failed: attempt(1),
                next: attempt(2),
                backoff_ms: 10,
            };
            log.push(registered.clone()).unwrap();
            assert!(
                log.push(registered).is_err(),
                "a reported failure registers only one timer"
            );
        }
        let (wake_first, wake_second) = if a_first { (&a, &b) } else { (&b, &a) };
        log.push(retry(wake_first, 1)).unwrap();
        log.push(retry(wake_second, 1)).unwrap();
        let (first, second) = if a_first { (&a, &b) } else { (&b, &a) };
        log.push(done(first, 2)).unwrap();
        log.push(done(second, 2)).unwrap();
        assert_eq!(
            log.push(retry(first, 2)),
            Err(RunEventRefusal::RetryNotEligible {
                call_id: first.clone(),
                failed: attempt(2),
                next: attempt(3)
            }),
            "a done attempt and the attempt bound both refuse a retry"
        );
    }

    let mut log = Log::new();
    log.push(RunEvent::Admitted {
        round: round(vec![retrying("a"), call("b")]),
    })
    .unwrap();
    log.push(failed(&b, 1, true)).unwrap();
    assert!(
        log.push(retry(&b, 1)).is_err(),
        "a call without a retry policy never retries"
    );
    log.push(failed(&a, 1, false)).unwrap();
    assert!(
        log.push(retry(&a, 1)).is_err(),
        "a non-retryable failure is final"
    );
    log.push(decided(&a, final_of(1, false))).unwrap();
}

#[test]
fn cancellation_during_backoff_starts_no_next_attempt() {
    let mut retrying = call("a");
    retrying.policy.retry = RecordedRetryPolicy::Reported {
        max_attempts: std::num::NonZeroU32::new(3).unwrap(),
        base_delay_ms: 10,
        max_delay_ms: 100,
    };
    let a = retrying.call_id.clone();
    let mut log = Log::new();
    log.push(RunEvent::Admitted {
        round: round(vec![retrying]),
    })
    .unwrap();
    log.push(failed(&a, 1, true)).unwrap();
    log.push(RunEvent::RetryTimerRegistered {
        call_id: a.clone(),
        failed: attempt(1),
        next: attempt(2),
        backoff_ms: 10,
    })
    .unwrap();
    log.push(RunEvent::Decided {
        call_id: a.clone(),
        decision: CallDecision::Cancelled,
        after: None,
    })
    .unwrap();
    assert!(
        log.push(RunEvent::RetryScheduled {
            call_id: a.clone(),
            failed: attempt(1),
            next: attempt(2),
            backoff_ms: 10,
        })
        .is_err()
    );
}

#[test]
fn protected_drain_is_transitive_across_intent_free_ranks() {
    let ids: Vec<_> = ["r1", "r2", "r3"].map(ToolCallId::fixture).to_vec();
    let mut log = Log::new();
    log.push(RunEvent::Admitted {
        round: round(vec![call("r1"), call("r2"), call("r3")]),
    })
    .unwrap();
    for id in &ids {
        log.push(done(id, 1)).unwrap();
    }
    log.push(decided(&ids[0], final_of(1, true))).unwrap();
    log.push(RunEvent::DeclarationsIssued {
        call_id: ids[0].clone(),
    })
    .unwrap();
    log.push(decided(&ids[1], final_of(1, false))).unwrap();
    log.push(decided(&ids[2], final_of(1, true))).unwrap();
    // Rank 2 is intent-free and seated, but rank 1 still drains.
    assert_eq!(
        log.push(RunEvent::DeclarationsIssued {
            call_id: ids[2].clone()
        }),
        Err(RunEventRefusal::DrainFrontier {
            call_id: ids[2].clone()
        })
    );
    assert_eq!(
        log.push(RunEvent::Presented {
            call_id: ids[0].clone(),
            presentation: None,
            failure: None,
        }),
        Err(RunEventRefusal::BoundaryOrder {
            call_id: ids[0].clone()
        }),
        "presentation waits for its declarations"
    );
    log.push(RunEvent::DeclarationsSettled {
        call_id: ids[0].clone(),
    })
    .unwrap();
    log.push(RunEvent::DeclarationsIssued {
        call_id: ids[2].clone(),
    })
    .unwrap();
    assert_eq!(
        log.push(RunEvent::Lifecycle {
            state: RunLifecycle::Settled
        }),
        Err(RunEventRefusal::UnsettledWork {
            call_id: ids[0].clone()
        }),
        "cancellation after a final cannot abandon its presentation"
    );
    assert_eq!(
        log.push(RunEvent::Incorporated {
            call_id: ids[1].clone()
        }),
        Err(RunEventRefusal::BoundaryOrder {
            call_id: ids[1].clone()
        }),
        "incorporation follows presentation"
    );
}

/// L08 in the fold: a declared start is admitted only inside its final's
/// issued declarations — never for a cancelled call — under a key no other
/// start of the Run holds; it is launched, then discharged, once each, and
/// the declarations cannot settle while it is owed.
#[test]
fn a_declared_start_drains_inside_its_declarations_under_one_key() {
    let ids: Vec<_> = ["start", "cancelled", "reuse"]
        .map(ToolCallId::fixture)
        .to_vec();
    let key = StartKey::for_host("fig4884-start");
    let admitted = |call_id: &ToolCallId, start_key: &StartKey| RunEvent::StartAdmitted {
        call_id: call_id.clone(),
        start_key: start_key.clone(),
    };
    let launched = RunEvent::StartLaunched {
        call_id: ids[0].clone(),
        start_key: key.clone(),
        process_id: ProcessId::fixture("fig4884-process"),
    };
    let discharged = RunEvent::StartDischarged {
        call_id: ids[0].clone(),
        start_key: key.clone(),
        cancelled: true,
    };
    let order = |call_id: &ToolCallId| RunEventRefusal::StartOrder {
        call_id: call_id.clone(),
        start_key: key.clone(),
    };
    let mut log = Log::new();
    log.push(RunEvent::Admitted {
        round: round(vec![call("start"), call("cancelled"), call("reuse")]),
    })
    .unwrap();
    for id in &ids {
        log.push(done(id, 1)).unwrap();
    }
    // The Run's cancellation of the undecided call.
    log.push(RunEvent::Decided {
        call_id: ids[1].clone(),
        decision: CallDecision::Cancelled,
        after: None,
    })
    .unwrap();
    assert_eq!(
        log.push(admitted(&ids[1], &key)),
        Err(order(&ids[1])),
        "a cancel before admission forbids the start"
    );
    log.push(decided(&ids[0], final_of(1, true))).unwrap();
    assert_eq!(
        log.push(admitted(&ids[0], &key)),
        Err(order(&ids[0])),
        "admission follows the issued declarations"
    );
    log.push(RunEvent::DeclarationsIssued {
        call_id: ids[0].clone(),
    })
    .unwrap();
    assert_eq!(log.push(launched.clone()), Err(order(&ids[0])));
    log.push(admitted(&ids[0], &key)).unwrap();
    assert_eq!(log.ledger.owed_starts(), vec![key.clone()]);
    assert_eq!(log.push(discharged.clone()), Err(order(&ids[0])));
    assert_eq!(
        log.push(RunEvent::DeclarationsSettled {
            call_id: ids[0].clone()
        }),
        Err(RunEventRefusal::StartOwed {
            call_id: ids[0].clone(),
            start_key: key.clone(),
        })
    );
    log.push(launched.clone()).unwrap();
    assert_eq!(log.push(launched), Err(order(&ids[0])), "one launch");
    assert_eq!(log.ledger.owed_starts(), vec![key.clone()]);
    log.push(discharged.clone()).unwrap();
    assert_eq!(log.push(discharged), Err(order(&ids[0])), "one discharge");
    assert!(log.ledger.owed_starts().is_empty());
    log.push(RunEvent::DeclarationsSettled {
        call_id: ids[0].clone(),
    })
    .unwrap();

    log.push(decided(&ids[2], final_of(1, true))).unwrap();
    log.push(RunEvent::DeclarationsIssued {
        call_id: ids[2].clone(),
    })
    .unwrap();
    assert_eq!(
        log.push(admitted(&ids[2], &key)),
        Err(RunEventRefusal::StartReused {
            start_key: key.clone()
        }),
        "a start key names one start of the Run"
    );

    let event = RunEvent::StartLaunched {
        call_id: ids[0].clone(),
        start_key: key.clone(),
        process_id: ProcessId::fixture("fig4884-process"),
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        json!({
            "event": "start_launched",
            "call_id": ids[0],
            "start_key": key,
            "process_id": ProcessId::fixture("fig4884-process"),
        })
    );

    // A production pending start obeys the same cancellation fence.
    let mut pending = call("pending");
    pending.declaration = ToolDeclaration::deferring().with_intents([ToolIntentKind::StartProcess]);
    let id = pending.call_id.clone();
    let mut log = Log::new();
    log.push(RunEvent::Admitted {
        round: round(vec![pending]),
    })
    .unwrap();
    log.push(RunEvent::AttemptRecorded {
        call_id: id.clone(),
        attempt: attempt(1),
        result: AttemptResult::Pending {
            source: source_key(&id),
            metadata: run_material(MaterialRole::AttemptOutput),
            start: Some(Box::new(PendingStart {
                start_key: key.clone(),
                obligation: run_material(MaterialRole::AttemptOutput),
            })),
        },
    })
    .unwrap();
    log.push(RunEvent::Decided {
        call_id: id.clone(),
        decision: CallDecision::Cancelled,
        after: None,
    })
    .unwrap();
    assert_eq!(
        log.push(admitted(&id, &key)),
        Err(order(&id)),
        "cancellation forbids admission of a recorded pending start"
    );
}

#[test]
fn check_decisions_follow_their_records_and_abort_run_stops_admission() {
    let (a, b, c) = (
        ToolCallId::fixture("a"),
        ToolCallId::fixture("b"),
        ToolCallId::fixture("c"),
    );
    let mut cached = call("b");
    cached.checks = CheckRecord::reduce(vec![before(
        "cache",
        "tool_args_check:first",
        BeforeCheckVerdict::Cached {
            result: run_material(MaterialRole::AttemptOutput),
        },
    )]);
    let mut log = Log::new();
    log.push(RunEvent::Admitted {
        round: round(vec![call("a"), cached]),
    })
    .unwrap();
    assert_eq!(
        log.push(done(&b, 1)),
        Err(RunEventRefusal::AttemptNotIssued {
            call_id: b.clone(),
            attempt: attempt(1)
        }),
        "a cached call executes no attempt"
    );
    assert_eq!(
        log.push(RunEvent::Decided {
            call_id: b.clone(),
            decision: CallDecision::Final {
                source: ResultSource::Cached,
                declares: false
            },
            after: None,
        }),
        Err(RunEventRefusal::DecisionUnsupported { call_id: b.clone() }),
        "a cached result still passes the after-checks"
    );
    log.push(RunEvent::Decided {
        call_id: b.clone(),
        decision: CallDecision::Final {
            source: ResultSource::Cached,
            declares: false,
        },
        after: allow_all(),
    })
    .unwrap();
    log.push(done(&a, 1)).unwrap();
    let abort = Some(CheckRecord::reduce(vec![
        AttributedVerdict {
            callback: callback("policy", "tool_result_check:first"),
            verdict: AfterCheckVerdict::Deny {
                cause: cause("deny"),
            },
        },
        AttributedVerdict {
            callback: callback("guard", "tool_result_check:first"),
            verdict: AfterCheckVerdict::AbortRun {
                cause: cause("abort"),
            },
        },
    ]));
    assert_eq!(
        log.push(RunEvent::Decided {
            call_id: a.clone(),
            decision: CallDecision::Denied,
            after: abort.clone(),
        }),
        Err(RunEventRefusal::DecisionUnsupported { call_id: a.clone() })
    );
    log.push(RunEvent::Decided {
        call_id: a.clone(),
        decision: CallDecision::Aborted,
        after: abort,
    })
    .unwrap();
    assert!(log.ledger.aborted());
    assert_eq!(
        log.push(RunEvent::Admitted {
            round: round(vec![call("c")]),
        }),
        Err(RunEventRefusal::AdmissionClosed)
    );
    let _ = c;
}

#[test]
fn material_references_refuse_typed_and_retention_keeps_identity() {
    for bad in ["", "AAAA", &DIGEST_A[1..], &format!("{DIGEST_A}0")] {
        assert!(MaterialDigest::parse(bad).is_err());
    }
    assert!(serde_json::from_value::<MaterialDigest>(json!("not-a-digest")).is_err());
    let local = run_material(MaterialRole::AttemptOutput);
    assert!(!local.crosses_segments());
    let artifact = ArtifactName {
        store: ArtifactStoreId::Engine("restate".into()),
        artifact_ref: "bundle-1".into(),
    };
    let retained = local.retained(artifact.clone());
    assert!(retained.crosses_segments());
    assert_eq!(
        (&retained.owner, retained.role, &retained.digest),
        (&local.owner, local.role, &local.digest)
    );
    assert_eq!(
        serde_json::to_value(&retained.location).unwrap(),
        json!({"location": "retained_artifact", "artifact": {"store": {"store": "engine", "kind": "restate"}, "artifact_ref": "bundle-1"}})
    );

    let owner = MaterialOwner::Run { opener: opener() };
    assert_eq!(local.verify(&owner, &digest(DIGEST_A)), Ok(()));
    let corrupt = local.verify(&owner, &digest(DIGEST_B)).unwrap_err();
    assert_eq!(corrupt.code(), "material_corrupt");
    let other = MaterialOwner::Process {
        process_id: ProcessId::fixture("worker"),
    };
    let wrong = local.verify(&other, &digest(DIGEST_B)).unwrap_err();
    assert_eq!(wrong.code(), "material_wrong_owner");
    let encoded = serde_json::to_value(&wrong).unwrap();
    assert_eq!(encoded["refusal"], json!("wrong_owner"));
    assert_eq!(
        serde_json::from_value::<MaterialRefusal>(encoded).unwrap(),
        wrong
    );
    for (refusal, code) in [
        (
            MaterialRefusal::Missing {
                reference: Box::new(local.clone()),
            },
            "material_missing",
        ),
        (
            MaterialRefusal::Retired {
                reference: Box::new(local.clone()),
            },
            "material_retired",
        ),
        (
            MaterialRefusal::RevisionMismatch {
                reference: Box::new(local.clone()),
                recorded: revision("tools"),
                available: Vec::new(),
            },
            "material_revision_mismatch",
        ),
    ] {
        assert_eq!(refusal.code(), code);
    }
}

#[test]
fn a_source_seals_once_and_only_its_authority_resolves_it() {
    let call_id = ToolCallId::fixture("deferred");
    let process_id = ProcessId::fixture("worker");
    let descriptor = SourceDescriptor {
        source: source_key(&call_id),
        call_id: call_id.clone(),
        owner: opener(),
        resolver: revision("tools"),
        authority: SourceAuthority::ProcessTerminal {
            process_id: process_id.clone(),
        },
        cancel: ExternalCancelPolicy::CancelExternalWork,
    };
    let source_output = MaterialRef {
        owner: MaterialOwner::Source {
            source: source_key(&call_id),
        },
        ..run_material(MaterialRole::AttemptOutput)
    };
    let resolved = SourceSeal::Resolved {
        result: Box::new(source_output.retained(tool_material("bundle-1"))),
    };
    let worker = SealWriter::Process {
        process_id: process_id.clone(),
    };
    assert_eq!(
        descriptor.seal(None, &SealWriter::External, resolved.clone()),
        Err(SealRefusal::WrongAuthority)
    );
    assert_eq!(
        descriptor.seal(
            None,
            &SealWriter::Owner {
                opener: EffectOpener::turn("session-2", "turn-1")
            },
            SourceSeal::Cancelled
        ),
        Err(SealRefusal::WrongAuthority)
    );
    assert_eq!(
        descriptor.seal(
            None,
            &worker,
            SourceSeal::Resolved {
                result: Box::new(run_material(MaterialRole::AttemptOutput))
            }
        ),
        Err(SealRefusal::ResultNotOwned)
    );
    assert_eq!(
        descriptor.seal(
            None,
            &worker,
            SourceSeal::Resolved {
                result: Box::new(source_output)
            }
        ),
        Err(SealRefusal::UnretainedResult),
        "a seal publishes only material its source already retained"
    );
    assert_eq!(
        descriptor.seal(None, &worker, resolved.clone()),
        Ok(SealOutcome::Sealed {
            seal: resolved.clone()
        })
    );
    let cancelled = descriptor
        .seal(
            None,
            &SealWriter::Owner { opener: opener() },
            SourceSeal::Cancelled,
        )
        .unwrap();
    assert_eq!(
        cancelled,
        SealOutcome::Sealed {
            seal: SourceSeal::Cancelled
        }
    );
    assert_eq!(
        descriptor.seal(Some(&SourceSeal::Cancelled), &worker, resolved.clone()),
        Ok(SealOutcome::AlreadySealed {
            seal: SourceSeal::Cancelled
        }),
        "a late resolution cannot revive cancelled work"
    );
    assert_eq!(
        serde_json::to_value(&cancelled).unwrap(),
        json!({"outcome": "sealed", "seal": {"seal": "cancelled"}})
    );
    for timed_out in [json!({"seal": "timed_out"}), json!({"seal": "timeout"})] {
        assert!(serde_json::from_value::<SourceSeal>(timed_out).is_err());
    }
}

fn tool_material(artifact_ref: &str) -> ArtifactName {
    ArtifactName {
        store: ArtifactStoreId::ToolMaterial,
        artifact_ref: artifact_ref.into(),
    }
}

fn segment_holder(segment: u32) -> MaterialHolder {
    MaterialHolder::Segment {
        opener: opener(),
        segment: SegmentOrdinal(segment),
    }
}

fn leased_bundle(holder: MaterialHolder) -> RetainedBundle {
    RetainedBundle {
        holder,
        artifact: tool_material("bundle-1"),
        references: vec![
            run_material(MaterialRole::AttemptOutput).retained(tool_material("bundle-1")),
        ],
        copy_bytes: 512,
    }
}

fn transfer() -> RunTransfer {
    let call_id = ToolCallId::fixture("deferred");
    let mut member = call("deferred");
    member.declaration = ToolDeclaration::deferring();
    let events = vec![
        RunEvent::Admitted {
            round: round(vec![member]),
        },
        RunEvent::AttemptRecorded {
            call_id: call_id.clone(),
            attempt: AttemptOrdinal::FIRST,
            result: AttemptResult::Deferred {
                source: source_key(&call_id),
            },
        },
    ];
    let entries = vec![RunJournalEntry {
        record: RunRecord {
            segment: SegmentOrdinal(0),
            first: RunEventOrdinal(0),
            events,
            trace: None,
        },
        materials: Vec::new(),
        state: Vec::new(),
    }];
    RunTransfer {
        owner: opener(),
        from: SegmentOrdinal(0),
        entries,
        attempts: Vec::new(),
        material_aliases: Vec::new(),
        sources: Vec::new(),
        environment: None,
        plugin_state: None,
        material: vec![leased_bundle(segment_holder(0)).into()],
        subscriptions: vec![source_key(&call_id)],
    }
}

/// S01 F1 / S08 F1: a transfer cannot decode a second truth for a journal
/// frontier or its container's boundary reason.
#[test]
fn s01_transfer_refuses_conflicting_copies_of_derived_facts() {
    for (field, conflicting) in [
        ("events", json!(999)),
        ("held_calls", json!(999)),
        ("reason", json!("journal_budget")),
        ("vm_continuation", json!(false)),
        ("owed_starts", json!([])),
        ("owed_cancels", json!([])),
        (
            "state",
            serde_json::to_value(StateFrontier::default()).unwrap(),
        ),
    ] {
        let mut encoded = serde_json::to_value(transfer()).unwrap();
        encoded[field] = conflicting;
        assert!(
            serde_json::from_value::<RunTransfer>(encoded).is_err(),
            "{field}"
        );
    }
    let mut encoded = serde_json::to_value(transfer()).unwrap();
    encoded["material"][0]["holder"] = serde_json::to_value(segment_holder(4)).unwrap();
    assert!(serde_json::from_value::<RunTransfer>(encoded).is_err());
    let mut encoded = serde_json::to_value(transfer()).unwrap();
    encoded["subscriptions"][0] = serde_json::to_value(SourceSubscription {
        source: source_key(&ToolCallId::fixture("deferred")),
        owner: opener(),
        segment: SegmentOrdinal(4),
    })
    .unwrap();
    assert!(serde_json::from_value::<RunTransfer>(encoded).is_err());
}

/// L10: ownership and terminal refusal apply before adoption; L09: the
/// in-memory successor fences the predecessor while the capture stays valid.
#[test]
fn l10_adoption_binds_a_live_owner_and_l09_fences_its_predecessor() {
    let capture = transfer();
    assert_eq!(
        capture.clone().adopt(
            &EffectOpener::turn("session-1", "fresh"),
            RunLifecycle::Live,
            SegmentOrdinal(1)
        ),
        Err(ContinuationRefusal::ForeignOwner)
    );
    assert_eq!(
        capture
            .clone()
            .adopt(&opener(), RunLifecycle::Settled, SegmentOrdinal(1)),
        Err(ContinuationRefusal::OwnerTerminal)
    );
    assert_eq!(
        capture
            .clone()
            .adopt(&opener(), RunLifecycle::Live, SegmentOrdinal(2)),
        Err(ContinuationRefusal::NotSuccessor { from: 0, found: 2 })
    );
    let adopted = capture
        .adopt(&opener(), RunLifecycle::Live, SegmentOrdinal(1))
        .unwrap();
    adopted
        .transfer
        .check_capture(CutPhase::Capturable)
        .unwrap();
    let mut ledger = adopted.ledger().unwrap();
    let record = RunRecord {
        trace: None,
        segment: SegmentOrdinal(0),
        first: ledger.next_ordinal(),
        events: vec![RunEvent::Lifecycle {
            state: RunLifecycle::Closing,
        }],
    };
    assert_eq!(
        ledger.append(SegmentOrdinal(0), &record),
        Err(RunEventRefusal::StaleSegment {
            latest: 1,
            found: 0
        })
    );
}

/// FIG-4889: material that stays in its opener journal needs no artifact
/// transaction, a bundle reports its handover copy, and a retained bundle
/// whose bytes no longer hash to a reference refuses typed.
#[test]
fn bundles_retain_only_crossing_material_and_refuse_corrupt_bytes() {
    assert_eq!(MaterialBundle::of(Vec::new()).unwrap(), None);
    let owner = MaterialOwner::Run { opener: opener() };
    let payload = MaterialPayload::new(
        owner.clone(),
        MaterialRole::AttemptOutput,
        None,
        "output".repeat(256),
    );
    let bundle = MaterialBundle::of([payload.clone(), payload.clone()])
        .unwrap()
        .unwrap();
    let [reference] = bundle.references() else {
        panic!("one payload, deduplicated by digest");
    };
    assert_eq!(bundle.artifact().store, ArtifactStoreId::ToolMaterial);
    let retained = bundle.retained_by(segment_holder(0));
    assert!(retained.is_retained());
    assert_eq!(retained.copy_bytes, bundle.bytes().len() as u64);
    assert_eq!(
        MaterialBundle::read(bundle.bytes(), reference, &owner, &[]).unwrap(),
        payload
    );
    let tampered = String::from_utf8(bundle.bytes().to_vec())
        .unwrap()
        .replace("outputoutput", "tamperedoutp");
    let error = MaterialBundle::read(tampered.as_bytes(), reference, &owner, &[]).unwrap_err();
    assert!(matches!(
        error.cause,
        Some(crate::RuntimeErrorCause::MaterialRefused { ref refusal })
            if matches!(**refusal, MaterialRefusal::Corrupt { .. })
    ));
    assert!(error.journaled, "a refused read never grants a fresh body");
}

#[test]
fn receipts_name_the_logical_call_and_permits_come_from_records() {
    let a = ToolCallId::fixture("a");
    let admitted = RunEvent::Admitted {
        round: round(vec![call("a"), call("b")]),
    };
    assert_eq!(
        BusinessReceipt::for_event(&admitted),
        vec![
            BusinessReceipt::Accepted { call_id: a.clone() },
            BusinessReceipt::Accepted {
                call_id: ToolCallId::fixture("b")
            },
        ]
    );
    let deferred = RunEvent::AttemptRecorded {
        call_id: a.clone(),
        attempt: attempt(1),
        result: AttemptResult::Deferred {
            source: source_key(&a),
        },
    };
    assert!(
        BusinessReceipt::for_event(&deferred).is_empty(),
        "Deferred is pending"
    );
    let permits = ObservationPermit::for_recorded(RunEventOrdinal(1), &deferred);
    assert_eq!(permits.len(), 1);
    assert_eq!(
        permits[0].fact(),
        &ObservedFact::Attempt {
            call_id: a.clone(),
            attempt: attempt(1)
        }
    );
    let terminal = ObservationPermit::for_recorded(
        RunEventOrdinal(2),
        &RunEvent::Decided {
            call_id: a.clone(),
            decision: CallDecision::Cancelled,
            after: None,
        },
    );
    assert_eq!(
        terminal[0].fact(),
        &ObservedFact::Logical(BusinessReceipt::Terminal {
            call_id: a,
            terminal: LogicalTerminal::Cancelled
        })
    );
    assert_eq!(terminal[0].recorded(), RunEventOrdinal(2));
}

#[test]
fn only_sequential_turn_and_tool_result_check_callbacks_publish_state() {
    let commands: Vec<_> = CallbackSlot::ALL
        .iter()
        .filter(|slot| slot.state_authority() == StateAuthority::Commands)
        .map(|slot| slot.key_prefix())
        .collect();
    assert_eq!(
        commands,
        vec![
            "before_turn",
            "tool_result_check",
            "after_turn",
            "checkpoint"
        ]
    );
    let mut prefixes: Vec<_> = CallbackSlot::ALL
        .iter()
        .map(|slot| slot.key_prefix())
        .collect();
    prefixes.sort_unstable();
    prefixes.dedup();
    assert_eq!(prefixes.len(), CallbackSlot::ALL.len());
    assert_eq!(
        CallbackSlot::of(&callback("p", "presentation_presenter")),
        Some(CallbackSlot::PresentationPresenter)
    );
    assert_eq!(
        CallbackSlot::of(&callback("p", "operation:compact")),
        Some(CallbackSlot::Operation)
    );
    assert_eq!(CallbackSlot::of(&callback("p", "unknown:0")), None);

    let limits = StateCommandLimits {
        max_commands: 2,
        max_encoded_bytes: 256,
    };
    let batch = StateCommandBatch {
        plugin: revision("state"),
        origin: StateCommandOrigin::TurnHook {
            callback: callback("state", "after_turn:0"),
            segment: SegmentOrdinal(0),
        },
        commands: vec![
            StateCommand::Set {
                key: "count".into(),
                value: json!(1),
            },
            StateCommand::Apply {
                key: "total".into(),
                name: "add".into(),
                input: json!(2),
            },
        ],
    };
    assert_eq!(
        batch.check(&callback("state", "after_turn:0"), limits),
        Ok(())
    );
    assert_eq!(
        batch.check(&callback("state", "assistant_response:0"), limits),
        Err(StateCommandRefusal::DecisionOnly)
    );
    assert_eq!(
        batch.check(&callback("state", "tool_args_check:first"), limits),
        Err(StateCommandRefusal::DecisionOnly)
    );
    assert_eq!(
        batch.check(&callback("other", "checkpoint:0"), limits),
        Err(StateCommandRefusal::WrongOwner)
    );
    assert_eq!(
        batch.check(
            &callback("state", "checkpoint:0"),
            StateCommandLimits {
                max_commands: 1,
                ..limits
            }
        ),
        Err(StateCommandRefusal::TooManyCommands { count: 2 })
    );
    assert!(matches!(
        batch.check(
            &callback("state", "checkpoint:0"),
            StateCommandLimits {
                max_encoded_bytes: 8,
                ..limits
            }
        ),
        Err(StateCommandRefusal::TooLarge { .. })
    ));
    let mut blank = batch.clone();
    blank
        .commands
        .push(StateCommand::Remove { key: " ".into() });
    assert_eq!(
        blank.check(
            &callback("state", "checkpoint:0"),
            StateCommandLimits {
                max_commands: 3,
                max_encoded_bytes: 1024,
            }
        ),
        Err(StateCommandRefusal::InvalidKey {
            index: 2,
            reason: crate::plugin_state::KeyRejection::IllegalCharacter { at: 0, byte: b' ' },
        })
    );
    assert_eq!(
        serde_json::to_value(&batch.commands[1]).unwrap(),
        json!({"command": "apply", "key": "total", "name": "add", "input": 2})
    );
}

#[test]
fn the_state_frontier_applies_each_publication_once_in_order() {
    let resolution = |ordinal: u64, predecessor: Option<u64>, segment: u32| StateResolution {
        publisher: crate::EffectAddress::new(
            ExecutionScope::turn("session-1", "turn-1"),
            "attempt:a",
        )
        .unwrap(),
        plugin: revision("state"),
        origin: StateCommandOrigin::ToolAttempt {
            call_id: ToolCallId::fixture("a"),
            attempt: attempt(1),
        },
        segment: SegmentOrdinal(segment),
        ordinal: PublicationOrdinal(ordinal),
        predecessor: predecessor.map(PublicationOrdinal),
        outcome: StateResolutionOutcome::Applied {
            changes: vec![ResolvedStateChange::Put {
                key: "count".into(),
                value: json!(1),
            }],
        },
    };
    let empty = StateFrontier::default();
    assert_eq!(empty.step(&resolution(1, None, 0)), Ok(FrontierStep::Apply));
    assert_eq!(
        empty.step(&resolution(2, Some(1), 0)),
        Err(FrontierRefusal::OutOfOrder { found: 2 })
    );
    let mut applied = StateFrontier {
        applied: Some(PublicationOrdinal(3)),
        owner_segment: SegmentOrdinal(1),
        receipts: Default::default(),
    };
    applied
        .receipts
        .insert(PublicationOrdinal(2), resolution(2, Some(1), 1).receipt());
    assert_eq!(
        applied.step(&resolution(2, Some(1), 1)),
        Ok(FrontierStep::AlreadyApplied),
        "a checkpoint never reapplies an older delta"
    );
    assert_eq!(
        applied.step(&resolution(4, Some(3), 1)),
        Ok(FrontierStep::Apply)
    );
    assert_eq!(
        applied.step(&resolution(4, Some(2), 1)),
        Err(FrontierRefusal::OutOfOrder { found: 4 })
    );
    assert_eq!(
        applied.step(&resolution(4, Some(3), 0)),
        Err(FrontierRefusal::StalePublisher { owner: 1, found: 0 }),
        "a predecessor cannot publish after transfer"
    );
    let refused = StateResolutionOutcome::Refused {
        refusal: StateCommandRefusal::Reducer {
            index: 1,
            cause: cause("overdraft"),
        },
    };
    let encoded = serde_json::to_value(&refused).unwrap();
    assert_eq!(encoded["refusal"]["refusal"], json!("reducer"));
    assert_eq!(
        serde_json::from_value::<StateResolutionOutcome>(encoded).unwrap(),
        refused
    );
}

#[test]
fn an_operation_run_keeps_the_session_operation_identity() {
    let operation = OperationRun {
        session_id: "session-1".into(),
        operation_id: "batch-7".into(),
    };
    assert_eq!(
        OperationRun::for_run_id(operation.session_id.clone(), &operation.run_id()),
        Some(operation.clone())
    );
    let opener = operation.opener();
    assert_eq!(opener.identity_encoding(), "drain:9:session-1:7:batch-7");
    assert_eq!(
        opener.tool_call_admission(),
        EffectOpener::session_operation("session-1", "batch-7").tool_call_admission()
    );
    assert_eq!(
        serde_json::to_value(operation.input()).unwrap(),
        json!({"input": "operation", "operation_id": "batch-7"})
    );
    assert_eq!(
        operation.run_id().as_str(),
        "shift-operation:batch-7",
        "every admission of the operation names one run"
    );
}

#[test]
fn presentation_plans_record_explicit_empty_and_decision_only_callbacks() {
    let empty = PresentationBinding::default();
    let encoded = serde_json::to_value(&empty).unwrap();
    assert_eq!(encoded, json!({"presenter": null, "steps": []}));
    assert_eq!(
        serde_json::from_value::<PresentationBinding>(encoded).unwrap(),
        empty
    );
    assert!(serde_json::from_value::<PresentationBinding>(json!({"steps": []})).is_err());
    assert!(serde_json::from_value::<PresentationBinding>(json!({"presenter": null})).is_err());
    let plan = binding().presentation;
    let recorded: PresentationBinding =
        serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
    assert_eq!(recorded, plan);
    let limits = StateCommandLimits {
        max_commands: 1,
        max_encoded_bytes: 256,
    };
    for proposer in recorded
        .callbacks()
        .chain([&callback("render", "assistant_response:derive")])
    {
        let batch = StateCommandBatch {
            plugin: proposer.owner.clone(),
            origin: StateCommandOrigin::TurnHook {
                callback: proposer.clone(),
                segment: SegmentOrdinal(0),
            },
            commands: vec![StateCommand::Set {
                key: "key".into(),
                value: json!(1),
            }],
        };
        assert_eq!(
            batch.check(proposer, limits),
            Err(StateCommandRefusal::DecisionOnly)
        );
    }
}

/// L19: a published state frontier survives the tagged journal envelope.
#[test]
fn l19_publication_receipts_round_trip_inside_a_journal_variant() {
    #[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "outcome")]
    enum Journal {
        Published { frontier: StateFrontier },
    }
    let frontier = StateFrontier {
        applied: Some(PublicationOrdinal(1)),
        owner_segment: SegmentOrdinal(0),
        receipts: [(
            PublicationOrdinal(1),
            crate::BlobRef::for_content(b"recorded resolution"),
        )]
        .into(),
    };
    let record = Journal::Published { frontier };
    let bytes = serde_json::to_vec(&record).unwrap();
    assert_eq!(serde_json::from_slice::<Journal>(&bytes).unwrap(), record);
    let bytes = rmp_serde::to_vec_named(&record).unwrap();
    assert_eq!(rmp_serde::from_slice::<Journal>(&bytes).unwrap(), record);
}

/// K1: a round reserves one call per unique member, aliases nothing more. A
/// held round keeps its whole reservation until every member is presented;
/// a cell's round counts for the Run's whole life, and only for that cell.
#[test]
fn capacity_holds_a_round_whole_until_every_member_is_presented() {
    let (a, b, c) = (call("held-a"), call("held-b"), call("cell-c"));
    let mut log = Log::new();
    let mut held = round(vec![a.clone(), b.clone()]);
    held.operands = vec![0, 0, 1];
    log.push(RunEvent::Admitted { round: held }).unwrap();
    let cell = CapacityScope::Cell {
        key: "cell-1".into(),
    };
    log.push(RunEvent::Admitted {
        round: RoundAdmission {
            capacity: cell.clone(),
            ..round(vec![c.clone()])
        },
    })
    .unwrap();
    assert_eq!(log.ledger.counted(&CapacityScope::Held), 2);
    assert_eq!(log.ledger.counted(&cell), 1);
    assert_eq!(
        log.ledger.counted(&CapacityScope::Cell {
            key: "cell-2".into()
        }),
        0
    );
    assert_eq!(log.ledger.held_calls(), 3);
    let present = |log: &mut Log, id: &ToolCallId| {
        log.push(done(id, 1)).unwrap();
        log.push(decided(id, final_of(1, false))).unwrap();
        log.push(RunEvent::Presented {
            call_id: id.clone(),
            presentation: None,
            failure: None,
        })
        .unwrap();
    };
    present(&mut log, &a.call_id);
    assert_eq!(
        log.ledger.counted(&CapacityScope::Held),
        2,
        "a presented winner releases nothing while its sibling is held"
    );
    present(&mut log, &b.call_id);
    present(&mut log, &c.call_id);
    assert_eq!(log.ledger.counted(&CapacityScope::Held), 0);
    assert_eq!(
        log.ledger.counted(&cell),
        1,
        "a cell counts every call it made"
    );
    assert_eq!(log.ledger.held_calls(), 1);
}
