//! The rank is reserved at the §4 point and a seat only publishes it (ADR 0099
//! §5 as amended by FIG-4308), driven through the index's own handlers and the
//! dispatch's real child handler.
//!
//! - A hole: a lower rank reserved and unseated while higher ranks seat. No
//!   read — point or run, consuming or cursorless — is served past it.
//! - Decision order: a cancel decided while a committed sibling still drains
//!   ranks after that sibling, and is not observable until it seats.
//! - A fallback seat over an already-committed child (an expired attach): its
//!   commit answer is `AlreadyCommitted`, so it waits at the §5 barrier for
//!   every lower committed sibling before it publishes, and then seats its
//!   refusal at the reserved rank. Retirement releases that wait.
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
    EffectGroupCommitChildRequest, EffectGroupCommitChildResponse, EffectGroupNotice,
    EffectGroupNotification, EffectGroupOpenRequest, EffectGroupOpenResponse,
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
                    dispatch_route: "EffectGroupDispatch".to_string(),
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
                    dispatch_route: "EffectGroupDispatch".to_string(),
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

    pub(super) async fn commit(&self, position: usize) -> EffectGroupCommitChildResponse {
        self.ingress
            .call_lash_object(
                "EffectGroupIndex",
                &self.key,
                "commit_child",
                &EffectGroupCommitChildRequest {
                    replay_key: self.shape.replay_keys[position].clone(),
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
                ranks
                    .iter()
                    .map(|served| (served.settlement.sequence, served.settlement.position))
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
                "EffectGroupDispatch",
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
            .call_lash_workflow::<_, ()>("EffectGroupDispatch", &self.key, "retire", &self.key)
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
/// drain even for a reopened caller.
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
    group.reopen().await;
    for rank in [1, 2] {
        assert!(
            matches!(
                group.read(rank, true, true).await,
                EffectGroupReadRankResponse::NotSettled
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
    let EffectGroupReadRankResponse::SettledRun { ranks } = group.read(1, true, true).await else {
        panic!("the reopened caller is served once A seats");
    };
    assert_eq!(
        ranks
            .iter()
            .map(|served| (served.settlement.sequence, served.settlement.position))
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
            "EffectGroupDispatch",
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

/// The A/B/C counterexample, through the actual child handler. A (rank 1) and
/// B (rank 2) committed with intents; C (rank 3) committed. B's invocation is
/// gone and its successor is an expired attach: it takes the fallback seat,
/// whose commit answer is `AlreadyCommitted`, so it waits at the §5 barrier
/// for A before it publishes — main's behaviour, kept. C's barrier holds for
/// A and B both, the closing barrier holds for every unseated child, and B's
/// refusal lands at its reserved rank.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fallback_seat_over_an_earlier_commit_waits_for_every_lower_sibling() {
    let harness = harness().await;
    let server = harness
        .server_double()
        .expect("the law watches the child on the server double");
    let group = Group::open(harness.ingress(), "fallback-barrier", 3).await;
    group.make_ready().await;
    for position in 0..3 {
        assert_eq!(rank_of(&group.commit(position).await), position as u64 + 1);
    }
    assert_eq!(
        group.barrier(3, Duration::from_millis(300)).await,
        None,
        "C's barrier holds while A and B owe their seats"
    );
    assert_eq!(
        group.barrier(4, Duration::from_millis(300)).await,
        None,
        "the closing barrier holds while any commit owes its seat"
    );
    let successor = send_attach_expired_child(&group, 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        server.outcome(&successor).is_none(),
        "B's fallback seat waits for A, whose rank is below its reserved one"
    );
    assert!(
        matches!(
            group.read(2, false, false).await,
            EffectGroupReadRankResponse::NotSettled
        ),
        "B has not published"
    );
    assert!(matches!(
        group.seat(0).await,
        EffectGroupRecordSettlementResponse::Recorded { rank: 1 }
    ));
    completed(&server, &successor).await;
    let EffectGroupReadRankResponse::Settled { settlement, .. } = group.read(2, false, false).await
    else {
        panic!("B's fallback seat published rank 2");
    };
    assert_eq!(settlement.position, 1);
    match &settlement.terminal {
        EffectGroupSettlementTerminal::Failed { error } => assert_eq!(
            error.code,
            lash_core::RuntimeErrorCode::RuntimeEffectGroupChildAttachExpired,
            "B's committed final is replaced by its typed refusal: main's behaviour, kept"
        ),
        other => panic!("B's fallback seat is its refusal, got {other:?}"),
    }
    assert_eq!(
        group.barrier(3, Duration::from_secs(30)).await,
        Some(EffectGroupNotification::Drained),
        "A and B seated, so C's barrier lifted"
    );
    assert_eq!(
        group.barrier(4, Duration::from_millis(300)).await,
        None,
        "C still owes its seat"
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

/// A fallback seat parked at the §5 barrier is released by retirement, not by
/// a seat, and publishes nothing into the retired group.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retirement_releases_a_fallback_seat_parked_at_the_barrier() {
    let harness = harness().await;
    let server = harness
        .server_double()
        .expect("the law watches the child on the server double");
    let group = Group::open(harness.ingress(), "fallback-retired", 2).await;
    group.make_ready().await;
    for position in 0..2 {
        rank_of(&group.commit(position).await);
    }
    let successor = send_attach_expired_child(&group, 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        server.outcome(&successor).is_none(),
        "the fallback seat waits for the unseated lower commit"
    );
    group.retire().await;
    completed(&server, &successor).await;
    assert!(
        matches!(server.outcome(&successor), Some(Ok(_))),
        "the released fallback seat meets the retired group and ends cleanly: {:?}",
        server.outcome(&successor)
    );
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
