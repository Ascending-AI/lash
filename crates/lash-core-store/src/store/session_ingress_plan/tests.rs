use super::*;
use crate::session_ingress_vocabulary::{
    IngressItemDraft, IngressPayload, IngressState, IngressWithdrawSelector,
};
use crate::store::{AdmissionId, DriveFence};
use crate::store::{IngressCommandOutcome, IngressTurnCancel};
use crate::{
    CheckpointKind, SessionCommand, TurnCancelDisposition, TurnCancelMode, TurnInput,
    TurnInputCheckpointBoundary,
};

const GENERATION: u64 = 7;

fn session() -> SessionId {
    SessionId::from("session")
}

fn fence() -> DriveFence {
    DriveFence::sealed_by_store(session(), GENERATION, AdmissionId::new("admission"))
}

fn wake_delivery(sequence: u64) -> crate::ProcessWakeDelivery {
    crate::ProcessWakeDelivery {
        version: crate::process_identity::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: format!("wake-{sequence}"),
        target_session_id: session(),
        process_id: ProcessId::from("process"),
        process_incarnation: crate::ProcessIncarnation::from_registration_sequence(1),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: crate::RuntimeInvocation {
            attribution: crate::effect_identity::RuntimeAttribution::for_session("session"),
            subject: crate::effect_identity::RuntimeSubject::ProcessEvent {
                process_id: ProcessId::from("process"),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: "wake".to_string(),
        created_at_ms: 1,
    }
}

fn item(seq: u64, draft: &IngressItemDraft) -> IngressItem {
    IngressItem {
        item_id: IngressItemId::new(format!("item-{seq}")),
        session_id: session(),
        enqueue_seq: seq,
        source_key: draft.source_key().map(str::to_string),
        delivery: draft.delivery().clone(),
        submission_digest: draft.submission_digest().expect("digest"),
        payload: draft.payload().clone(),
        authority: draft.authority().cloned(),
        merge_key: draft.merge_key().map(str::to_string),
        state: IngressState::Open,
        terminal_cause: None,
        enqueued_at_ms: 0,
        terminal_at_ms: None,
    }
}

fn free(item: IngressItem) -> IngressClaimCandidate {
    IngressClaimCandidate {
        item,
        claim_fencing_token: 0,
        claim: None,
        addressed_turn_ended: false,
        claim_turn_ended: false,
    }
}

fn input(seq: u64, delivery: Delivery) -> IngressClaimCandidate {
    free(item(
        seq,
        &IngressItemDraft::input(session(), delivery, TurnInput::text(format!("input {seq}"))),
    ))
}

fn wake(seq: u64) -> IngressClaimCandidate {
    free(item(
        seq,
        &IngressItemDraft::process_wake(wake_delivery(seq)),
    ))
}

fn refresh(seq: u64) -> IngressClaimCandidate {
    free(item(
        seq,
        &IngressItemDraft::session_command(
            session(),
            SessionCommand::RefreshToolCatalog {
                reason: "test".to_string(),
            },
            format!("refresh-{seq}"),
        ),
    ))
}

fn addressed(turn: &str, min_boundary: TurnInputCheckpointBoundary) -> Delivery {
    Delivery::Turn {
        turn_id: TurnId::from(turn),
        min_boundary,
    }
}

fn attempt(fence: &DriveFence) -> IngressClaimAttempt<'_> {
    IngressClaimAttempt {
        fence,
        now_epoch_ms: 10,
    }
}

fn claimed_seqs(plan: Option<IngressClaimPlan>) -> Vec<u64> {
    plan.map(|plan| {
        plan.into_claim()
            .items
            .iter()
            .map(|item| item.enqueue_seq)
            .collect()
    })
    .unwrap_or_default()
}

fn checkpoint(turn: &str, checkpoint: CheckpointKind) -> ClaimMode {
    ClaimMode::Checkpoint {
        turn_id: TurnId::from(turn),
        checkpoint,
    }
}

#[test]
fn an_idle_claim_takes_inputs_and_wakes_as_one_fifo_prefix() {
    let fence = fence();
    let candidates = [
        input(1, Delivery::NextTurn),
        wake(2),
        input(3, Delivery::AnyBoundary),
    ];
    let plan = plan_turn_claim(
        &attempt(&fence),
        ClaimMode::Idle,
        &IngressClaimPolicy::bounded(8),
        &candidates,
    )
    .expect("plan");
    assert_eq!(claimed_seqs(plan), vec![1, 2, 3]);
}

#[test]
fn a_checkpoint_prefix_stops_at_a_next_turn_row_and_never_skips_it() {
    let fence = fence();
    let candidates = [wake(1), input(2, Delivery::NextTurn), wake(3)];
    let plan = plan_turn_claim(
        &attempt(&fence),
        checkpoint("t", CheckpointKind::AfterWork),
        &IngressClaimPolicy::bounded(8),
        &candidates,
    )
    .expect("plan");
    assert_eq!(claimed_seqs(plan), vec![1]);
}

#[test]
fn a_checkpoint_takes_its_addressed_items_regardless_of_earlier_rows() {
    let fence = fence();
    let candidates = [
        input(1, Delivery::NextTurn),
        input(2, addressed("t", TurnInputCheckpointBoundary::AfterWork)),
        input(
            3,
            addressed("t", TurnInputCheckpointBoundary::BeforeCompletion),
        ),
        input(
            4,
            addressed("other", TurnInputCheckpointBoundary::AfterWork),
        ),
    ];
    let policy = IngressClaimPolicy::bounded(8);
    let after_work = plan_turn_claim(
        &attempt(&fence),
        checkpoint("t", CheckpointKind::AfterWork),
        &policy,
        &candidates,
    )
    .expect("plan");
    assert_eq!(claimed_seqs(after_work), vec![2]);
    let before_completion = plan_turn_claim(
        &attempt(&fence),
        checkpoint("t", CheckpointKind::BeforeCompletion),
        &policy,
        &candidates,
    )
    .expect("plan");
    assert_eq!(claimed_seqs(before_completion), vec![2, 3]);
}

#[test]
fn an_item_addressed_to_an_ended_turn_is_next_turn_at_its_own_position() {
    let fence = fence();
    let mut ended = input(
        1,
        addressed("ended", TurnInputCheckpointBoundary::AfterWork),
    );
    ended.addressed_turn_ended = true;
    let running = input(
        3,
        addressed("running", TurnInputCheckpointBoundary::AfterWork),
    );
    let candidates = [ended, wake(2), running, wake(4)];
    let policy = IngressClaimPolicy::bounded(8);
    let idle =
        plan_turn_claim(&attempt(&fence), ClaimMode::Idle, &policy, &candidates).expect("plan");
    assert_eq!(
        claimed_seqs(idle),
        vec![1, 2],
        "a running turn's item stops the prefix"
    );
    let at_checkpoint = plan_turn_claim(
        &attempt(&fence),
        checkpoint("t", CheckpointKind::AfterWork),
        &policy,
        &candidates,
    )
    .expect("plan");
    assert!(
        claimed_seqs(at_checkpoint).is_empty(),
        "an ended turn's item is NextTurn, which a checkpoint cannot deliver"
    );
}

#[test]
fn caps_and_bounds_are_stop_points_not_filters() {
    let fence = fence();
    let candidates = [
        input(1, Delivery::NextTurn),
        input(2, Delivery::NextTurn),
        wake(3),
        input(4, Delivery::NextTurn),
    ];
    let policy = IngressClaimPolicy {
        max_inputs: 1,
        ..IngressClaimPolicy::bounded(8)
    };
    let plan =
        plan_turn_claim(&attempt(&fence), ClaimMode::Idle, &policy, &candidates).expect("plan");
    assert_eq!(
        claimed_seqs(plan),
        vec![1],
        "the cap stops before row 2 and never skips to the wake"
    );
    let total = plan_turn_claim(
        &attempt(&fence),
        ClaimMode::Idle,
        &IngressClaimPolicy::bounded(3),
        &candidates,
    )
    .expect("plan");
    assert_eq!(claimed_seqs(total), vec![1, 2, 3]);
    let tokens = plan_turn_claim(
        &attempt(&fence),
        ClaimMode::Idle,
        &IngressClaimPolicy {
            available_tokens: 1,
            ..IngressClaimPolicy::bounded(8)
        },
        &candidates,
    )
    .expect("plan");
    assert_eq!(
        claimed_seqs(tokens),
        vec![1],
        "an oversized head is still claimed alone"
    );
    let aged = plan_turn_claim(
        &attempt(&fence),
        ClaimMode::Idle,
        &IngressClaimPolicy {
            max_pending_age_ms: 5,
            ..IngressClaimPolicy::bounded(8)
        },
        &candidates,
    )
    .expect("plan");
    assert_eq!(claimed_seqs(aged), vec![1], "an aged head is claimed alone");
}

fn held(
    mut candidate: IngressClaimCandidate,
    claim_id: &str,
    generation: u64,
) -> IngressClaimCandidate {
    candidate.claim = Some(IngressRowClaim {
        identity: IngressClaimIdentity {
            claim_id: claim_id.to_string(),
            claim_token: format!("{claim_id}-token"),
        },
        drive_epoch: generation,
        claim_turn_id: None,
    });
    candidate
}

#[test]
fn a_live_held_head_stops_the_claim() {
    let fence = fence();
    let candidates = [held(wake(1), "live", GENERATION), wake(2)];
    let plan = plan_turn_claim(
        &attempt(&fence),
        ClaimMode::Idle,
        &IngressClaimPolicy::bounded(8),
        &candidates,
    )
    .expect("plan");
    assert!(claimed_seqs(plan).is_empty());
}

#[test]
fn an_interrupted_idle_claim_is_rederived_exactly_without_the_policy() {
    let fence = fence();
    let candidates = [
        held(wake(1), "old", GENERATION - 1),
        wake(2),
        held(wake(3), "old", GENERATION - 1),
    ];
    let plan = plan_turn_claim(
        &attempt(&fence),
        ClaimMode::Idle,
        &IngressClaimPolicy::bounded(1),
        &candidates,
    )
    .expect("plan")
    .expect("the interrupted claim is re-derived");
    let claim = plan.into_claim();
    assert_eq!(
        claim
            .items
            .iter()
            .map(|item| item.enqueue_seq)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(
        claim
            .predecessor
            .as_ref()
            .map(|identity| identity.claim_id.as_str()),
        Some("old")
    );
}

#[test]
fn adjacent_config_patches_coalesce_and_other_commands_claim_alone() {
    let fence = fence();
    let patch = |seq: u64| {
        free(item(
            seq,
            &IngressItemDraft::session_command(
                session(),
                SessionCommand::ApplyConfigPatch {
                    patch: Box::new(crate::ApplyConfigPatch::default()),
                },
                format!("patch-{seq}"),
            ),
        ))
    };
    let coalesced = plan_command_claim(
        &attempt(&fence),
        &[patch(1), patch(2), refresh(3), patch(4)],
    )
    .expect("plan");
    assert_eq!(claimed_seqs(coalesced), vec![1, 2]);
    let alone = plan_command_claim(&attempt(&fence), &[refresh(1), refresh(2)]).expect("plan");
    assert_eq!(claimed_seqs(alone), vec![1]);
}

fn claimed_row(
    candidate: &IngressClaimCandidate,
    identity: &IngressClaimIdentity,
) -> IngressSettlementRow {
    let mut item = candidate.item.clone();
    item.state = IngressState::Accepted;
    IngressSettlementRow {
        item,
        claim: Some(identity.clone()),
        claim_epoch: Some(GENERATION),
    }
}

fn identity() -> IngressClaimIdentity {
    IngressClaimIdentity {
        claim_id: "claim".to_string(),
        claim_token: "token".to_string(),
    }
}

#[test]
fn a_turn_cancel_applies_its_disposition_only_by_author() {
    let addressed_input = input(1, addressed("t", TurnInputCheckpointBoundary::AfterWork));
    let held_wake = wake(2);
    let other_input = input(3, Delivery::AnyBoundary);
    let open_addressed = input(
        4,
        addressed("t", TurnInputCheckpointBoundary::BeforeCompletion),
    );
    let claim = identity();
    let observed = [
        claimed_row(&addressed_input, &claim),
        claimed_row(&held_wake, &claim),
        claimed_row(&other_input, &claim),
    ];
    let settlement = IngressClaimSettlement {
        session_id: session(),
        claims: vec![IngressClaimRef {
            identity: claim,
            item_ids: observed
                .iter()
                .map(|row| row.item.item_id.clone())
                .collect(),
        }],
        intent: IngressSettlementIntent::Turn {
            delivered: Vec::new(),
            cancel: Some(IngressTurnCancel {
                turn_id: TurnId::from("t"),
                request_id: "request".to_string(),
                mode: TurnCancelMode::Immediate,
                undelivered: TurnCancelDisposition::Drop,
            }),
        },
    };
    let addressed_rows = [IngressSettlementRow {
        item: open_addressed.item.clone(),
        claim: None,
        claim_epoch: None,
    }];
    let plan =
        plan_ingress_settlement(&settlement, &fence(), &observed, &addressed_rows).expect("plan");
    let dispositions = plan
        .affected
        .iter()
        .map(|record| (record.enqueue_seq, record.disposition))
        .collect::<Vec<_>>();
    assert_eq!(
        dispositions,
        vec![
            (1, IngressUndeliveredDisposition::Drop),
            (2, IngressUndeliveredDisposition::Defer),
            (3, IngressUndeliveredDisposition::Defer),
            (4, IngressUndeliveredDisposition::Drop),
        ],
        "every affected item is recorded; only host input addressed to the turn is dropped"
    );
    assert!(
        plan.floor_raises.is_empty(),
        "a deferred wake never touches the floor"
    );
    assert!(plan.writes.contains(&IngressRowSettlement::Release {
        item_id: held_wake.item.item_id.clone()
    }));
}

#[test]
fn delivered_wakes_complete_and_raise_the_floor_and_the_rest_defer() {
    let delivered_wake = wake(1);
    let undelivered = input(2, Delivery::AnyBoundary);
    let claim = identity();
    let observed = [
        claimed_row(&delivered_wake, &claim),
        claimed_row(&undelivered, &claim),
    ];
    let settlement = IngressClaimSettlement {
        session_id: session(),
        claims: vec![IngressClaimRef {
            identity: claim,
            item_ids: observed
                .iter()
                .map(|row| row.item.item_id.clone())
                .collect(),
        }],
        intent: IngressSettlementIntent::Turn {
            delivered: vec![delivered_wake.item.item_id.clone()],
            cancel: None,
        },
    };
    let plan = plan_ingress_settlement(&settlement, &fence(), &observed, &[]).expect("plan");
    assert_eq!(plan.floor_raises, vec![(ProcessId::from("process"), 1)]);
    assert_eq!(
        plan.writes,
        vec![
            IngressRowSettlement::Complete {
                item_id: delivered_wake.item.item_id.clone(),
                cause: IngressTerminalCause::Delivered,
                state: IngressState::Completed,
            },
            IngressRowSettlement::Release {
                item_id: undelivered.item.item_id.clone(),
            },
        ]
    );
    assert!(plan.affected.is_empty());
}

#[test]
fn completing_an_unclaimed_or_undelivered_item_is_refused() {
    let claimed = wake(1);
    let stranger = wake(2);
    let claim = identity();
    let observed = [claimed_row(&claimed, &claim)];
    let settlement = IngressClaimSettlement {
        session_id: session(),
        claims: vec![IngressClaimRef {
            identity: claim.clone(),
            item_ids: vec![claimed.item.item_id.clone()],
        }],
        intent: IngressSettlementIntent::Turn {
            delivered: vec![stranger.item.item_id.clone()],
            cancel: None,
        },
    };
    assert!(matches!(
        plan_ingress_settlement(&settlement, &fence(), &observed, &[]),
        Err(StoreError::IngressSettlementRefused { .. })
    ));
    let command = refresh(3);
    let commands = IngressClaimSettlement {
        session_id: session(),
        claims: vec![IngressClaimRef {
            identity: claim.clone(),
            item_ids: vec![claimed.item.item_id.clone()],
        }],
        intent: IngressSettlementIntent::Commands {
            outcomes: vec![IngressCommandOutcome {
                item_id: command.item.item_id.clone(),
                result: crate::store::IngressCommandResult::Applied,
            }],
            refused_windows: Vec::new(),
        },
    };
    assert!(matches!(
        plan_ingress_settlement(&commands, &fence(), &observed, &[]),
        Err(StoreError::IngressSettlementRefused { .. })
    ));
}

#[test]
fn a_row_reclaimed_by_another_claim_supersedes_the_settlement() {
    let row = wake(1);
    let observed = [claimed_row(
        &row,
        &IngressClaimIdentity {
            claim_id: "successor".to_string(),
            claim_token: "successor-token".to_string(),
        },
    )];
    let settlement = IngressClaimSettlement {
        session_id: session(),
        claims: vec![IngressClaimRef {
            identity: identity(),
            item_ids: vec![row.item.item_id.clone()],
        }],
        intent: IngressSettlementIntent::Turn {
            delivered: vec![row.item.item_id.clone()],
            cancel: None,
        },
    };
    assert!(matches!(
        plan_ingress_settlement(&settlement, &fence(), &observed, &[]),
        Err(StoreError::IngressClaimSuperseded { .. })
    ));
}

#[test]
fn a_claim_of_a_superseded_epoch_settles_nothing() {
    let row = wake(1);
    let mut stale = claimed_row(&row, &identity());
    stale.claim_epoch = Some(GENERATION - 1);
    let settlement = IngressClaimSettlement {
        session_id: session(),
        claims: vec![IngressClaimRef {
            identity: identity(),
            item_ids: vec![row.item.item_id.clone()],
        }],
        intent: IngressSettlementIntent::Turn {
            delivered: vec![row.item.item_id.clone()],
            cancel: None,
        },
    };
    assert!(matches!(
        plan_ingress_settlement(&settlement, &fence(), &[stale], &[]),
        Err(StoreError::StaleDriveFence { .. })
    ));
}

#[test]
fn a_settlement_naming_part_of_a_claim_is_refused() {
    let first = wake(1);
    let second = wake(2);
    let claim = identity();
    let observed = [claimed_row(&first, &claim), claimed_row(&second, &claim)];
    let settlement = IngressClaimSettlement {
        session_id: session(),
        claims: vec![IngressClaimRef {
            identity: claim,
            item_ids: vec![first.item.item_id.clone()],
        }],
        intent: IngressSettlementIntent::Turn {
            delivered: vec![first.item.item_id.clone()],
            cancel: None,
        },
    };
    assert!(matches!(
        plan_ingress_settlement(&settlement, &fence(), &observed, &[]),
        Err(StoreError::IngressSettlementRefused { .. })
    ));
}

#[test]
fn a_turn_cancel_reaches_addressed_rows_an_interrupted_claim_holds() {
    let interrupted = input(1, addressed("t", TurnInputCheckpointBoundary::AfterWork));
    let live = input(2, addressed("t", TurnInputCheckpointBoundary::AfterWork));
    let mut interrupted_row = claimed_row(&interrupted, &identity());
    interrupted_row.claim_epoch = Some(GENERATION - 1);
    let live_row = claimed_row(
        &live,
        &IngressClaimIdentity {
            claim_id: "live".to_string(),
            claim_token: "live-token".to_string(),
        },
    );
    let settlement = IngressClaimSettlement {
        session_id: session(),
        claims: Vec::new(),
        intent: IngressSettlementIntent::Turn {
            delivered: Vec::new(),
            cancel: Some(IngressTurnCancel {
                turn_id: TurnId::from("t"),
                request_id: "request".to_string(),
                mode: TurnCancelMode::Immediate,
                undelivered: TurnCancelDisposition::Drop,
            }),
        },
    };
    let plan = plan_ingress_settlement(&settlement, &fence(), &[], &[interrupted_row, live_row])
        .expect("plan");
    assert_eq!(
        plan.affected
            .iter()
            .map(|record| (record.enqueue_seq, record.disposition))
            .collect::<Vec<_>>(),
        vec![(1, IngressUndeliveredDisposition::Drop)],
        "the interrupted hold is dropped and recorded; a live claim's row is its own"
    );
    assert!(matches!(
        plan.writes.as_slice(),
        [IngressRowSettlement::Drop { item_id, .. }] if item_id == &interrupted.item.item_id
    ));
}

#[test]
fn a_withdrawal_tombstones_unless_a_live_claim_holds_the_item() {
    let row = wake(4).item;
    assert!(matches!(
        plan_withdrawal(
            &row,
            Some(GENERATION),
            GENERATION,
            IngressWithdrawSelector::ItemId
        ),
        IngressWithdrawDecision::Held
    ));
    match plan_withdrawal(
        &row,
        Some(GENERATION - 1),
        GENERATION,
        IngressWithdrawSelector::ItemId,
    ) {
        IngressWithdrawDecision::Withdraw {
            floor_raise,
            record,
            ..
        } => {
            assert_eq!(floor_raise, Some((ProcessId::from("process"), 4)));
            assert_eq!(record.disposition, IngressUndeliveredDisposition::Drop);
        }
        other => panic!("an interrupted hold never blocks a withdrawal: {other:?}"),
    }
}

#[test]
fn a_wake_digest_covers_the_process_fact_not_the_delivery() {
    let wake = IngressItemDraft::process_wake(wake_delivery(3));
    let IngressPayload::ProcessWake { wake: delivery } = wake.payload().clone() else {
        panic!("wake payload");
    };
    let mut redelivered = delivery.clone();
    redelivered.input = "different".to_string();
    assert_ne!(
        wake.submission_digest().expect("digest"),
        IngressItemDraft::process_wake(*redelivered)
            .submission_digest()
            .expect("digest"),
        "a different process fact under the same key is a different submission"
    );
    assert_eq!(
        wake.submission_digest().expect("digest"),
        IngressItemDraft::process_wake(*delivery)
            .submission_digest()
            .expect("digest")
    );
}

#[test]
fn reserved_source_key_prefixes_are_kind_owned() {
    let host = |key: &str| {
        IngressItemDraft::input(session(), Delivery::NextTurn, TurnInput::text("x"))
            .with_source_key(key)
    };
    assert!(
        host("process:p:event:1:wake")
            .reserved_source_key_violation()
            .is_some()
    );
    assert!(
        host("command:apply_config_patch:k")
            .reserved_source_key_violation()
            .is_some()
    );
    assert!(
        host("process:p:event:1:ack")
            .reserved_source_key_violation()
            .is_none()
    );
    assert!(host("host-key").reserved_source_key_violation().is_none());
    assert!(
        IngressItemDraft::process_wake(wake_delivery(1))
            .reserved_source_key_violation()
            .is_none()
    );
}

#[test]
fn a_claim_renders_inputs_first_then_wakes_each_in_order() {
    let fence = fence();
    let candidates = [
        wake(1),
        input(2, Delivery::NextTurn),
        wake(3),
        input(4, Delivery::AnyBoundary),
    ];
    let claim = plan_turn_claim(
        &attempt(&fence),
        ClaimMode::Idle,
        &IngressClaimPolicy::bounded(8),
        &candidates,
    )
    .expect("plan")
    .expect("a claim")
    .into_claim();
    assert_eq!(
        claim
            .render_order()
            .iter()
            .map(|item| item.enqueue_seq)
            .collect::<Vec<_>>(),
        vec![2, 4, 1, 3],
        "host inputs first, then wake causes, each in enqueue order (ADR 0101 §6)"
    );
}
