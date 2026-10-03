//! Codec, refusal and transition witnesses of the tool-run contract.

use lash_core_ids::BehaviorRevision;
use lash_sansio::{ToolCallId, ToolIntentKind};
use serde_json::json;

use super::*;
use crate::artifact_referrer::{ArtifactName, ArtifactStoreId};
use crate::await_event_identity::{AwaitEventKey, AwaitEventWaitIdentity};
use crate::effect_opener::EffectOpener;
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
            presenter: callback("standard", "presentation_presenter"),
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
            segment: SegmentOrdinal(0),
            first: self.ledger.next_ordinal(),
            events: vec![event],
        };
        self.ledger.append(SegmentOrdinal(0), &record)
    }
}

fn decided(call_id: &ToolCallId, rank: u64, decision: CallDecision) -> RunEvent {
    RunEvent::Decided {
        call_id: call_id.clone(),
        rank,
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
        vec![decided(&id, 1, final_of(1, false))],
        vec![
            RunEvent::Presented {
                call_id: id.clone(),
                presentation: None,
            },
            RunEvent::Incorporated {
                call_id: id.clone(),
            },
        ],
    ];
    let mut ledger = RunLedger::new(opener());
    for events in &records {
        let record = RunRecord {
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
        serde_json::to_value(decided(&id, 1, final_of(1, false))).unwrap(),
        json!({
            "event": "decided",
            "call_id": id.as_str(),
            "rank": 1,
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
    let admitted = ok.clone().admit(&available(), |_| false).unwrap();
    assert_eq!(admitted.reserved_calls(), 2);

    let mut aliased = round(vec![call("a")]);
    aliased.operands = vec![0, 0];
    assert_eq!(
        aliased
            .admit(&available(), |_| false)
            .unwrap()
            .reserved_calls(),
        1
    );

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
fn final_or_cancel_chooses_once_and_ranks_rise() {
    let mut log = Log::new();
    let (a, b) = (ToolCallId::fixture("a"), ToolCallId::fixture("b"));
    log.push(RunEvent::Admitted {
        round: round(vec![call("a"), call("b")]),
    })
    .unwrap();
    log.push(done(&a, 1)).unwrap();
    assert_eq!(
        log.push(done(&a, 1)),
        Err(RunEventRefusal::AttemptNotIssued {
            call_id: a.clone(),
            attempt: attempt(1)
        })
    );
    log.push(decided(&a, 2, final_of(1, false))).unwrap();
    assert_eq!(
        log.push(RunEvent::Decided {
            call_id: a.clone(),
            rank: 3,
            decision: CallDecision::Cancelled,
            after: None,
        }),
        Err(RunEventRefusal::DecidedTwice { call_id: a.clone() })
    );
    assert_eq!(
        log.push(RunEvent::Decided {
            call_id: b.clone(),
            rank: 2,
            decision: CallDecision::Cancelled,
            after: None,
        }),
        Err(RunEventRefusal::RankOrder { rank: 2, last: 2 })
    );
    assert_eq!(
        log.push(decided(&b, 3, final_of(1, false))),
        Err(RunEventRefusal::DecisionUnsupported { call_id: b.clone() }),
        "a final needs a recorded attempt"
    );
    log.push(RunEvent::Decided {
        call_id: b.clone(),
        rank: 3,
        decision: CallDecision::Cancelled,
        after: None,
    })
    .unwrap();
    // The cancelled call's issued attempt still settles durably.
    log.push(done(&b, 1)).unwrap();
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
        log.push(retry(&a, 1)).unwrap();
        log.push(retry(&b, 1)).unwrap();
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
    log.push(decided(&a, 1, final_of(1, false))).unwrap();
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
    log.push(RunEvent::Decided {
        call_id: a.clone(),
        rank: 1,
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
    log.push(decided(&ids[0], 1, final_of(1, true))).unwrap();
    log.push(RunEvent::DeclarationsIssued {
        call_id: ids[0].clone(),
    })
    .unwrap();
    log.push(decided(&ids[1], 2, final_of(1, false))).unwrap();
    log.push(decided(&ids[2], 3, final_of(1, true))).unwrap();
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
            rank: 1,
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
        rank: 1,
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
            rank: 2,
            decision: CallDecision::Denied,
            after: abort.clone(),
        }),
        Err(RunEventRefusal::DecisionUnsupported { call_id: a.clone() })
    );
    log.push(RunEvent::Decided {
        call_id: a.clone(),
        rank: 2,
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
    RunTransfer {
        owner: opener(),
        reason: lash_sansio::BoundaryReason::HandOver,
        from: SegmentOrdinal(0),
        events: RunEventOrdinal(7),
        material: vec![leased_bundle(segment_holder(0))],
        subscriptions: vec![SourceSubscription {
            source: source_key(&call_id),
            owner: opener(),
            segment: SegmentOrdinal(0),
        }],
        owed_starts: Vec::new(),
        owed_cancels: Vec::new(),
        state: StateFrontier::default(),
        reserved_calls: 1,
        vm_continuation: true,
    }
}

#[test]
fn a_cut_captures_only_after_local_quiescence() {
    let cut = Cut::request(lash_sansio::BoundaryReason::JournalBudget);
    assert!(!cut.admits_new_work());
    assert_eq!(
        transfer().check_capture(&cut),
        Err(ContinuationRefusal::NotQuiescent)
    );
    let quiescing = cut.observe(1);
    assert_eq!(quiescing.phase, CutPhase::Quiescing);
    assert_eq!(
        transfer().check_capture(&quiescing),
        Err(ContinuationRefusal::NotQuiescent)
    );
    let capturable = quiescing.observe(0);
    assert_eq!(transfer().check_capture(&capturable), Ok(()));
    let mut local = transfer();
    local.material[0].references = vec![run_material(MaterialRole::AttemptOutput)];
    assert_eq!(
        local.check_capture(&capturable),
        Err(ContinuationRefusal::UnretainedMaterial)
    );
    let mut foreign = transfer();
    foreign.subscriptions[0].segment = SegmentOrdinal(4);
    assert_eq!(
        foreign.check_capture(&capturable),
        Err(ContinuationRefusal::ForeignSubscription)
    );
}

/// FIG-4889: a reference relocated to an artifact is not retained until the
/// transferring segment holds its dependency lease. Without the lease the
/// bytes can be reclaimed between publication and the successor's acquire.
#[test]
fn a_transfer_refuses_retained_material_without_the_predecessor_lease() {
    let capturable = Cut::request(lash_sansio::BoundaryReason::HandOver).observe(0);
    let elsewhere = ArtifactName {
        store: ArtifactStoreId::Engine("restate".into()),
        artifact_ref: "bundle-1".into(),
    };
    let mut relocated = transfer();
    relocated.material[0].artifact = elsewhere.clone();
    for reference in &mut relocated.material[0].references {
        *reference = reference.retained(elsewhere.clone());
    }
    assert_eq!(
        relocated.check_capture(&capturable),
        Err(ContinuationRefusal::UnretainedMaterial),
        "a reference moved to some artifact has no lease behind it"
    );
    for holder in [
        segment_holder(1),
        MaterialHolder::Segment {
            opener: EffectOpener::turn("session-1", "turn-2"),
            segment: SegmentOrdinal(0),
        },
    ] {
        let mut unleased = transfer();
        unleased.material = vec![leased_bundle(holder)];
        assert_eq!(
            unleased.check_capture(&capturable),
            Err(ContinuationRefusal::UnleasedMaterial)
        );
    }
    assert_eq!(transfer().check_capture(&capturable), Ok(()));
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

/// L10's protocol witness: a cancelled Run's continuation cannot infect a
/// fresh Run, and only the owner's next segment adopts it.
#[test]
fn a_continuation_is_adopted_only_by_its_own_live_run() {
    let fresh = EffectOpener::turn("session-1", "turn-2");
    assert_eq!(
        transfer().adopt(&fresh, RunLifecycle::Live, SegmentOrdinal(1)),
        Err(ContinuationRefusal::ForeignOwner)
    );
    assert_eq!(
        transfer().adopt(&opener(), RunLifecycle::Settled, SegmentOrdinal(1)),
        Err(ContinuationRefusal::OwnerTerminal)
    );
    assert_eq!(
        transfer().adopt(&opener(), RunLifecycle::Live, SegmentOrdinal(2)),
        Err(ContinuationRefusal::NotSuccessor { from: 0, found: 2 })
    );
    let adopted = transfer()
        .adopt(&opener(), RunLifecycle::Closing, SegmentOrdinal(1))
        .unwrap();
    assert_eq!(adopted.subscriptions[0].segment, SegmentOrdinal(1));
    assert_eq!(adopted.state.owner_segment, SegmentOrdinal(1));
    let encoded = serde_json::to_value(&adopted).unwrap();
    assert_eq!(
        serde_json::from_value::<RunTransfer>(encoded).unwrap(),
        adopted
    );
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
            rank: 1,
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
        Err(StateCommandRefusal::InvalidKey { index: 2 })
    );
    assert_eq!(
        serde_json::to_value(&batch.commands[1]).unwrap(),
        json!({"command": "apply", "key": "total", "name": "add", "input": 2})
    );
}

#[test]
fn the_state_frontier_applies_each_publication_once_in_order() {
    let resolution = |ordinal: u64, predecessor: Option<u64>, segment: u32| StateResolution {
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
    let applied = StateFrontier {
        applied: Some(PublicationOrdinal(3)),
        owner_segment: SegmentOrdinal(1),
    };
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
}
