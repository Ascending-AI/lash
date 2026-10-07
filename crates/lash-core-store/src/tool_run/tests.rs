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

fn attempt(n: u32) -> AttemptOrdinal {
    AttemptOrdinal::new(n).unwrap()
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
fn material_references_refuse_typed_and_retention_keeps_identity() {
    for bad in ["", "AAAA", &DIGEST_A[1..], &format!("{DIGEST_A}0")] {
        assert!(MaterialDigest::parse(bad).is_err());
    }
    assert!(serde_json::from_value::<MaterialDigest>(json!("not-a-digest")).is_err());
    let local = run_material(MaterialRole::AttemptOutput);
    assert!(!local.crosses_segments());
    let artifact = ArtifactName {
        store: ArtifactStoreId::Engine("engine-store".into()),
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
        json!({"location": "retained_artifact", "artifact": {"store": {"store": "engine", "kind": "engine-store"}, "artifact_ref": "bundle-1"}})
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

fn source_holder() -> MaterialHolder {
    MaterialHolder::Source {
        source: AwaitEventKey {
            scope: ExecutionScope::turn("s", "t"),
            wait: AwaitEventWaitIdentity::SessionCommandCancelSignal,
            key_id: "key".into(),
            signature: "signature".into(),
        },
    }
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
    let retained = bundle.retained_by(source_holder());
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
