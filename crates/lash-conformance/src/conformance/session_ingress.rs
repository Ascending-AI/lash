//! The one session ingress (ADR 0101 §16): the store laws every backend owes
//! over `SessionIngressStore`.
//!
//! These are the laws the store answers on its own — order, the FIFO prefix
//! and its stop points, the command lane, turn addressing, coalescing, dedup,
//! reserved prefixes, tombstones, the wake floor, cancel by author,
//! recomposition and fencing. Settlement runs through the conformance seam
//! [`StoreTestSupport::settle_session_ingress_for_testing`], which executes
//! the same planner and row writes the head commit will. The laws that need
//! the runtime driver (render order, delivery evidence at commit, commands at
//! boundaries only, config compare-and-set, the pending follow-on and the
//! cutover refusal) join when the ingress becomes a `RuntimePersistence`
//! component.

use std::sync::Arc;

use lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION;
use lash_core::store::{AdmissionId, DriveEpochSeal, DriveEpochStore, DriveFence};
use lash_core::store::{
    BeginQueuedRun, ClaimMode, Delivery, IngressClaim, IngressClaimPolicy, IngressClaimRef,
    IngressClaimSettlement, IngressCommandOutcome, IngressCommandResult, IngressEnqueueOutcome,
    IngressItem, IngressItemDraft, IngressKind, IngressReadStatus, IngressReclaimOutcome,
    IngressSettlementIntent, IngressSettlementReceipt, IngressState, IngressSuffixWithdrawOutcome,
    IngressTerminalCause, IngressTurnCancel, IngressUndeliveredDisposition, IngressWithdrawOutcome,
    IngressWithdrawTarget, QueuedRunRequest, SessionIngressStore, StoreTestSupport,
};
use lash_core::testing::store_fixtures::claim_session_execution_lease_for_test;
use lash_sansio::{SessionId, TurnId};

/// The session every ingress law runs in, on a fresh fixture per law.
pub const SESSION_INGRESS_SESSION_ID: &str = "session-ingress";

/// A backend's ingress store together with its test seams.
pub trait SessionIngressConformance:
    SessionIngressStore + DriveEpochStore + StoreTestSupport
{
}

impl<T: SessionIngressStore + DriveEpochStore + StoreTestSupport + ?Sized> SessionIngressConformance
    for T
{
}

/// The two handles an ingress law drives: the runtime store bound to
/// [`SESSION_INGRESS_SESSION_ID`], which owns leases, queued runs and turn
/// commits, and the ingress store over the same database.
#[derive(Clone)]
pub struct SessionIngressHandles {
    pub runtime: Arc<dyn crate::RuntimePersistence>,
    pub ingress: Arc<dyn SessionIngressConformance>,
}

/// The store-creation request the fixture opens [`SESSION_INGRESS_SESSION_ID`]
/// with.
#[must_use]
pub fn session_ingress_session_request() -> crate::SessionStoreCreateRequest {
    crate::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: session(),
        relation: crate::SessionRelation::Root,
        policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    }
}

pub(crate) fn session() -> SessionId {
    SessionId::from(SESSION_INGRESS_SESSION_ID)
}

fn wake_delivery(process: &str, sequence: u64, text: &str) -> crate::ProcessWakeDelivery {
    crate::ProcessWakeDelivery {
        version: crate::FleetFormat::current().writer_version(lash_core::surface_format!(
            PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
        wake_id: format!("{process}-wake-{sequence}"),
        target_session_id: session(),
        process_id: crate::ProcessId::fixture(process),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: crate::RuntimeInvocation {
            attribution: crate::RuntimeAttribution::for_session(SESSION_INGRESS_SESSION_ID),
            subject: crate::RuntimeSubject::ProcessEvent {
                process_id: crate::ProcessId::fixture(process),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 1,
    }
}

pub(crate) fn input(text: &str) -> IngressItemDraft {
    IngressItemDraft::input(session(), Delivery::NextTurn, crate::TurnInput::text(text))
}

fn steer(text: &str) -> IngressItemDraft {
    IngressItemDraft::input(
        session(),
        Delivery::AnyBoundary,
        crate::TurnInput::text(text),
    )
}

fn addressed(
    turn_id: &TurnId,
    min_boundary: crate::TurnInputCheckpointBoundary,
    text: &str,
) -> IngressItemDraft {
    IngressItemDraft::input(
        session(),
        Delivery::Turn {
            turn_id: turn_id.clone(),
            min_boundary,
        },
        crate::TurnInput::text(text),
    )
}

pub(crate) fn wake(process: &str, sequence: u64) -> IngressItemDraft {
    IngressItemDraft::process_wake(wake_delivery(process, sequence, "wake"))
}

fn refresh(key: &str) -> IngressItemDraft {
    IngressItemDraft::session_command(
        session(),
        crate::SessionCommand::RefreshToolCatalog {
            reason: format!("refresh {key}"),
        },
        key,
    )
}

pub(crate) fn patch(key: &str) -> IngressItemDraft {
    IngressItemDraft::session_command(
        session(),
        crate::SessionCommand::ApplyConfigPatch {
            patch: Box::new(crate::ApplyConfigPatch::default()),
        },
        key,
    )
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(crate) async fn admit(handles: &SessionIngressHandles, draft: IngressItemDraft) -> IngressItem {
    match handles
        .ingress
        .enqueue_ingress_item(draft)
        .await
        .expect("admit an ingress item")
    {
        IngressEnqueueOutcome::Inserted(item) => item,
        other => panic!("a fresh submission is inserted, got {other:?}"),
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(crate) async fn replay(
    handles: &SessionIngressHandles,
    draft: IngressItemDraft,
) -> IngressEnqueueOutcome {
    handles
        .ingress
        .enqueue_ingress_item(draft)
        .await
        .expect("replay an ingress submission")
}

/// The runtime session-execution lease the turn helpers commit and admit
/// queued runs under. Ingress operations never use it: they are fenced by the
/// drive epoch.
pub(crate) async fn runtime_lease(
    handles: &SessionIngressHandles,
    owner: &str,
) -> crate::SessionExecutionLease {
    claim_session_execution_lease_for_test(&handles.runtime, &session(), owner).await
}

/// The start marker the store-level laws seal under: one execution per
/// admission (ADR 0105 L-S8).
fn root_start() -> lash_core::store::RootStartNonce {
    lash_core::store::RootStartNonce::new("conformance-root-start")
}

/// Seal the next drive epoch under a fresh admission and return its fence,
/// as the engine's admission seal would.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(crate) async fn seal(handles: &SessionIngressHandles, admission: &str) -> DriveFence {
    let observed = handles
        .ingress
        .drive_epoch(&session())
        .await
        .expect("read the drive epoch")
        .epoch;
    match handles
        .ingress
        .seal_drive_epoch(
            &session(),
            &AdmissionId::new(admission),
            observed,
            &root_start(),
        )
        .await
        .expect("seal a drive epoch")
    {
        DriveEpochSeal::Sealed(fence) => {
            assert_eq!(
                fence.epoch(),
                observed + 1,
                "a seal raises the epoch by one"
            );
            fence
        }
        other => panic!("a seal at the observed epoch is granted, got {other:?}"),
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn claim(
    handles: &SessionIngressHandles,
    fence: &DriveFence,
    mode: ClaimMode,
    policy: IngressClaimPolicy,
) -> Option<IngressClaim> {
    handles
        .ingress
        .claim_turn_items(fence, mode, &policy)
        .await
        .expect("claim the turn lane")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(crate) async fn claim_commands(
    handles: &SessionIngressHandles,
    fence: &DriveFence,
) -> Option<IngressClaim> {
    handles
        .ingress
        .claim_session_commands(fence)
        .await
        .expect("claim the command lane")
}

fn seqs(claim: Option<&IngressClaim>) -> Vec<u64> {
    claim
        .map(|claim| claim.items.iter().map(|item| item.enqueue_seq).collect())
        .unwrap_or_default()
}

fn checkpoint(turn_id: &TurnId, checkpoint: crate::CheckpointKind) -> ClaimMode {
    ClaimMode::Checkpoint {
        turn_id: turn_id.clone(),
        checkpoint,
    }
}

pub(crate) async fn settle(
    handles: &SessionIngressHandles,
    fence: &DriveFence,
    claims: &[&IngressClaim],
    intent: IngressSettlementIntent,
) -> Result<IngressSettlementReceipt, crate::StoreError> {
    handles
        .ingress
        .settle_session_ingress_for_testing(
            fence,
            IngressClaimSettlement {
                session_id: session(),
                claims: claims
                    .iter()
                    .map(|claim| IngressClaimRef::of(claim))
                    .collect(),
                intent,
            },
        )
        .await
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn deliver(
    handles: &SessionIngressHandles,
    fence: &DriveFence,
    claim: &IngressClaim,
    delivered: &[&IngressItem],
) {
    settle(
        handles,
        fence,
        &[claim],
        IngressSettlementIntent::Turn {
            delivered: delivered.iter().map(|item| item.item_id.clone()).collect(),
            cancel: None,
        },
    )
    .await
    .expect("settle a turn's claim");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(crate) async fn rows(handles: &SessionIngressHandles) -> Vec<IngressItem> {
    handles
        .ingress
        .session_ingress_rows_for_testing(&session())
        .await
        .expect("read every ingress row")
}

pub(crate) fn row<'a>(rows: &'a [IngressItem], item: &IngressItem) -> &'a IngressItem {
    rows.iter()
        .find(|row| row.item_id == item.item_id)
        .unwrap_or_else(|| panic!("row `{}` is present", item.item_id))
}

/// Turn commits of the ingress session, tracking the head revision and the
/// one running logical run.
pub(crate) struct Turns {
    pub(crate) state: crate::RuntimeSessionState,
    running: Option<(crate::ExecutionScope, TurnId)>,
}

impl Turns {
    pub(crate) fn new() -> Self {
        Self {
            state: crate::RuntimeSessionState {
                session_id: session(),
                ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                ))
            },
            running: None,
        }
    }

    /// Record `turn_id`'s final commit under the held `lease`.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn end(
        &mut self,
        handles: &SessionIngressHandles,
        lease: &crate::SessionExecutionLease,
        turn_id: &TurnId,
    ) {
        let mut commit = crate::RuntimeCommit::persisted_state_for_test(&self.state, &[]);
        commit.turn_commit = crate::RuntimeTurnCommitStamp::new(crate::OperationId::turn(
            session(),
            turn_id,
            "final",
        ));
        commit.session_execution_lease_fence = Some(lease.authority());
        if let Some((scope, _)) = self.running.take_if(|(_, running)| running == turn_id) {
            commit.queued_run = Some(Box::new(lash_core::store::QueuedRunCommit {
                scope,
                expected_revision: 0,
                progress: lash_core::store::QueuedRunProgress::Settle {
                    terminal: lash_core::store::QueuedRunTerminal::Completed {
                        turn_id: turn_id.clone(),
                        outcome: crate::TurnOutcome::Stopped(crate::TurnStop::ToolFailure),
                    },
                },
            }));
        }
        let receipt = handles
            .runtime
            .commit_runtime_state(commit)
            .await
            .expect("record the turn's final commit");
        self.state.head_revision = receipt.head_revision;
    }

    /// Admit a logical run whose current physical turn is running, and
    /// return that turn.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn start(
        &mut self,
        handles: &SessionIngressHandles,
        lease: &crate::SessionExecutionLease,
        drain: &str,
    ) -> TurnId {
        let scope = crate::ExecutionScope::queue_drain(session(), drain);
        let turn_id = handles
            .runtime
            .begin_or_resume_queued_run(
                &lease.authority(),
                BeginQueuedRun {
                    session_id: session(),
                    identity: Some(scope.clone()),
                    request: QueuedRunRequest::Automatic,
                    configuration: crate::RuntimeCommit::persisted_state_for_test(&self.state, &[])
                        .config,
                    expected_head_revision: self.state.head_revision,
                    initial_turn_index: 1,
                    generation: None,
                },
            )
            .await
            .expect("admit a running logical run")
            .position
            .turn_id;
        self.running = Some((scope, turn_id.clone()));
        turn_id
    }
}

/// Law 1 (order): one `enqueue_seq` strictly increasing across every kind,
/// in enqueue order, and the list read reports `(lane, enqueue_seq)`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn ingress_order_is_one_sequence_across_kinds(handles: SessionIngressHandles) {
    let first = admit(&handles, input("first")).await;
    let wake = admit(&handles, wake("process", 1)).await;
    let command = admit(&handles, refresh("refresh")).await;
    let last = admit(&handles, steer("last")).await;
    let order = [&first, &wake, &command, &last];
    assert!(
        order
            .windows(2)
            .all(|pair| pair[0].enqueue_seq < pair[1].enqueue_seq),
        "one sequence, strictly increasing in enqueue order across kinds"
    );
    assert_eq!(command.kind().lane(), crate::IngressLane::Command);
    let listed = handles
        .ingress
        .list_ingress_items(&session())
        .await
        .expect("list the ingress");
    assert_eq!(
        listed
            .iter()
            .map(|read| read.item.item_id.clone())
            .collect::<Vec<_>>(),
        vec![
            command.item_id.clone(),
            first.item_id.clone(),
            wake.item_id.clone(),
            last.item_id.clone()
        ],
        "the list reads (lane, enqueue_seq), command lane first"
    );
    assert!(
        listed
            .iter()
            .all(|read| read.status == IngressReadStatus::Pending)
    );
}

/// Law 2 (FIFO prefix): an idle claim is a prefix of the open turn-lane rows,
/// inputs and wakes together, and a later claim continues where it stopped.
pub async fn idle_claim_takes_the_fifo_prefix(handles: SessionIngressHandles) {
    let one = admit(&handles, input("one")).await;
    let two = admit(&handles, wake("process", 1)).await;
    let three = admit(&handles, input("three")).await;
    let fence = seal(&handles, "fifo").await;
    let first = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(2),
    )
    .await;
    assert_eq!(seqs(first.as_ref()), vec![one.enqueue_seq, two.enqueue_seq]);
    let first = first.unwrap_or_else(|| panic!("a claim"));
    deliver(&handles, &fence, &first, &[&one, &two]).await;
    let second = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(2),
    )
    .await;
    assert_eq!(seqs(second.as_ref()), vec![three.enqueue_seq]);
}

/// Law 3 (stop points): the prefix ends at the first delivery mismatch, the
/// first per-kind cap, the total bound and a held head row, and never
/// continues past any of them.
pub async fn turn_claim_stops_and_never_skips(handles: SessionIngressHandles) {
    let wake_one = admit(&handles, wake("process", 1)).await;
    let next_turn = admit(&handles, input("next turn")).await;
    let wake_three = admit(&handles, wake("process", 3)).await;
    let input_four = admit(&handles, input("four")).await;
    let fence = seal(&handles, "stops").await;
    let running = TurnId::from("running-turn");

    let at_checkpoint = claim(
        &handles,
        &fence,
        checkpoint(&running, crate::CheckpointKind::AfterWork),
        IngressClaimPolicy::bounded(8),
    )
    .await;
    assert_eq!(
        seqs(at_checkpoint.as_ref()),
        vec![wake_one.enqueue_seq],
        "a checkpoint stops at the first NextTurn row and never skips to the wake after it"
    );
    let at_checkpoint = at_checkpoint.unwrap_or_else(|| panic!("a claim"));
    assert!(
        claim(
            &handles,
            &fence,
            ClaimMode::Idle,
            IngressClaimPolicy::bounded(8)
        )
        .await
        .is_none(),
        "a held head row stops an idle claim; it never passes it"
    );
    settle(
        &handles,
        &fence,
        &[&at_checkpoint],
        IngressSettlementIntent::Turn {
            delivered: Vec::new(),
            cancel: None,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("release the checkpoint claim: {error}"));

    let capped = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy {
            max_wakes: 1,
            ..IngressClaimPolicy::bounded(8)
        },
    )
    .await;
    assert_eq!(
        seqs(capped.as_ref()),
        vec![wake_one.enqueue_seq, next_turn.enqueue_seq],
        "the wake cap is a stop point inside the prefix, not a filter"
    );
    let capped = capped.unwrap_or_else(|| panic!("a claim"));
    deliver(&handles, &fence, &capped, &[&wake_one, &next_turn]).await;
    let bounded = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(1),
    )
    .await;
    assert_eq!(
        seqs(bounded.as_ref()),
        vec![wake_three.enqueue_seq],
        "the total bound stops the prefix"
    );
    let bounded = bounded.unwrap_or_else(|| panic!("a claim"));
    deliver(&handles, &fence, &bounded, &[&wake_three]).await;
    let rest = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await;
    assert_eq!(seqs(rest.as_ref()), vec![input_four.enqueue_seq]);
}

/// Inputs, commands, wakes and ingress rows share the session's allocation counter.
pub async fn every_ingress_producer_shares_the_session_sequence(handles: SessionIngressHandles) {
    let first = handles
        .runtime
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            session(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("legacy input"),
        ))
        .await
        .unwrap_or_else(|error| panic!("enqueue input: {error}"));
    let command = handles
        .runtime
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            session(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::SessionCommand::RefreshToolCatalog {
                reason: "legacy command".into(),
            },
        ))
        .await
        .unwrap_or_else(|error| panic!("enqueue command: {error}"));
    let wake = handles
        .runtime
        .enqueue_queued_work(crate::runtime::process_wake_batch_draft(wake_delivery(
            "legacy-process",
            1,
            "legacy wake",
        )))
        .await
        .unwrap_or_else(|error| panic!("enqueue wake: {error}"));
    let input = admit(&handles, input("ingress input")).await;
    let next = admit(&handles, refresh("ingress command")).await;
    assert_eq!(
        [
            first.enqueue_seq,
            command.enqueue_seq,
            wake.enqueue_seq,
            input.enqueue_seq,
            next.enqueue_seq
        ],
        [1, 2, 3, 4, 5]
    );
    let unrelated = handles
        .runtime
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            SessionId::from("another-session"),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("unrelated"),
        ))
        .await
        .unwrap_or_else(|error| panic!("enqueue unrelated input: {error}"));
    assert_eq!(unrelated.enqueue_seq, 1, "sessions allocate independently");
}

/// At an idle boundary every command settles before any turn-lane claim.
pub async fn command_lane_drains_before_idle_turn_claims(handles: SessionIngressHandles) {
    let early_input = admit(&handles, input("early input")).await;
    let first_command = admit(&handles, refresh("first")).await;
    let late_input = admit(&handles, input("late input")).await;
    let second_command = admit(&handles, refresh("second")).await;
    let fence = seal(&handles, "lanes").await;
    for command in [&first_command, &second_command] {
        assert!(
            claim(
                &handles,
                &fence,
                ClaimMode::Idle,
                IngressClaimPolicy::bounded(8)
            )
            .await
            .is_none(),
            "every pending command must apply before the idle turn claim"
        );
        let held = claim_commands(&handles, &fence)
            .await
            .unwrap_or_else(|| panic!("a command claim"));
        assert_eq!(seqs(Some(&held)), vec![command.enqueue_seq]);
        settle(
            &handles,
            &fence,
            &[&held],
            IngressSettlementIntent::Commands {
                outcomes: vec![IngressCommandOutcome {
                    item_id: command.item_id.clone(),
                    result: IngressCommandResult::Applied,
                }],
                refused_windows: Vec::new(),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("apply the command: {error}"));
    }
    let turn = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await;
    assert_eq!(
        seqs(turn.as_ref()),
        vec![early_input.enqueue_seq, late_input.enqueue_seq]
    );
    let turn = turn.unwrap_or_else(|| panic!("a turn claim"));
    deliver(&handles, &fence, &turn, &[&early_input, &late_input]).await;
    admit(&handles, refresh("checkpoint command")).await;
    let wake = admit(&handles, wake("checkpoint wake", 1)).await;
    let checkpoint_claim = claim(
        &handles,
        &fence,
        checkpoint(&TurnId::from("t"), crate::CheckpointKind::BeforeCompletion),
        IngressClaimPolicy::bounded(8),
    )
    .await;
    assert_eq!(
        seqs(checkpoint_claim.as_ref()),
        vec![wake.enqueue_seq],
        "a checkpoint ignores pending commands"
    );
}

/// Law 5 (addressed items): a turn address is admitted only for the running
/// turn or an ended one, an unknown address writes nothing, a `Turn{t}` item
/// is claimable at t's admitting checkpoints regardless of earlier rows and
/// never elsewhere while t runs, and after t ends it is claimed exactly where
/// a `NextTurn` item at its position would be, its delivery never rewritten.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn turn_addressed_items_follow_their_turn(handles: SessionIngressHandles) {
    let fence = seal(&handles, "addressed").await;
    let lease = runtime_lease(&handles, "addressed").await;
    let mut turns = Turns::new();
    let unknown = handles
        .ingress
        .enqueue_ingress_item(addressed(
            &TurnId::from("never-ran"),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "lost",
        ))
        .await;
    assert!(
        matches!(
            unknown,
            Err(crate::StoreError::IngressTurnAddressUnknown { .. })
        ),
        "an unknown turn address is refused: {unknown:?}"
    );
    assert!(
        rows(&handles).await.is_empty(),
        "no row, tombstone or sequence number"
    );

    let running = turns.start(&handles, &lease, "addressed-drain").await;
    let earlier = admit(&handles, input("earlier")).await;
    let for_running = admit(
        &handles,
        addressed(
            &running,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "steer",
        ),
    )
    .await;
    let later = admit(&handles, input("later")).await;
    assert!(
        claim(
            &handles,
            &fence,
            ClaimMode::Idle,
            IngressClaimPolicy::bounded(8)
        )
        .await
        .is_some_and(|claim| claim.items.len() == 1 && claim.items[0].item_id == earlier.item_id),
        "a running turn's item stops the idle prefix"
    );
    let rows_now = rows(&handles).await;
    let held_earlier = row(&rows_now, &earlier);
    assert_eq!(held_earlier.state, IngressState::Accepted);
    let checkpoint_claim = claim(
        &handles,
        &fence,
        checkpoint(&running, crate::CheckpointKind::AfterWork),
        IngressClaimPolicy::bounded(8),
    )
    .await;
    assert_eq!(
        seqs(checkpoint_claim.as_ref()),
        vec![for_running.enqueue_seq],
        "the checkpoint takes its addressed item regardless of earlier rows"
    );
    let checkpoint_claim = checkpoint_claim.expect("a checkpoint claim");
    settle(
        &handles,
        &fence,
        &[&checkpoint_claim],
        IngressSettlementIntent::Turn {
            delivered: Vec::new(),
            cancel: None,
        },
    )
    .await
    .expect("release the addressed item undelivered");
    turns.end(&handles, &lease, &running).await;
    let after = rows(&handles).await;
    assert_eq!(
        row(&after, &for_running).delivery,
        Delivery::Turn {
            turn_id: running.clone(),
            min_boundary: crate::TurnInputCheckpointBoundary::AfterWork,
        },
        "no write after admission changes a row's delivery"
    );
    let ended_admitted = admit(
        &handles,
        addressed(
            &running,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "late steer",
        ),
    )
    .await;
    let fence = seal(&handles, "addressed-successor").await;
    let idle = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await;
    assert_eq!(
        seqs(idle.as_ref()),
        vec![earlier.enqueue_seq],
        "the interrupted idle claim at the head is re-derived exactly"
    );
    let idle = idle.expect("the re-derived claim");
    deliver(&handles, &fence, &idle, &[&earlier]).await;
    let next = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await;
    assert_eq!(
        seqs(next.as_ref()),
        vec![
            for_running.enqueue_seq,
            later.enqueue_seq,
            ended_admitted.enqueue_seq
        ],
        "an ended turn's items are NextTurn at their own positions"
    );
}

/// Law 6 (coalescing), as a store claim law: adjacent config patches share
/// one command claim; any other command is claimed alone.
pub async fn adjacent_config_patches_share_one_command_claim(handles: SessionIngressHandles) {
    let first = admit(&handles, patch("first")).await;
    let second = admit(&handles, patch("second")).await;
    let refresh = admit(&handles, refresh("between")).await;
    let third = admit(&handles, patch("third")).await;
    let fence = seal(&handles, "coalesce").await;
    let mut expected = vec![
        vec![first.clone(), second.clone()],
        vec![refresh.clone()],
        vec![third.clone()],
    ]
    .into_iter();
    while let Some(claim) = claim_commands(&handles, &fence).await {
        let group = expected
            .next()
            .unwrap_or_else(|| panic!("an unexpected command claim {:?}", seqs(Some(&claim))));
        assert_eq!(
            seqs(Some(&claim)),
            group
                .iter()
                .map(|item| item.enqueue_seq)
                .collect::<Vec<_>>()
        );
        settle(
            &handles,
            &fence,
            &[&claim],
            IngressSettlementIntent::Commands {
                outcomes: group
                    .iter()
                    .map(|item| IngressCommandOutcome {
                        item_id: item.item_id.clone(),
                        result: IngressCommandResult::Applied,
                    })
                    .collect(),
                refused_windows: Vec::new(),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("apply the commands: {error}"));
    }
    assert!(expected.next().is_none(), "every command group was claimed");
}

/// Law 12 (dedup): the same key and digest is `Existing` while the row or its
/// tombstone exists; a different digest is a typed `Conflict` for every kind;
/// an identical retry after its addressed turn ended is `Existing`.
pub async fn replay_compares_the_immutable_digest_for_every_kind(handles: SessionIngressHandles) {
    let fence = seal(&handles, "dedup").await;
    let lease = runtime_lease(&handles, "dedup").await;
    let mut turns = Turns::new();
    let keyed = admit(&handles, input("keyed").with_source_key("host-key")).await;
    assert!(matches!(
        replay(&handles, input("keyed").with_source_key("host-key")).await,
        IngressEnqueueOutcome::Existing(item) if item.item_id == keyed.item_id
    ));
    assert!(matches!(
        replay(&handles, input("changed").with_source_key("host-key")).await,
        IngressEnqueueOutcome::Conflict { existing_item_id } if existing_item_id == keyed.item_id
    ));
    let provisioned = admit(
        &handles,
        input("provisioned").with_item_id("ti:provisioned"),
    )
    .await;
    assert!(matches!(
        replay(&handles, input("provisioned").with_item_id("ti:provisioned")).await,
        IngressEnqueueOutcome::Existing(item) if item.item_id == provisioned.item_id
    ));
    assert!(matches!(
        replay(&handles, input("other").with_item_id("ti:provisioned")).await,
        IngressEnqueueOutcome::Conflict { .. }
    ));
    let command = admit(&handles, refresh("command")).await;
    assert!(matches!(
        replay(&handles, refresh("command")).await,
        IngressEnqueueOutcome::Existing(item) if item.item_id == command.item_id
    ));
    assert!(matches!(
        replay(
            &handles,
            IngressItemDraft::session_command(
                session(),
                crate::SessionCommand::RefreshToolCatalog {
                    reason: "different".to_string(),
                },
                "command",
            )
        )
        .await,
        IngressEnqueueOutcome::Conflict { .. }
    ));
    let wake_row = admit(&handles, wake("process", 4)).await;
    assert!(matches!(
        replay(&handles, wake("process", 4)).await,
        IngressEnqueueOutcome::Existing(item) if item.item_id == wake_row.item_id
    ));
    assert!(matches!(
        replay(
            &handles,
            IngressItemDraft::process_wake(wake_delivery("process", 4, "different fact"))
        )
        .await,
        IngressEnqueueOutcome::Conflict { .. }
    ));

    let running = turns.start(&handles, &lease, "dedup-drain").await;
    let draft = || {
        addressed(
            &running,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "addressed",
        )
        .with_source_key("addressed-key")
    };
    let addressed_row = admit(&handles, draft()).await;
    turns.end(&handles, &lease, &running).await;
    assert!(
        matches!(
            replay(&handles, draft()).await,
            IngressEnqueueOutcome::Existing(item) if item.item_id == addressed_row.item_id
        ),
        "an identical retry after its addressed turn ended is Existing"
    );

    let command_claim = claim_commands(&handles, &fence)
        .await
        .unwrap_or_else(|| panic!("the command claim"));
    settle(
        &handles,
        &fence,
        &[&command_claim],
        IngressSettlementIntent::Commands {
            outcomes: vec![IngressCommandOutcome {
                item_id: command.item_id.clone(),
                result: IngressCommandResult::Applied,
            }],
            refused_windows: Vec::new(),
        },
    )
    .await
    .unwrap_or_else(|error| panic!("settle before the turn claim: {error}"));
    let claimed = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(1),
    )
    .await
    .unwrap_or_else(|| panic!("a claim"));
    let head = claimed.items[0].clone();
    deliver(&handles, &fence, &claimed, &[&head]).await;
    assert!(
        matches!(
            replay(&handles, input("keyed").with_source_key("host-key")).await,
            IngressEnqueueOutcome::Existing(item)
                if item.item_id == keyed.item_id && item.state == IngressState::Completed
        ),
        "a replay meets the tombstone"
    );
}

/// Law 13 (prefixes): a host input using a reserved source-key prefix is
/// refused at admission, and nothing is written.
pub async fn reserved_source_key_prefixes_are_refused(handles: SessionIngressHandles) {
    for key in ["process:p:event:1:wake", "command:refresh_tool_catalog:k"] {
        let refused = handles
            .ingress
            .enqueue_ingress_item(input("spoof").with_source_key(key))
            .await;
        assert!(
            matches!(
                refused,
                Err(crate::StoreError::IngressReservedSourceKey { .. })
            ),
            "`{key}` is reserved: {refused:?}"
        );
    }
    assert!(rows(&handles).await.is_empty());
    admit(
        &handles,
        input("ordinary").with_source_key("process:p:event:1:ack"),
    )
    .await;
}

/// Law 14 (tombstones): every terminal row survives until vacuum with a
/// closed cause, is never claimable, and a `cancelled` tombstone is never
/// reopened by enqueue; vacuum removes tombstones and nothing else.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn tombstones_stay_until_vacuum_and_never_reopen(handles: SessionIngressHandles) {
    let delivered = admit(&handles, input("delivered").with_source_key("delivered")).await;
    let withdrawn = admit(&handles, input("withdrawn").with_source_key("withdrawn")).await;
    let open = admit(&handles, input("open")).await;
    let fence = seal(&handles, "tombstones").await;
    let first = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(1),
    )
    .await
    .expect("a claim");
    deliver(&handles, &fence, &first, &[&delivered]).await;
    let receipts = handles
        .ingress
        .withdraw_ingress_items(
            &session(),
            &[IngressWithdrawTarget::SourceKey("withdrawn".to_string())],
        )
        .await
        .expect("withdraw");
    assert!(receipts[0].outcome.is_withdrawn());

    let all = rows(&handles).await;
    assert_eq!(
        row(&all, &delivered).terminal_cause,
        Some(IngressTerminalCause::Delivered)
    );
    assert!(matches!(
        row(&all, &withdrawn).terminal_cause,
        Some(IngressTerminalCause::Cancelled { .. })
    ));
    assert_eq!(row(&all, &withdrawn).state, IngressState::Cancelled);
    assert!(
        matches!(
            replay(&handles, input("withdrawn").with_source_key("withdrawn")).await,
            IngressEnqueueOutcome::Existing(item) if item.state == IngressState::Cancelled
        ),
        "a cancelled tombstone is never reopened"
    );
    let next = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await
    .expect("a claim");
    assert_eq!(
        seqs(Some(&next)),
        vec![open.enqueue_seq],
        "tombstones are never claimable"
    );
    let removed = handles
        .ingress
        .vacuum_session_ingress(&session())
        .await
        .expect("vacuum");
    assert_eq!(removed, 2);
    let remaining = rows(&handles).await;
    assert_eq!(
        remaining
            .iter()
            .map(|item| item.item_id.clone())
            .collect::<Vec<_>>(),
        vec![open.item_id.clone()]
    );
    assert!(
        matches!(
            replay(&handles, input("withdrawn").with_source_key("withdrawn")).await,
            IngressEnqueueOutcome::Inserted(_)
        ),
        "after vacuum the key admits a new row"
    );
}

/// Law 15 (floor): every wake terminal raises the floor in its transaction —
/// completion, host withdrawal and conflict discard — a deferral does not, a
/// redelivery at or below the floor is absorbed, and vacuum leaves nothing
/// the floor would not absorb.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn every_wake_terminal_raises_the_redelivery_floor(handles: SessionIngressHandles) {
    let fence = seal(&handles, "floor").await;
    let rewound = |outcome: &IngressEnqueueOutcome, expected: u64| matches!(outcome, IngressEnqueueOutcome::WakeRewound { floor, .. } if *floor == expected);

    // A deferral leaves the floor alone.
    let deferred = admit(&handles, wake("deferred", 7)).await;
    let claim_deferred = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await
    .expect("a claim");
    deliver(&handles, &fence, &claim_deferred, &[]).await;
    admit(&handles, wake("deferred", 6)).await;

    // Completion raises it.
    let claimed = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await
    .expect("a claim");
    let delivered: Vec<IngressItem> = claimed.items.clone();
    deliver(
        &handles,
        &fence,
        &claimed,
        &delivered.iter().collect::<Vec<_>>(),
    )
    .await;
    assert_eq!(
        delivered.len(),
        2,
        "both deferred-process wakes were claimed"
    );
    assert!(
        delivered
            .iter()
            .any(|item| item.item_id == deferred.item_id)
    );
    assert!(rewound(&replay(&handles, wake("deferred", 5)).await, 7));

    // A host withdrawal raises it and records the floor after.
    admit(&handles, wake("withdrawn", 9)).await;
    let receipts = handles
        .ingress
        .withdraw_ingress_items(
            &session(),
            &[IngressWithdrawTarget::SourceKey(
                crate::process_wake_source_key(&crate::ProcessId::fixture("withdrawn"), 9),
            )],
        )
        .await
        .expect("withdraw the wake");
    match &receipts[0].outcome {
        IngressWithdrawOutcome::Withdrawn(record) => {
            assert_eq!(record.fence_floor_after, Some(9));
            assert_eq!(record.disposition, IngressUndeliveredDisposition::Drop);
        }
        other => panic!("the wake is withdrawn: {other:?}"),
    }
    assert!(rewound(&replay(&handles, wake("withdrawn", 8)).await, 9));

    // A conflict discard raises it.
    admit(&handles, wake("conflict", 11)).await;
    assert!(matches!(
        replay(
            &handles,
            IngressItemDraft::process_wake(wake_delivery("conflict", 11, "another fact"))
        )
        .await,
        IngressEnqueueOutcome::Conflict { .. }
    ));
    assert!(rewound(&replay(&handles, wake("conflict", 10)).await, 11));

    // Before vacuum an identical redelivery meets its tombstone; after it,
    // the floor absorbs it.
    assert!(matches!(
        replay(&handles, wake("withdrawn", 9)).await,
        IngressEnqueueOutcome::Existing(_)
    ));
    handles
        .ingress
        .vacuum_session_ingress(&session())
        .await
        .expect("vacuum");
    assert!(rewound(&replay(&handles, wake("withdrawn", 9)).await, 9));
    assert!(rewound(&replay(&handles, wake("deferred", 7)).await, 7));
}

/// Law 16 (cancel by author): a turn cancel applies its disposition to the
/// host input addressed to the cancelled turn and to nothing else, defers
/// every wake it held with the floor unchanged, and records every affected
/// item; a host withdrawal of a wake tombstones it, raises the floor and
/// records it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_turn_cancel_disposes_by_author(handles: SessionIngressHandles) {
    let fence = seal(&handles, "cancel").await;
    let lease = runtime_lease(&handles, "cancel").await;
    let mut turns = Turns::new();
    let running = turns.start(&handles, &lease, "cancel-drain").await;
    let claimed_input = admit(
        &handles,
        addressed(
            &running,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "claimed",
        ),
    )
    .await;
    let held_wake = admit(&handles, wake("held", 3)).await;
    let open_addressed = admit(
        &handles,
        addressed(
            &running,
            crate::TurnInputCheckpointBoundary::BeforeCompletion,
            "open",
        ),
    )
    .await;
    let unrelated = admit(&handles, input("unrelated")).await;
    let turn_claim = claim(
        &handles,
        &fence,
        checkpoint(&running, crate::CheckpointKind::AfterWork),
        IngressClaimPolicy::bounded(8),
    )
    .await
    .expect("a checkpoint claim");
    assert_eq!(
        seqs(Some(&turn_claim)),
        vec![claimed_input.enqueue_seq, held_wake.enqueue_seq]
    );
    let receipt = settle(
        &handles,
        &fence,
        &[&turn_claim],
        IngressSettlementIntent::Turn {
            delivered: Vec::new(),
            cancel: Some(IngressTurnCancel {
                turn_id: running.clone(),
                request_id: "cancel-request".to_string(),
                mode: crate::TurnCancelMode::Immediate,
                undelivered: crate::TurnCancelDisposition::Drop,
            }),
        },
    )
    .await
    .expect("settle the cancelled turn");
    assert_eq!(
        receipt
            .affected
            .iter()
            .map(|record| (record.item_id.clone(), record.disposition))
            .collect::<Vec<_>>(),
        vec![
            (
                claimed_input.item_id.clone(),
                IngressUndeliveredDisposition::Drop
            ),
            (
                held_wake.item_id.clone(),
                IngressUndeliveredDisposition::Defer
            ),
            (
                open_addressed.item_id.clone(),
                IngressUndeliveredDisposition::Drop
            ),
        ],
        "every affected item is recorded; only input addressed to the turn is dropped"
    );
    let after = rows(&handles).await;
    assert_eq!(row(&after, &claimed_input).state, IngressState::Cancelled);
    assert_eq!(row(&after, &open_addressed).state, IngressState::Cancelled);
    assert_eq!(row(&after, &held_wake).state, IngressState::Open);
    assert_eq!(
        row(&after, &held_wake).enqueue_seq,
        held_wake.enqueue_seq,
        "a deferred wake keeps its position"
    );
    assert_eq!(row(&after, &unrelated).state, IngressState::Open);
    assert!(
        matches!(
            replay(&handles, wake("held", 2)).await,
            IngressEnqueueOutcome::Inserted(_)
        ),
        "a deferred wake leaves the floor unchanged"
    );
    let withdrawn = handles
        .ingress
        .withdraw_ingress_items(
            &session(),
            &[IngressWithdrawTarget::ItemId(held_wake.item_id.clone())],
        )
        .await
        .expect("withdraw the wake");
    match &withdrawn[0].outcome {
        IngressWithdrawOutcome::Withdrawn(record) => {
            assert_eq!(record.kind, IngressKind::ProcessWake);
            assert_eq!(record.fence_floor_after, Some(3));
        }
        other => panic!("the wake is withdrawn: {other:?}"),
    }
}

/// Law 16 (cancel by author), interrupted holds: a turn cancel reaches an
/// addressed item an interrupted claim of a superseded epoch still holds,
/// applies the request's disposition and records it, so no later claim
/// delivers what the host asked to drop.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_turn_cancel_reaches_rows_an_interrupted_claim_holds(handles: SessionIngressHandles) {
    let first = seal(&handles, "interrupted-cancel").await;
    let lease = runtime_lease(&handles, "interrupted-cancel").await;
    let mut turns = Turns::new();
    let running = turns
        .start(&handles, &lease, "interrupted-cancel-drain")
        .await;
    let held = admit(
        &handles,
        addressed(
            &running,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "held",
        ),
    )
    .await;
    let interrupted = claim(
        &handles,
        &first,
        checkpoint(&running, crate::CheckpointKind::AfterWork),
        IngressClaimPolicy::bounded(8),
    )
    .await
    .expect("a checkpoint claim");
    assert_eq!(seqs(Some(&interrupted)), vec![held.enqueue_seq]);
    let second = seal(&handles, "interrupted-cancel-redrive").await;
    let receipt = settle(
        &handles,
        &second,
        &[],
        IngressSettlementIntent::Turn {
            delivered: Vec::new(),
            cancel: Some(IngressTurnCancel {
                turn_id: running.clone(),
                request_id: "interrupted-cancel-request".to_string(),
                mode: crate::TurnCancelMode::Immediate,
                undelivered: crate::TurnCancelDisposition::Drop,
            }),
        },
    )
    .await
    .expect("settle the cancel under the current epoch");
    assert_eq!(
        receipt
            .affected
            .iter()
            .map(|record| (record.item_id.clone(), record.disposition))
            .collect::<Vec<_>>(),
        vec![(held.item_id.clone(), IngressUndeliveredDisposition::Drop)],
        "the interrupted hold is dropped and recorded"
    );
    assert_eq!(
        row(&rows(&handles).await, &held).state,
        IngressState::Cancelled
    );
    assert!(
        claim(
            &handles,
            &second,
            ClaimMode::Idle,
            IngressClaimPolicy::bounded(8)
        )
        .await
        .is_none(),
        "nothing delivers the dropped item"
    );
}

/// Two admissions sealing the same observed epoch at once are serialized by
/// the store: exactly one raises the epoch, and the other is superseded.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn concurrent_seals_serialize(handles: SessionIngressHandles) {
    let observed = handles
        .ingress
        .drive_epoch(&session())
        .await
        .expect("read the drive epoch")
        .epoch;
    let spawn_seal = |admission: &'static str| {
        let handles = handles.clone();
        tokio::spawn(async move {
            handles
                .ingress
                .seal_drive_epoch(
                    &session(),
                    &AdmissionId::new(admission),
                    observed,
                    &root_start(),
                )
                .await
                .expect("seal a drive epoch")
        })
    };
    let (left, right) = (spawn_seal("concurrent-a"), spawn_seal("concurrent-b"));
    let outcomes = [
        left.await.expect("join the first seal"),
        right.await.expect("join the second seal"),
    ];
    let winners = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            DriveEpochSeal::Sealed(fence) => Some(fence.clone()),
            DriveEpochSeal::Superseded { .. } | DriveEpochSeal::ExecutionLost => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(winners.len(), 1, "exactly one seal wins: {outcomes:?}");
    assert_eq!(winners[0].epoch(), observed + 1);
    assert!(
        outcomes.contains(&DriveEpochSeal::Superseded {
            epoch: observed + 1
        }),
        "the other seal is superseded at the winner's epoch: {outcomes:?}"
    );
    let stored = handles
        .ingress
        .drive_epoch(&session())
        .await
        .expect("read the drive epoch");
    assert_eq!(stored.epoch, observed + 1);
    assert_eq!(stored.admission.as_ref(), Some(winners[0].admission()));
}

/// Law 19 (recompose): a deferred multi-row claim returns row by row to its
/// own positions, and a row ready before a deferred member is claimed first.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_deferred_claim_is_recomposed_row_by_row(handles: SessionIngressHandles) {
    let fence = seal(&handles, "recompose").await;
    let lease = runtime_lease(&handles, "recompose").await;
    let mut turns = Turns::new();
    let running = turns.start(&handles, &lease, "recompose-drain").await;
    let first_ready = admit(
        &handles,
        addressed(
            &running,
            crate::TurnInputCheckpointBoundary::BeforeCompletion,
            "ready",
        ),
    )
    .await;
    let wake_one = admit(&handles, wake("recompose", 1)).await;
    let wake_two = admit(&handles, wake("recompose", 2)).await;
    let joined = admit(&handles, input("joined")).await;
    let turn_claim = claim(
        &handles,
        &fence,
        checkpoint(&running, crate::CheckpointKind::AfterWork),
        IngressClaimPolicy::bounded(8),
    )
    .await
    .expect("a checkpoint claim");
    assert_eq!(
        seqs(Some(&turn_claim)),
        vec![wake_one.enqueue_seq, wake_two.enqueue_seq]
    );
    deliver(&handles, &fence, &turn_claim, &[]).await;
    turns.end(&handles, &lease, &running).await;
    let single = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(1),
    )
    .await
    .expect("a claim");
    assert_eq!(
        seqs(Some(&single)),
        vec![first_ready.enqueue_seq],
        "the row ready before the deferred members is claimed first, alone"
    );
    deliver(&handles, &fence, &single, &[&first_ready]).await;
    let rest = claim(
        &handles,
        &fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await
    .expect("a claim");
    assert_eq!(
        seqs(Some(&rest)),
        vec![
            wake_one.enqueue_seq,
            wake_two.enqueue_seq,
            joined.enqueue_seq
        ],
        "deferred members recompose with later rows at their own positions"
    );
}

/// Law 20 (fencing): a settlement, claim or abandon under a drive fence that
/// is no longer the session's current epoch is refused and writes nothing; a
/// claim a superseded epoch took settles and abandons nothing even under the
/// current fence; the next epoch re-derives the interrupted claim exactly, and
/// once it has, the old claim is superseded.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_reclaimed_row_supersedes_the_old_claim(handles: SessionIngressHandles) {
    let one = admit(&handles, input("one")).await;
    let two = admit(&handles, input("two")).await;
    let three = admit(&handles, input("three")).await;
    let first = seal(&handles, "first-epoch").await;
    let settled = claim(
        &handles,
        &first,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(1),
    )
    .await
    .expect("a claim");
    assert_eq!(settled.drive_epoch, first.epoch());
    assert_eq!(&settled.admission, first.admission());
    assert!(
        claim(
            &handles,
            &first,
            ClaimMode::Idle,
            IngressClaimPolicy::bounded(8)
        )
        .await
        .is_none(),
        "the lane head is held"
    );
    deliver(&handles, &first, &settled, &[&one]).await;
    let interrupted = claim(
        &handles,
        &first,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(1),
    )
    .await
    .expect("a claim");
    assert_eq!(seqs(Some(&interrupted)), vec![two.enqueue_seq]);

    let second = seal(&handles, "second-epoch").await;
    assert!(second.epoch() > first.epoch());
    let stale = settle(
        &handles,
        &first,
        &[&interrupted],
        IngressSettlementIntent::Turn {
            delivered: vec![two.item_id.clone()],
            cancel: None,
        },
    )
    .await;
    assert!(
        matches!(stale, Err(crate::StoreError::StaleDriveFence { .. })),
        "a superseded drive epoch settles nothing: {stale:?}"
    );
    let stale_claim = handles
        .ingress
        .claim_turn_items(&first, ClaimMode::Idle, &IngressClaimPolicy::bounded(8))
        .await;
    assert!(
        matches!(stale_claim, Err(crate::StoreError::StaleDriveFence { .. })),
        "a superseded drive epoch claims nothing: {stale_claim:?}"
    );
    let unreclaimed = settle(
        &handles,
        &second,
        &[&interrupted],
        IngressSettlementIntent::Turn {
            delivered: vec![two.item_id.clone()],
            cancel: None,
        },
    )
    .await;
    assert!(
        matches!(unreclaimed, Err(crate::StoreError::StaleDriveFence { .. })),
        "a claim of a superseded epoch settles nothing under the current fence \
         until the current epoch reclaims it: {unreclaimed:?}"
    );
    for (fence, what) in [(&first, "a stale fence"), (&second, "a superseded claim")] {
        let abandoned = handles
            .ingress
            .abandon_ingress_claim(fence, &interrupted)
            .await;
        assert!(
            matches!(abandoned, Err(crate::StoreError::StaleDriveFence { .. })),
            "{what} abandons nothing: {abandoned:?}"
        );
    }
    let still_held = rows(&handles).await;
    assert_eq!(
        row(&still_held, &two).state,
        IngressState::Accepted,
        "a refused abandon leaves the interrupted hold in place"
    );
    let rederived = claim(
        &handles,
        &second,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await
    .expect("the interrupted claim is re-derived");
    assert_eq!(
        seqs(Some(&rederived)),
        vec![two.enqueue_seq],
        "the next epoch re-derives exactly the interrupted composition, not the prefix"
    );
    assert_eq!(rederived.drive_epoch, second.epoch());
    assert_eq!(
        rederived.predecessor.as_ref(),
        Some(&interrupted.identity())
    );
    let refused = settle(
        &handles,
        &second,
        &[&interrupted],
        IngressSettlementIntent::Turn {
            delivered: vec![two.item_id.clone()],
            cancel: None,
        },
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(crate::StoreError::IngressClaimSuperseded { .. })
        ),
        "the superseded claim settles nothing: {refused:?}"
    );
    deliver(&handles, &second, &rederived, &[&two]).await;
    let last = claim(
        &handles,
        &second,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await;
    assert_eq!(seqs(last.as_ref()), vec![three.enqueue_seq]);
}

/// The drive-epoch seal is a compare-and-set on the session's `session_meta`
/// row, idempotent per admission: a retried seal answers the fence it already
/// raised, and a seal from a stale observation is superseded without writing.
/// The seal stores the start marker of the execution that sealed it: the same
/// admission sealed under another marker, a fresh execution of a root that
/// already started, is `ExecutionLost` and writes nothing (L-S8).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn the_drive_epoch_seal_is_idempotent_per_admission(handles: SessionIngressHandles) {
    let seal_at = |admission: &'static str, observed: u64| {
        let handles = handles.clone();
        async move {
            handles
                .ingress
                .seal_drive_epoch(
                    &session(),
                    &AdmissionId::new(admission),
                    observed,
                    &root_start(),
                )
                .await
                .expect("seal a drive epoch")
        }
    };
    let start = handles
        .ingress
        .drive_epoch(&session())
        .await
        .expect("read the drive epoch");
    let observed = start.epoch;
    let DriveEpochSeal::Sealed(first) = seal_at("seal-a", observed).await else {
        panic!("a seal at the stored epoch is granted");
    };
    assert_eq!(first.epoch(), observed + 1);
    assert_eq!(first.admission(), &AdmissionId::new("seal-a"));
    assert_eq!(
        seal_at("seal-a", observed).await,
        DriveEpochSeal::Sealed(first.clone()),
        "a retried seal answers the same fence"
    );
    assert_eq!(
        handles
            .ingress
            .seal_drive_epoch(
                &session(),
                &AdmissionId::new("seal-a"),
                observed,
                &lash_core::store::RootStartNonce::new("another execution"),
            )
            .await
            .expect("seal a drive epoch"),
        DriveEpochSeal::ExecutionLost,
        "the sealed admission under another start marker is a lost execution"
    );
    let sealed = handles
        .ingress
        .drive_epoch(&session())
        .await
        .expect("read the drive epoch");
    assert_eq!(
        sealed.epoch,
        first.epoch(),
        "a lost execution writes nothing"
    );
    assert_eq!(sealed.root_start, Some(root_start()));
    assert_eq!(
        seal_at("seal-b", observed).await,
        DriveEpochSeal::Superseded {
            epoch: first.epoch()
        },
        "another admission at the old observation is superseded"
    );
    assert_eq!(
        seal_at("seal-a", first.epoch()).await,
        DriveEpochSeal::Sealed(first.clone()),
        "a retry that re-read the epoch it raised answers the same fence without raising"
    );
    let DriveEpochSeal::Sealed(second) = seal_at("seal-b", first.epoch()).await else {
        panic!("a seal at the new epoch is granted");
    };
    assert_eq!(second.epoch(), first.epoch() + 1);
    assert_eq!(
        seal_at("seal-a", observed).await,
        DriveEpochSeal::Superseded {
            epoch: second.epoch()
        },
        "a retry after a later seal no longer answers"
    );
    let stored = handles
        .ingress
        .drive_epoch(&session())
        .await
        .expect("read the drive epoch");
    assert_eq!(stored.epoch, second.epoch());
    assert_eq!(stored.admission, Some(AdmissionId::new("seal-b")));
}

/// A suffix withdrawal withdraws the anchor and every later open item of its
/// lane, and leaves the other lane alone.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_suffix_withdrawal_stays_in_its_lane(handles: SessionIngressHandles) {
    let before = admit(&handles, input("before")).await;
    let anchor = admit(&handles, input("anchor").with_source_key("anchor")).await;
    let command = admit(&handles, refresh("after")).await;
    let after = admit(&handles, wake("suffix", 1)).await;
    let outcome = handles
        .ingress
        .withdraw_ingress_suffix(
            &session(),
            &IngressWithdrawTarget::SourceKey("anchor".to_string()),
        )
        .await
        .expect("withdraw the suffix");
    let IngressSuffixWithdrawOutcome::Outcomes { outcomes, .. } = outcome else {
        panic!("the anchor exists");
    };
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(IngressWithdrawOutcome::is_withdrawn));
    let all = rows(&handles).await;
    assert_eq!(row(&all, &before).state, IngressState::Open);
    assert_eq!(row(&all, &anchor).state, IngressState::Cancelled);
    assert_eq!(row(&all, &after).state, IngressState::Cancelled);
    assert_eq!(row(&all, &command).state, IngressState::Open);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn reclaim(
    handles: &SessionIngressHandles,
    fence: &DriveFence,
    claim: &IngressClaim,
) -> IngressReclaimOutcome {
    handles
        .ingress
        .reclaim_ingress_claim(fence, claim)
        .await
        .expect("reclaim a resumed claim")
}

/// FIG-3552, own rows: a run resumed under a new drive epoch reclaims
/// every row its claim owns, through the claim CAS, as one new claim pinned
/// to the new epoch; the old claim then settles nothing, and no other
/// claim of the new epoch can take the rows.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_resumed_claim_reclaims_every_row_it_owns(handles: SessionIngressHandles) {
    let running = TurnId::from("resumed-turn");
    let first = admit(&handles, steer("first")).await;
    let second = admit(&handles, wake("resumed", 1)).await;
    let later = admit(&handles, steer("later")).await;
    let old_fence = seal(&handles, "resumed-first").await;
    let old = claim(
        &handles,
        &old_fence,
        checkpoint(&running, crate::CheckpointKind::AfterWork),
        IngressClaimPolicy::bounded(2),
    )
    .await
    .expect("a checkpoint claim");
    assert_eq!(
        seqs(Some(&old)),
        vec![first.enqueue_seq, second.enqueue_seq]
    );
    assert!(
        matches!(reclaim(&handles, &old_fence, &old).await, IngressReclaimOutcome::Reclaimed(same) if same.identity() == old.identity()),
        "a claim that already pins the epoch is its own reclaim"
    );
    let partial = handles
        .ingress
        .settle_session_ingress_for_testing(
            &old_fence,
            IngressClaimSettlement {
                session_id: session(),
                claims: vec![IngressClaimRef {
                    identity: old.identity(),
                    item_ids: vec![first.item_id.clone()],
                }],
                intent: IngressSettlementIntent::Turn {
                    delivered: vec![first.item_id.clone()],
                    cancel: None,
                },
            },
        )
        .await;
    assert!(
        matches!(
            partial,
            Err(crate::StoreError::IngressSettlementRefused { .. })
        ),
        "a settlement that names only part of its claim is refused: {partial:?}"
    );
    let new_fence = seal(&handles, "resumed-second").await;
    let stale = handles
        .ingress
        .reclaim_ingress_claim(&old_fence, &old)
        .await;
    assert!(
        matches!(stale, Err(crate::StoreError::StaleDriveFence { .. })),
        "a superseded drive epoch reclaims nothing: {stale:?}"
    );
    // Whether the claim is already this epoch's is the stored rows' answer:
    // a copy of the claim that claims the new epoch still moves the rows.
    let restamped = IngressClaim {
        drive_epoch: new_fence.epoch(),
        admission: new_fence.admission().clone(),
        ..old.clone()
    };
    let IngressReclaimOutcome::Reclaimed(resumed) = reclaim(&handles, &new_fence, &restamped).await
    else {
        panic!("the resumed run owns every row of its claim");
    };
    assert_eq!(
        resumed.item_ids(),
        old.item_ids(),
        "every owned row, and only those"
    );
    assert_eq!(resumed.drive_epoch, new_fence.epoch());
    assert_eq!(&resumed.admission, new_fence.admission());
    assert_eq!(resumed.mode, old.mode, "the reclaim keeps the claim's mode");
    assert_eq!(resumed.predecessor.as_ref(), Some(&old.identity()));
    assert!(
        claim(
            &handles,
            &new_fence,
            checkpoint(&running, crate::CheckpointKind::AfterWork),
            IngressClaimPolicy::bounded(8),
        )
        .await
        .is_none(),
        "the reclaimed rows are held by the new epoch"
    );
    assert!(matches!(
        settle(
            &handles,
            &new_fence,
            &[&old],
            IngressSettlementIntent::Turn {
                delivered: vec![first.item_id.clone()],
                cancel: None,
            },
        )
        .await,
        Err(crate::StoreError::IngressClaimSuperseded { .. })
    ));
    deliver(&handles, &new_fence, &resumed, &[&first, &second]).await;
    let rest = claim(
        &handles,
        &new_fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await;
    assert_eq!(seqs(rest.as_ref()), vec![later.enqueue_seq]);
}

/// FIG-3552, peer supersession: a redrive whose claim a peer superseded
/// through the claim CAS — re-claimed or withdrawn any row of it — cedes and
/// writes nothing. It never drops the lost row and retries with the rest.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_redrive_superseded_by_a_peer_cedes(handles: SessionIngressHandles) {
    let first = admit(&handles, input("first")).await;
    let second = admit(&handles, input("second")).await;
    let old_fence = seal(&handles, "peer-first").await;
    let old = claim(
        &handles,
        &old_fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await
    .expect("a claim");
    assert_eq!(
        seqs(Some(&old)),
        vec![first.enqueue_seq, second.enqueue_seq]
    );
    let peer_fence = seal(&handles, "peer-second").await;
    let peer = claim(
        &handles,
        &peer_fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await
    .expect("the peer re-derives the interrupted claim");
    assert_eq!(peer.item_ids(), old.item_ids());
    let redrive_fence = seal(&handles, "peer-third").await;
    assert!(
        matches!(
            reclaim(&handles, &redrive_fence, &old).await,
            IngressReclaimOutcome::Ceded
        ),
        "a peer re-claimed the rows through the CAS: the redrive cedes"
    );
    assert!(
        matches!(
            settle(
                &handles,
                &redrive_fence,
                &[&old],
                IngressSettlementIntent::Turn {
                    delivered: old.item_ids(),
                    cancel: None,
                },
            )
            .await,
            Err(crate::StoreError::IngressClaimSuperseded { .. })
        ),
        "the ceding redrive commits nothing"
    );
    let after = rows(&handles).await;
    assert!(
        after.iter().all(|row| row.state == IngressState::Accepted),
        "the peer still holds every row"
    );

    // A withdrawn row supersedes too, and nothing moves.
    let IngressReclaimOutcome::Reclaimed(current) = reclaim(&handles, &redrive_fence, &peer).await
    else {
        panic!("the peer's claim is still whole");
    };
    let third = admit(&handles, input("third")).await;
    let fourth = admit(&handles, input("fourth")).await;
    deliver(&handles, &redrive_fence, &current, &[&first, &second]).await;
    let pair = claim(
        &handles,
        &redrive_fence,
        ClaimMode::Idle,
        IngressClaimPolicy::bounded(8),
    )
    .await
    .expect("a claim");
    assert_eq!(
        pair.item_ids(),
        vec![third.item_id.clone(), fourth.item_id.clone()]
    );
    let last_fence = seal(&handles, "peer-fourth").await;
    let withdrawn = handles
        .ingress
        .withdraw_ingress_items(
            &session(),
            &[IngressWithdrawTarget::ItemId(fourth.item_id.clone())],
        )
        .await
        .expect("withdraw an interrupted row");
    assert!(withdrawn[0].outcome.is_withdrawn());
    assert!(matches!(
        reclaim(&handles, &last_fence, &pair).await,
        IngressReclaimOutcome::Ceded
    ));
    let still = rows(&handles).await;
    assert_eq!(
        row(&still, &third).state,
        IngressState::Accepted,
        "a ceding reclaim moves no row, not even the ones it still owned"
    );
}
