//! The rank is reserved at the §4 point and a seat only publishes it (ADR 0099
//! §5 as amended by FIG-4308), driven through the index's own handlers and the
//! dispatch's real child handler.
//!
//! - A hole: a lower rank reserved and unseated while higher ranks seat. No
//!   read — point or run, consuming or cursorless — is served past it.
//! - Decision order: a cancel decided while a committed sibling still drains
//!   ranks after that sibling, and is not observable until it seats.
//! - The committed final wins (ADR 0099 §5): a commit retains the final it
//!   committed and answers it to every later commit of the child, and a
//!   successor whose attach expired seats that final — a committed refusal as
//!   recorded, and a final it cannot realize reported lost by name — never
//!   its own refusal. Retirement clears every retained final.
//! - The dispatch's one registration: its transitions and refusals.
//! - A drain held by several blockers lifts whichever of them seats first
//!   (FIG-4431), through the in-handler controller a tool child drains with.

use std::collections::BTreeMap;
use std::time::Duration;

use lash_core::RuntimeEffectEnvelope;
use restate_sdk::context::WorkflowContext;
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;

use crate::RestateIngressClient;
use crate::effect_group::{
    EffectGroupAdmissionRequest, EffectGroupAdmissionResponse, EffectGroupAdoptRequest,
    EffectGroupChildRequest, EffectGroupCloseRequest, EffectGroupCloseResponse,
    EffectGroupCommitChildRequest, EffectGroupCommitChildResponse, EffectGroupCommittedFinal,
    EffectGroupNotice, EffectGroupNotification, EffectGroupOpenRequest, EffectGroupOpenResponse,
    EffectGroupProbeAdoptResponse, EffectGroupReadRankRequest, EffectGroupReadRankResponse,
    EffectGroupRecordSettlementRequest, EffectGroupRecordSettlementResponse,
    EffectGroupRegisterDispatchRequest, EffectGroupRegisterDispatchResponse,
    EffectGroupSettlementTerminal, EffectGroupShape,
};

use super::effect_group_conformance::{
    HarnessServer, LiveConformanceHarness, witness_child, witness_key, witness_membership,
    witness_shape,
};

/// One group of witness children and the index calls the laws make on it.
pub(super) struct Group {
    pub(super) ingress: RestateIngressClient,
    pub(super) key: String,
    children: Vec<RuntimeEffectEnvelope>,
    shape: EffectGroupShape,
}

impl Group {
    pub(super) async fn open(ingress: RestateIngressClient, label: &str, width: usize) -> Self {
        let key = witness_key(label);
        let children = (0..width)
            .map(|position| witness_child(&key, position))
            .collect::<Vec<_>>();
        let shape = witness_shape(&key, &children);
        let opened: EffectGroupOpenResponse = ingress
            .call_lash_object(
                "EffectGroupIndex",
                &key,
                "open",
                &EffectGroupOpenRequest {
                    shape: shape.clone(),
                    membership: witness_membership(&children),
                    dispatch_route: super::effect_group_conformance::witness_dispatch_route(),
                    content_checked: false,
                },
            )
            .await
            .expect("the law's group opens");
        assert!(
            matches!(opened, EffectGroupOpenResponse::OpenedFresh { .. }),
            "the law's group opens fresh: {opened:?}"
        );
        Self {
            ingress,
            key,
            children,
            shape,
        }
    }

    /// A group the index holds no record of.
    pub(super) fn unopened(ingress: RestateIngressClient, label: &str) -> Self {
        let key = witness_key(label);
        let shape = witness_shape(&key, &[]);
        Self {
            ingress,
            key,
            children: Vec::new(),
            shape,
        }
    }

    pub(super) async fn reopen(&self) {
        let reopened: EffectGroupOpenResponse = self
            .ingress
            .call_lash_object(
                "EffectGroupIndex",
                &self.key,
                "open",
                &EffectGroupOpenRequest {
                    shape: self.shape.clone(),
                    membership: witness_membership(&self.children),
                    dispatch_route: super::effect_group_conformance::witness_dispatch_route(),
                    content_checked: false,
                },
            )
            .await
            .expect("the law's group reopens");
        assert!(
            matches!(reopened, EffectGroupOpenResponse::ReopenedClosed { .. }),
            "the law's closed group reopens: {reopened:?}"
        );
    }

    /// Commits `position` with an outcome only its committing invocation
    /// holds, as an atomic child's commit does.
    pub(super) async fn commit(&self, position: usize) -> EffectGroupCommitChildResponse {
        self.commit_final(position, EffectGroupCommittedFinal::Held)
            .await
    }

    pub(super) async fn commit_final(
        &self,
        position: usize,
        committed: EffectGroupCommittedFinal,
    ) -> EffectGroupCommitChildResponse {
        self.ingress
            .call_lash_object(
                "EffectGroupIndex",
                &self.key,
                "commit_child",
                &EffectGroupCommitChildRequest {
                    replay_key: self.shape.replay_keys[position].clone(),
                    committed,
                },
            )
            .await
            .expect("a child of the law commits")
    }

    pub(super) async fn seat(&self, position: usize) -> EffectGroupRecordSettlementResponse {
        self.ingress
            .call_lash_object(
                "EffectGroupIndex",
                &self.key,
                "record_settlement",
                &EffectGroupRecordSettlementRequest {
                    position,
                    terminal: EffectGroupSettlementTerminal::Cancelled,
                },
            )
            .await
            .expect("a child of the law seats")
    }

    pub(super) async fn read(
        &self,
        rank: u64,
        for_caller: bool,
        run: bool,
    ) -> EffectGroupReadRankResponse {
        self.ingress
            .call_lash_object(
                "EffectGroupIndex",
                &self.key,
                "read_rank",
                &EffectGroupReadRankRequest {
                    rank,
                    for_caller,
                    run,
                },
            )
            .await
            .expect("the law reads a rank")
    }

    /// The (rank, position) pairs a run read from `rank` serves, or `None`
    /// when the read is not served.
    pub(super) async fn run_from(&self, rank: u64) -> Option<Vec<(u64, usize)>> {
        match self.read(rank, false, true).await {
            EffectGroupReadRankResponse::SettledRun { ranks } => Some(
                (rank..)
                    .zip(&ranks)
                    .map(|(rank, served)| (rank, served.settlement.position))
                    .collect(),
            ),
            EffectGroupReadRankResponse::NotSettled | EffectGroupReadRankResponse::Closed => None,
            other => panic!("a run read of rank {rank} answered {other:?}"),
        }
    }

    /// The §5 barrier at `rank`, as the index's `Drained` notice answers it
    /// within `within`: `None` while some committed sibling below it still
    /// owes its seat.
    pub(super) async fn barrier(
        &self,
        rank: u64,
        within: Duration,
    ) -> Option<EffectGroupNotification> {
        tokio::time::timeout(
            within,
            super::effect_group_conformance::await_group_wait(
                &self.ingress,
                &self.key,
                EffectGroupNotice::Drained { rank },
            ),
        )
        .await
        .ok()
    }

    pub(super) async fn close_cancel(&self) {
        let closed: EffectGroupCloseResponse = self
            .ingress
            .call_lash_object(
                "EffectGroupIndex",
                &self.key,
                "close",
                &EffectGroupCloseRequest {
                    disposition: lash_core::LoserPolicy::Cancel,
                },
            )
            .await
            .expect("the law's group closes");
        assert_eq!(closed, EffectGroupCloseResponse::Closed);
    }

    pub(super) async fn adopt(&self, dispatcher: &str) -> EffectGroupProbeAdoptResponse {
        self.ingress
            .call_lash_object(
                "EffectGroupIndex",
                &self.key,
                "probe_and_adopt",
                &EffectGroupAdoptRequest {
                    invocation_id: dispatcher.to_owned(),
                },
            )
            .await
            .expect("the law's dispatcher adopts")
    }

    pub(super) async fn register(
        &self,
        addresses: BTreeMap<usize, String>,
    ) -> EffectGroupRegisterDispatchResponse {
        self.ingress
            .call_lash_object(
                "EffectGroupIndex",
                &self.key,
                "register_dispatch",
                &EffectGroupRegisterDispatchRequest { addresses },
            )
            .await
            .expect("the law's dispatch registers")
    }

    pub(super) async fn admit(
        &self,
        position: usize,
        invocation_id: &str,
    ) -> EffectGroupAdmissionResponse {
        self.ingress
            .call_lash_object(
                "EffectGroupIndex",
                &self.key,
                "admit_child",
                &EffectGroupAdmissionRequest {
                    position,
                    invocation_id: invocation_id.to_owned(),
                },
            )
            .await
            .expect("the law's child asks for admission")
    }

    /// A real, finished invocation that stands in for a dispatcher or a
    /// dispatched child: the index records its id, and a close or retirement
    /// may cancel it harmlessly.
    pub(super) async fn stand_in(&self, label: &str) -> String {
        self.ingress
            .send_lash_workflow(
                &super::effect_group_conformance::witness_dispatch_route(),
                &format!("{}-stand-in-{label}", self.key),
                "preflight",
                &Vec::<RuntimeEffectEnvelope>::new(),
            )
            .await
            .expect("a stand-in invocation is accepted")
            .as_str()
            .to_owned()
    }

    pub(super) async fn stand_in_ids(&self) -> BTreeMap<usize, String> {
        let mut ids = BTreeMap::new();
        for position in 0..self.children.len() {
            ids.insert(position, self.stand_in(&position.to_string()).await);
        }
        ids
    }

    pub(super) async fn adopt_stand_in(&self) {
        let dispatcher = self.stand_in("dispatcher").await;
        assert!(
            matches!(
                self.adopt(&dispatcher).await,
                EffectGroupProbeAdoptResponse::Adopted { .. }
            ),
            "the law's dispatcher adopts a fresh group"
        );
    }

    /// Adopts and registers stand-in children: a ready group whose recorded
    /// ids no real child invocation carries.
    pub(super) async fn make_ready(&self) -> BTreeMap<usize, String> {
        self.adopt_stand_in().await;
        let ids = self.stand_in_ids().await;
        assert_eq!(
            self.register(ids.clone()).await,
            EffectGroupRegisterDispatchResponse::Registered
        );
        ids
    }

    pub(super) async fn retire(&self) {
        self.ingress
            .call_lash_workflow::<_, ()>(
                &super::effect_group_conformance::witness_dispatch_route(),
                &self.key,
                "retire",
                &self.key,
            )
            .await
            .expect("the law's group retires");
    }
}

pub(super) fn rank_of(response: &EffectGroupCommitChildResponse) -> u64 {
    match response {
        EffectGroupCommitChildResponse::Committed { rank }
        | EffectGroupCommitChildResponse::AlreadyCommitted { rank, .. } => *rank,
        other => panic!("the law's child commits: {other:?}"),
    }
}

async fn harness() -> LiveConformanceHarness {
    LiveConformanceHarness::start_on(HarnessServer::in_process()).await
}

/// A workflow that waits at one child's §5 barrier through the in-handler
/// controller, as a tool child's drain does, and ends when the barrier lifts.
/// Its input is the group key and the rank the child's commit reserved.
#[restate_sdk::workflow]
pub(super) trait DrainBarrierProbe {
    async fn run(barrier: Json<(String, u64)>) -> HandlerResult<Json<()>>;
}

pub(super) struct DrainBarrierProbeImpl;

impl DrainBarrierProbe for DrainBarrierProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json((group_key, rank)): Json<(String, u64)>,
    ) -> HandlerResult<Json<()>> {
        let controller = crate::RestateRuntimeEffectController::with_options_for_test(
            ctx,
            crate::RestateEffectControllerOptions::default(),
        );
        lash_core::RuntimeEffectController::await_group_child_drain_admission(
            &controller,
            &group_key,
            rank,
        )
        .await
        .map_err(TerminalError::from_error)?;
        Ok(Json(()))
    }
}

/// Waits until nothing on the double moves: every invocation has completed,
/// suspended or blocked on the server, and no journal grew since the last
/// look. Whatever a seat set in motion has then reached its waiters.
async fn quiescent(server: &lash_restate_test::RestateTestServer) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut last = None;
    loop {
        let views = server.invocations();
        let settled = views
            .iter()
            .all(|view| view.status != "running" || view.blocked_on_server == Some(true));
        let snapshot = views
            .into_iter()
            .map(|view| (view.id, view.status, view.journal_len))
            .collect::<Vec<_>>();
        if settled && last.as_ref() == Some(&snapshot) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the server double never went quiet"
        );
        last = Some(snapshot);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A reserved rank that has not seated is a hole: no read of it or past it is
/// served, a run stops at it, and once it seats the whole prefix is served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_read_is_served_past_an_unseated_reserved_rank() {
    let harness = harness().await;
    let group = Group::open(harness.ingress(), "rank-hole", 4).await;
    for position in 0..4 {
        assert_eq!(
            rank_of(&group.commit(position).await),
            position as u64 + 1,
            "commit {position} reserves rank {}",
            position + 1
        );
    }
    // Ranks 2 and 3 seat while rank 1 is held before its projection, and
    // rank 4 stays unseated too.
    for position in [2, 1] {
        assert!(
            matches!(
                group.seat(position).await,
                EffectGroupRecordSettlementResponse::Recorded { rank } if rank == position as u64 + 1
            ),
            "child {position} publishes its reserved rank"
        );
    }
    for rank in 1..=4 {
        for (for_caller, run) in [(true, true), (false, true), (false, false)] {
            assert!(
                matches!(
                    group.read(rank, for_caller, run).await,
                    EffectGroupReadRankResponse::NotSettled
                ),
                "rank {rank} is not served while rank 1 is a hole (caller {for_caller}, run {run})"
            );
        }
    }
    assert!(
        matches!(
            group.seat(0).await,
            EffectGroupRecordSettlementResponse::Recorded { rank: 1 }
        ),
        "the held child publishes rank 1"
    );
    assert_eq!(
        group.run_from(1).await,
        Some(vec![(1, 0), (2, 1), (3, 2)]),
        "the run stops at the next hole, rank 4"
    );
    assert_eq!(group.run_from(2).await, Some(vec![(2, 1), (3, 2)]));
    assert_eq!(
        group.run_from(4).await,
        None,
        "rank 4 is reserved and unseated"
    );
    assert!(
        matches!(
            group.read(3, false, false).await,
            EffectGroupReadRankResponse::Settled { settlement, .. } if settlement.position == 2
        ),
        "a point read inside the prefix is served"
    );
    group.retire().await;
    harness.finish().await;
}

/// The §4 point reserves the rank and the seat publishes it: a later commit
/// that seats first publishes its own higher rank, a cancel decision ranks
/// after every earlier commit, a repeated commit allocates nothing, and a
/// redriven seat answers its reserved rank again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_rank_is_reserved_at_the_commit_and_published_at_the_seat() {
    let harness = harness().await;
    let group = Group::open(harness.ingress(), "reserved-rank", 3).await;
    group.make_ready().await;
    assert_eq!(
        rank_of(&group.commit(0).await),
        1,
        "the first commit reserves rank 1"
    );
    assert_eq!(
        rank_of(&group.commit(1).await),
        2,
        "the second commit reserves rank 2"
    );
    assert!(
        matches!(
            group.commit(1).await,
            EffectGroupCommitChildResponse::AlreadyCommitted { rank: 2, .. }
        ),
        "a repeated commit answers its reserved rank and allocates nothing"
    );
    assert!(
        matches!(
            group.seat(1).await,
            EffectGroupRecordSettlementResponse::Recorded { rank: 2 }
        ),
        "a later-committed sibling that seats first publishes its reserved rank 2"
    );
    assert_eq!(
        group.run_from(1).await,
        None,
        "rank 1 is reserved and unseated, so no reader is served past it"
    );
    group.close_cancel().await;
    assert!(
        matches!(
            group.commit(2).await,
            EffectGroupCommitChildResponse::CancelDecided { rank: 3 }
        ),
        "the cancel decision took the next rank after both commits"
    );
    assert!(
        matches!(
            group.seat(0).await,
            EffectGroupRecordSettlementResponse::Recorded { rank: 1 }
        ),
        "the first committer seats the rank its commit reserved"
    );
    assert!(
        matches!(
            group.seat(0).await,
            EffectGroupRecordSettlementResponse::Duplicate { rank: 1 }
        ),
        "a redriven seat answers its reserved rank again"
    );
    assert_eq!(
        group.run_from(1).await,
        Some(vec![(1, 0), (2, 1), (3, 2)]),
        "rank order is §4 decision order: both commits, then the cancel decision"
    );
    group.retire().await;
    harness.finish().await;
}

/// The approved decision order: a cancel decided while a committed sibling is
/// still draining ranks after it, and B's cancellation is held behind A's
/// drain: no reader is served past A's unseated rank, and a reopened caller
/// is served the run once A seats.
///
/// The group's recorded children are stand-ins, so A has no invocation of
/// its own: a reopen while A owes its seat would re-send A, as recovery does
/// for a committed child whose invocation is gone (FIG-4454). The caller
/// therefore reopens once A has seated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_decided_behind_a_draining_commit_ranks_after_it() {
    let harness = harness().await;
    let group = Group::open(harness.ingress(), "decision-order", 2).await;
    group.make_ready().await;
    // A commits and stalls in its drain.
    assert_eq!(rank_of(&group.commit(0).await), 1, "A reserves rank 1");
    // The opener closes with Cancel, which decides B.
    group.close_cancel().await;
    assert!(
        matches!(
            group.commit(1).await,
            EffectGroupCommitChildResponse::CancelDecided { rank: 2 }
        ),
        "B's cancel decision took rank 2, after A's commit"
    );
    for rank in [1, 2] {
        assert!(
            matches!(
                group.read(rank, false, true).await,
                EffectGroupReadRankResponse::Closed
            ),
            "rank {rank} is not observable until A seats"
        );
    }
    assert!(
        matches!(
            group.seat(0).await,
            EffectGroupRecordSettlementResponse::Recorded { rank: 1 }
        ),
        "A publishes rank 1 when its drain finishes"
    );
    group.reopen().await;
    let EffectGroupReadRankResponse::SettledRun { ranks } = group.read(1, true, true).await else {
        panic!("the reopened caller is served once A seats");
    };
    assert_eq!(
        (1u64..)
            .zip(&ranks)
            .map(|(rank, served)| (rank, served.settlement.position))
            .collect::<Vec<_>>(),
        vec![(1, 0), (2, 1)],
        "the run is A, then B's cancellation"
    );
    assert!(matches!(
        ranks[1].settlement.terminal,
        EffectGroupSettlementTerminal::Cancelled
    ));
    group.retire().await;
    harness.finish().await;
}

/// Sends the real child handler for `position` under a fresh invocation: the
/// index retains a different id for it, so the child is an expired attach and
/// settles its typed refusal without running.
async fn send_attach_expired_child(group: &Group, position: usize) -> String {
    group
        .ingress
        .send_lash_workflow(
            &super::effect_group_conformance::witness_dispatch_route(),
            &group.key,
            "child",
            &EffectGroupChildRequest {
                group_key: group.key.clone(),
                shape: group.shape.clone(),
                position,
                envelope: group.children[position].clone(),
            },
        )
        .await
        .expect("the successor child is accepted")
        .as_str()
        .to_owned()
}

/// Waits until `invocation` completes on the double.
async fn completed(server: &lash_restate_test::RestateTestServer, invocation: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while server.outcome(invocation).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "invocation {invocation} did not complete"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The settlement the successor at `position` seated, once it completed.
async fn seated_failure(
    group: &Group,
    rank: u64,
    position: usize,
) -> lash_core::RuntimeEffectControllerError {
    let EffectGroupReadRankResponse::Settled { settlement, .. } =
        group.read(rank, false, false).await
    else {
        panic!("rank {rank} is served");
    };
    assert_eq!(settlement.position, position);
    match settlement.terminal {
        EffectGroupSettlementTerminal::Failed { error } => error,
        other => panic!("child {position} seated a failure, got {other:?}"),
    }
}

/// The A/B/C counterexample, through the actual child handler. A (rank 1), B
/// (rank 2) and C (rank 3) committed; B's final is one only its committing
/// invocation held. That invocation is gone, and B's successor is an expired
/// attach: its commit finds B's final holding the point, so the committed
/// final wins and the successor reports it lost by name, not with its own
/// attach-expired refusal. It drains nothing, so it seats without waiting for
/// A, and no read is served past A's hole until A seats. C's barrier holds for
/// A throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_attach_over_a_committed_final_reports_it_lost_not_its_refusal() {
    let harness = harness().await;
    let server = harness
        .server_double()
        .expect("the law watches the child on the server double");
    let group = Group::open(harness.ingress(), "successor-lost", 3).await;
    group.make_ready().await;
    for position in 0..3 {
        assert_eq!(rank_of(&group.commit(position).await), position as u64 + 1);
    }
    let successor = send_attach_expired_child(&group, 1).await;
    quiescent(&server).await;
    assert!(
        matches!(server.outcome(&successor), Some(Ok(_))),
        "B's successor seats at once: it drains nothing, so it waits on no sibling: {:?}",
        server.outcome(&successor)
    );
    assert!(
        matches!(
            group.read(2, false, false).await,
            EffectGroupReadRankResponse::NotSettled
        ),
        "no read is served past A's unseated rank"
    );
    assert_eq!(
        group.barrier(3, Duration::from_millis(300)).await,
        None,
        "C's barrier holds while A owes its seat"
    );
    assert!(matches!(
        group.seat(0).await,
        EffectGroupRecordSettlementResponse::Recorded { rank: 1 }
    ));
    let error = seated_failure(&group, 2, 1).await;
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RuntimeEffectGroupChildCommittedFinalLost,
        "B's committed final is reported lost, not replaced by the attach-expired refusal: {error}"
    );
    assert_eq!(
        group.barrier(3, Duration::from_secs(30)).await,
        Some(EffectGroupNotification::Drained),
        "A and B seated, so C's barrier lifted"
    );
    assert!(matches!(
        group.seat(2).await,
        EffectGroupRecordSettlementResponse::Recorded { rank: 3 }
    ));
    assert_eq!(
        group.barrier(4, Duration::from_secs(30)).await,
        Some(EffectGroupNotification::Drained),
        "the closing barrier lifts once every commit seated"
    );
    group.retire().await;
    harness.finish().await;
}

/// A refusal an earlier invocation committed is the child's final: B's first
/// successor committed its session-generation refusal and ended before its
/// seat, and B's next successor, an expired attach, seats that committed
/// refusal as recorded rather than its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_attach_over_a_committed_refusal_seats_that_refusal() {
    let harness = harness().await;
    let server = harness
        .server_double()
        .expect("the law watches the child on the server double");
    let group = Group::open(harness.ingress(), "successor-refusal", 2).await;
    group.make_ready().await;
    assert_eq!(rank_of(&group.commit(0).await), 1);
    let committed_refusal = lash_core::RuntimeEffectControllerError::new(
        lash_core::RuntimeErrorCode::SessionStateVersionNewerThanRuntime,
        "the law's committed refusal",
    );
    assert_eq!(
        rank_of(
            &group
                .commit_final(
                    1,
                    EffectGroupCommittedFinal::Refusal {
                        error: committed_refusal,
                    },
                )
                .await
        ),
        2
    );
    assert!(matches!(
        group.seat(0).await,
        EffectGroupRecordSettlementResponse::Recorded { rank: 1 }
    ));
    let successor = send_attach_expired_child(&group, 1).await;
    completed(&server, &successor).await;
    let error = seated_failure(&group, 2, 1).await;
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::SessionStateVersionNewerThanRuntime,
        "the committed refusal seats as recorded: {error}"
    );
    assert_eq!(error.message, "the law's committed refusal");
    group.retire().await;
    harness.finish().await;
}

/// A successor over a child no final holds commits its own refusal and seats
/// it: the attach-expired refusal is the child's final only where nothing
/// else is committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_attach_over_an_uncommitted_child_seats_its_refusal() {
    let harness = harness().await;
    let server = harness
        .server_double()
        .expect("the law watches the child on the server double");
    let group = Group::open(harness.ingress(), "successor-uncommitted", 2).await;
    group.make_ready().await;
    assert_eq!(rank_of(&group.commit(0).await), 1);
    let successor = send_attach_expired_child(&group, 1).await;
    completed(&server, &successor).await;
    assert!(matches!(
        group.seat(0).await,
        EffectGroupRecordSettlementResponse::Recorded { rank: 1 }
    ));
    let error = seated_failure(&group, 2, 1).await;
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RuntimeEffectGroupChildAttachExpired,
        "{error}"
    );
    group.retire().await;
    harness.finish().await;
}

/// The point retains the final a commit won with, answers it to every later
/// commit of the child whatever that commit offers, and retirement clears it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commit_retains_its_final_until_retirement() {
    let harness = harness().await;
    let server = harness
        .server_double()
        .expect("the law reads the index state on the server double");
    let group = Group::open(harness.ingress(), "retained-final", 2).await;
    group.make_ready().await;
    let sealed = EffectGroupCommittedFinal::Tool {
        drain_input: "the law's sealed drain input".to_owned(),
    };
    assert_eq!(rank_of(&group.commit_final(0, sealed).await), 1);
    assert_eq!(rank_of(&group.commit(1).await), 2);
    for offered in [
        EffectGroupCommittedFinal::Held,
        EffectGroupCommittedFinal::Refusal {
            error: lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::RuntimeEffectGroupChildAttachExpired,
                "a later offer",
            ),
        },
    ] {
        match group.commit_final(0, offered).await {
            EffectGroupCommitChildResponse::AlreadyCommitted {
                rank: 1,
                committed: EffectGroupCommittedFinal::Tool { drain_input },
            } => assert_eq!(drain_input, "the law's sealed drain input"),
            other => panic!("the retained tool final answers a later commit: {other:?}"),
        }
    }
    assert!(matches!(
        group.commit(1).await,
        EffectGroupCommitChildResponse::AlreadyCommitted {
            rank: 2,
            committed: EffectGroupCommittedFinal::Held,
        }
    ));
    let retained = |server: &lash_restate_test::RestateTestServer| {
        server
            .object_state("EffectGroupIndex", &group.key)
            .into_keys()
            .filter(|key| key.starts_with("effect-group/v1/committed-final/"))
            .collect::<Vec<_>>()
    };
    assert_eq!(retained(&server).len(), 2, "each commit retains its final");
    group.retire().await;
    assert!(
        retained(&server).is_empty(),
        "retirement clears every retained final: {:?}",
        retained(&server)
    );
    harness.finish().await;
}

/// FIG-4455: unrelated shared and exclusive handlers must load the same
/// metadata even when retained tool finals grow by megabytes. Protocol byte
/// counts pin that property independently of host timing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn index_metadata_reads_load_no_retained_tool_drain_inputs() {
    use lash_restate_test::protocol::MessageType;
    use std::collections::BTreeSet;

    let harness = harness().await;
    let server = harness
        .server_double()
        .expect("the counting law uses the double");
    let group = Group::open(harness.ingress(), "drain-read-cost", 8).await;
    group.make_ready().await;
    for position in 0..8 {
        assert_eq!(
            rank_of(
                &group
                    .commit_final(
                        position,
                        EffectGroupCommittedFinal::Tool {
                            drain_input: "x".repeat(32),
                        }
                    )
                    .await
            ),
            position as u64 + 1,
        );
    }
    let small = server.object_state("EffectGroupIndex", &group.key);
    let mut large = small.clone();
    for position in 0..8 {
        let key = format!("effect-group/v1/committed-final/{position}");
        let value = large
            .get_mut(&key)
            .expect("a winning commit retained its final");
        let mut final_state: serde_json::Value =
            serde_json::from_slice(value).expect("stamped final");
        final_state["body"]["drain_input"] = serde_json::json!("x".repeat(256 * 1024));
        *value = serde_json::to_vec(&final_state).expect("encode the larger fixture");
    }
    let cases = [
        (
            "read_rank",
            serde_json::json!({"rank": 1, "for_caller": false, "run": false}),
            serde_json::json!({"type": "not_settled"}),
        ),
        (
            "probe",
            serde_json::Value::Null,
            serde_json::json!({"type": "exists", "shape_digest": group.shape.digest(&witness_membership(&group.children)).expect("shape digest"), "phase": {"type": "ready"}}),
        ),
        (
            "unsettled_children",
            serde_json::Value::Null,
            serde_json::json!(8),
        ),
        (
            "admit_child",
            serde_json::json!({"position": 0, "invocation_id": "successor"}),
            serde_json::json!({"type": "attach_expired"}),
        ),
        (
            "admit_semantic",
            serde_json::json!({"replay_key": group.shape.replay_keys[0]}),
            serde_json::json!({"type": "admitted"}),
        ),
        (
            "child_cancel",
            serde_json::json!({"position": 0}),
            serde_json::Value::Null,
        ),
    ];
    let state_bytes =
        |state: &BTreeMap<String, Vec<u8>>| state.values().map(Vec::len).sum::<usize>();
    let mut timings = vec![[Vec::new(), Vec::new()]; cases.len()];
    let mut byte_counts = vec![[Vec::new(), Vec::new()]; cases.len()];
    for run in 0..5 {
        for side in if run % 2 == 0 { [0, 1] } else { [1, 0] } {
            server.set_object_state(
                "EffectGroupIndex",
                &group.key,
                if side == 0 {
                    small.clone()
                } else {
                    large.clone()
                },
            );
            for (case, (handler, input, expected)) in cases.iter().enumerate() {
                let before: BTreeSet<_> = server
                    .invocations()
                    .into_iter()
                    .map(|view| view.id)
                    .collect();
                let started = std::time::Instant::now();
                let response: serde_json::Value = group
                    .ingress
                    .call_lash_object("EffectGroupIndex", &group.key, handler, input)
                    .await
                    .expect("the metadata call succeeds");
                timings[case][side].push(started.elapsed().as_nanos());
                assert_eq!(&response, expected, "{handler} preserves its result");
                let invocation = server
                    .invocations()
                    .into_iter()
                    .find(|view| {
                        view.target == format!("EffectGroupIndex/{}/{handler}", group.key)
                            && !before.contains(&view.id)
                    })
                    .expect("the metadata call actually executed");
                let journal = server
                    .journal(&invocation.id)
                    .expect("the metadata call's journal");
                let reads = journal
                    .iter()
                    .filter(|entry| entry.ty == MessageType::GetLazyStateCommand)
                    .count();
                let bytes = journal
                    .iter()
                    .filter(|entry| entry.ty == MessageType::GetLazyStateCompletionNotification)
                    .map(|entry| entry.payload.len())
                    .sum::<usize>();
                println!(
                    "FIG-4455 sample run={run} side={side} handler={handler} state_bytes={} state_reads={reads} read_completion_bytes={bytes} elapsed_ns={}",
                    if side == 0 {
                        state_bytes(&small)
                    } else {
                        state_bytes(&large)
                    },
                    timings[case][side].last().expect("recorded timing")
                );
                assert!(
                    reads >= 2 && bytes > 0,
                    "compatibility and index metadata reads actually executed"
                );
                assert_eq!(
                    journal
                        .iter()
                        .filter(|entry| entry.ty == MessageType::GetEagerStateCommand)
                        .count(),
                    0,
                    "no eager state reads"
                );
                byte_counts[case][side].push(bytes);
            }
        }
    }
    for (case, (handler, _, _)) in cases.iter().enumerate() {
        assert_eq!(
            byte_counts[case][0], byte_counts[case][1],
            "{handler} must not load retained drain inputs"
        );
        for (side, samples) in timings[case].iter_mut().enumerate() {
            samples.sort_unstable();
            println!(
                "FIG-4455 median side={side} handler={handler} elapsed_ns={}",
                samples[2]
            );
        }
    }
    // The larger finals remain recoverable. Avoiding unrelated bytes never
    // drops the committed obligation or changes the winning final.
    assert!(matches!(group.commit(0).await,
        EffectGroupCommitChildResponse::AlreadyCommitted {
            rank: 1,
            committed: EffectGroupCommittedFinal::Tool { drain_input },
        } if drain_input == "x".repeat(256 * 1024)
    ));
    group.retire().await;
    harness.finish().await;
}

/// A drain held by two blockers lifts when the higher-ranked one seats first
/// (FIG-4431). The drain parks in its handler; B seats and everything that
/// seat set in motion settles; only then does A seat, and the drain must end.
/// A barrier that awaited one call per blocker and polled them together lost
/// B's wake here: the call waiting for A read B's completion off the input,
/// and B's call, already blocked on the input, read the input again instead
/// of its own completion, parking the drain until the stream's inactivity
/// timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drain_lifts_when_its_blockers_seat_in_reverse_rank_order() {
    let harness = harness().await;
    let server = harness
        .server_double()
        .expect("the law watches the drain on the server double");
    let group = Group::open(harness.ingress(), "reverse-seats", 3).await;
    group.make_ready().await;
    for position in 0..3 {
        assert_eq!(rank_of(&group.commit(position).await), position as u64 + 1);
    }
    let drain = harness
        .ingress()
        .send_workflow_json("DrainBarrierProbe", &group.key, "run", &(&group.key, 3_u64))
        .await
        .expect("the drain is accepted")
        .as_str()
        .to_owned();
    quiescent(&server).await;
    assert!(
        server.outcome(&drain).is_none(),
        "the drain waits for A and B"
    );
    assert!(matches!(
        group.seat(1).await,
        EffectGroupRecordSettlementResponse::Recorded { rank: 2 }
    ));
    quiescent(&server).await;
    assert!(
        server.outcome(&drain).is_none(),
        "the drain still waits for A, the lower blocker"
    );
    assert!(matches!(
        group.seat(0).await,
        EffectGroupRecordSettlementResponse::Recorded { rank: 1 }
    ));
    // Well inside the 60-second inactivity timeout that ended a lost wake.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while server.outcome(&drain).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the drain never lifted after both of its blockers seated"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        matches!(server.outcome(&drain), Some(Ok(_))),
        "both seats lift the barrier: {:?}",
        server.outcome(&drain)
    );
    group.retire().await;
    harness.finish().await;
}

/// The dispatch's one registration: the full position map and the move to
/// ready in one step, with a refusal seated before registration kept, and no
/// ready group made from a retired one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dispatch_registers_its_children_and_makes_the_group_ready_in_one_step() {
    let harness = harness().await;
    let group = Group::open(harness.ingress(), "register-dispatch", 2).await;
    let ids = group.stand_in_ids().await;
    assert_eq!(
        group.register(ids.clone()).await,
        EffectGroupRegisterDispatchResponse::Mismatch,
        "an unadopted group has no dispatch to register"
    );
    group.adopt_stand_in().await;
    assert_eq!(
        group.admit(0, &ids[&0]).await,
        EffectGroupAdmissionResponse::NotYetRecorded,
        "a preparing group records no child id"
    );
    // A generation refusal settles before admission, while the group is
    // still preparing.
    assert_eq!(rank_of(&group.commit(1).await), 1);
    assert!(matches!(
        group.seat(1).await,
        EffectGroupRecordSettlementResponse::Recorded { rank: 1 }
    ));
    let partial = ids
        .iter()
        .filter(|(position, _)| **position == 0)
        .map(|(position, id)| (*position, id.clone()))
        .collect();
    assert_eq!(
        group.register(partial).await,
        EffectGroupRegisterDispatchResponse::Mismatch,
        "a map that misses a position is refused"
    );
    assert_eq!(
        group.register(ids.clone()).await,
        EffectGroupRegisterDispatchResponse::Registered
    );
    assert_eq!(
        group.admit(0, &ids[&0]).await,
        EffectGroupAdmissionResponse::Admitted,
        "the registered id is admitted"
    );
    assert_eq!(
        group.admit(0, "a-replacement-invocation").await,
        EffectGroupAdmissionResponse::AttachExpired,
        "no child runs on a replacement id"
    );
    assert_eq!(
        group.run_from(1).await,
        Some(vec![(1, 1)]),
        "the refusal seated before registration survives it"
    );
    assert_eq!(
        group.register(ids.clone()).await,
        EffectGroupRegisterDispatchResponse::AlreadyRegistered,
        "a redriven registration of the same map is a duplicate"
    );
    let mut different = ids.clone();
    different.insert(0, "a-different-invocation".to_owned());
    assert_eq!(
        group.register(different).await,
        EffectGroupRegisterDispatchResponse::Mismatch,
        "a registration of a different map is refused"
    );
    group.retire().await;

    // Retirement before registration: the late registration never makes a
    // retired group ready.
    let early = Group::open(harness.ingress(), "register-after-retire", 1).await;
    early.adopt_stand_in().await;
    let early_ids = early.stand_in_ids().await;
    early.retire().await;
    assert_eq!(
        early.register(early_ids.clone()).await,
        EffectGroupRegisterDispatchResponse::Retired
    );
    assert_eq!(
        early.admit(0, &early_ids[&0]).await,
        EffectGroupAdmissionResponse::Retired,
        "no child of a retired group is admitted"
    );
    harness.finish().await;
}
