//! FIG-4634: routing misses end only after the index no longer needs the seat.

use super::effect_group_conformance::{
    HarnessServer, LiveConformanceHarness, witness_child, witness_dispatch_route, witness_key,
    witness_membership, witness_shape,
};
use super::*;
use crate::effect_group::{
    EffectGroupChildCancelRequest, EffectGroupCloseRequest, EffectGroupCloseResponse,
    EffectGroupCommitChildRequest, EffectGroupCommitChildResponse, EffectGroupCommittedFinal,
    EffectGroupDispatchRequest, EffectGroupNotification, EffectGroupOpenRequest,
    EffectGroupOpenResponse, EffectGroupRecordSettlementRequest,
    EffectGroupRecordSettlementResponse, EffectGroupSettlementTerminal,
};

const BUDGET: Duration = Duration::from_secs(15);

/// Routing misses, counted by child. The endpoint serves every group on its
/// lane, so another group's still-needed child misses here too (FIG-4762):
/// a count that is not keyed by child reads those misses as this child's.
#[derive(Default)]
struct MissingExecutor {
    misses: Mutex<HashMap<String, usize>>,
}

impl MissingExecutor {
    fn misses_of(&self, replay_key: &str) -> usize {
        self.misses
            .lock_recover()
            .get(replay_key)
            .copied()
            .unwrap_or(0)
    }
}

impl lash_core::GroupExecutors for MissingExecutor {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        *self
            .misses
            .lock_recover()
            .entry(envelope.invocation.effect_replay_key().to_owned())
            .or_default() += 1;
        None
    }

    fn routes(&self, _: &RuntimeEffectEnvelope) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug)]
enum End {
    Cancel,
    Settled,
    Retired,
}

async fn await_end(
    harness: &LiveConformanceHarness,
    group_key: &str,
    end: End,
) -> crate::RestateInvocationStatus {
    let mut last = None;
    let completed = tokio::time::timeout(BUDGET, async {
        loop {
            let status = harness.admin_client()
                .workflow_invocation_status("EffectGroupDispatch", group_key, "child")
                .await.expect("read the child's invocation");
            if let Some(status) = status {
                last = Some(format!("{status:?}"));
                if status.completed_with_failure() {
                    assert!(
                        status.completion_failure.as_deref() == Some("[409] killed")
                            || (!matches!(end, End::Settled)
                                && status.completion_failure.as_deref() == Some("[409] cancelled")),
                        "only an authorized engine release may complete the invocation without a reply: {status:?}"
                    );
                    return status;
                }
                if status.completed_successfully() {
                    return status;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await;
    completed.unwrap_or_else(|_| {
        panic!("a child whose seat is no longer needed ends on its routing miss: {last:?}")
    })
}

/// A group whose one child no executor carries, dispatched on the harness's
/// endpoint under its own miss count.
struct Uncarried {
    group_key: String,
    shape: crate::effect_group::EffectGroupShape,
    replay_key: String,
    missing: Arc<MissingExecutor>,
}

impl Uncarried {
    async fn dispatch(harness: &LiveConformanceHarness, kind: &str, label: &str) -> Self {
        let group_key = witness_key(&format!("routing-miss-{kind}-{label}"));
        let mut child = witness_child(&group_key, 0);
        child.command = match kind {
            "sleep" => RuntimeEffectCommand::Sleep {
                spec: lash_core::SleepSpec::For {
                    duration_ms: 3_600_000,
                },
            },
            "await" => {
                let key = harness
                    .endpoint_host()
                    .await_event_key(
                        child.invocation.execution_scope(),
                        AwaitEventWaitIdentity::tool_completion(lash_core::ToolCallId::fixture(
                            "routing-miss",
                        )),
                    )
                    .await
                    .expect("mint the wait child's key");
                RuntimeEffectCommand::AwaitEvent { key }
            }
            "atomic" => child.command,
            _ => unreachable!("the law's three child kinds"),
        };
        let replay_key = child.invocation.effect_replay_key().to_owned();
        let children = vec![child];
        let mut shape = witness_shape(&group_key, &children);
        shape.loser_disposition = lash_core::LoserPolicy::Cancel;
        let missing = Arc::new(MissingExecutor::default());
        harness
            .install_current_executors(Arc::clone(&missing) as Arc<dyn lash_core::GroupExecutors>);
        let ingress = harness.ingress();
        let opened: EffectGroupOpenResponse = ingress
            .call_lash_object(
                "EffectGroupIndex",
                &group_key,
                "open",
                &EffectGroupOpenRequest {
                    shape: shape.clone(),
                    membership: witness_membership(&children),
                    dispatch_route: witness_dispatch_route(),
                    content_checked: false,
                },
            )
            .await
            .expect("open the law's group");
        assert!(matches!(
            opened,
            EffectGroupOpenResponse::OpenedFresh { .. }
        ));
        ingress
            .send_workflow_json(
                &witness_dispatch_route(),
                &group_key,
                "run",
                &crate::Call::new(EffectGroupDispatchRequest {
                    group_key: group_key.clone(),
                }),
            )
            .await
            .expect("dispatch the child");
        Self {
            group_key,
            shape,
            replay_key,
            missing,
        }
    }

    /// This child's own misses on the resolver its group was dispatched under.
    fn misses(&self) -> usize {
        self.missing.misses_of(&self.replay_key)
    }

    async fn misses_past(&self, misses: usize, why: &str) {
        tokio::time::timeout(BUDGET, async {
            while self.misses() <= misses {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{why}"));
    }

    /// Every invocation Restate records for this child, on any lane.
    async fn invocations(
        &self,
        harness: &LiveConformanceHarness,
    ) -> Vec<crate::RestateInvocationStatus> {
        harness
            .admin_client()
            .query_json(&format!(
                "SELECT {} FROM sys_invocation WHERE {} AND target_service_key = {} AND target_handler_name = 'child'",
                crate::ingress::RESTATE_INVOCATION_STATUS_COLUMNS,
                crate::ingress::service_lanes_sql("EffectGroupDispatch"),
                crate::ingress::sql_string_literal(&self.group_key),
            ))
            .await
            .expect("read the child's invocations")
    }

    async fn close(&self, harness: &LiveConformanceHarness) -> EffectGroupCloseResponse {
        harness
            .ingress()
            .call_lash_object(
                "EffectGroupIndex",
                &self.group_key,
                "close",
                &EffectGroupCloseRequest {
                    disposition: lash_core::LoserPolicy::Cancel,
                },
            )
            .await
            .expect("close the law's group")
    }
}

/// A child whose invocation ended, as Restate recorded it at its end.
struct Ended {
    child: Uncarried,
    invocation: crate::RestateInvocationStatus,
    attempts: usize,
}

impl Ended {
    /// Nothing of the child started after its end (FIG-4762), read from what
    /// is recorded rather than from a quiet interval: Restate holds the one
    /// invocation the dispatch minted, completed as it was at its end, and
    /// the endpoint recorded no attempt of this child since.
    async fn assert_nothing_started_since(&self, harness: &LiveConformanceHarness) {
        assert_eq!(
            self.child.invocations(harness).await,
            vec![self.invocation.clone()],
            "no invocation of the child starts after its end, and the ended one stays completed"
        );
        assert_eq!(
            self.child.misses(),
            self.attempts,
            "a completed invocation never retries: {:?}",
            self.invocation
        );
    }
}

async fn uncarried_child(
    harness: &LiveConformanceHarness,
    kind: &str,
    end: Option<End>,
) -> Option<Ended> {
    let child = Uncarried::dispatch(harness, kind, &format!("{end:?}")).await;
    let group_key = &child.group_key;
    let ingress = harness.ingress();

    child
        .misses_past(2, "a still-needed child keeps retrying without an executor")
        .await;
    let notice: Option<EffectGroupNotification> = ingress
        .call_lash_object(
            "EffectGroupIndex",
            group_key,
            "child_cancel",
            &EffectGroupChildCancelRequest { position: 0 },
        )
        .await
        .expect("read the child's still-needed seat");
    assert_eq!(
        notice, None,
        "routing misses do not settle a still-needed child"
    );

    child
        .misses_past(child.misses(), "the still-needed invocation keeps retrying")
        .await;
    let status = harness
        .admin_client()
        .workflow_invocation_status("EffectGroupDispatch", group_key, "child")
        .await
        .expect("read the still-needed invocation")
        .expect("the still-needed invocation exists");
    assert!(
        status.is_still_active(),
        "a still-needed child is never killed: {status:?}"
    );
    let Some(end) = end else {
        child.close(harness).await;
        await_end(harness, group_key, End::Cancel).await;
        return None;
    };

    let expected = match end {
        End::Cancel => {
            assert_eq!(child.close(harness).await, EffectGroupCloseResponse::Closed);
            EffectGroupNotification::Cancel
        }
        End::Settled | End::Retired => {
            let committed: EffectGroupCommitChildResponse = ingress
                .call_lash_object(
                    "EffectGroupIndex",
                    group_key,
                    "commit_child",
                    &EffectGroupCommitChildRequest {
                        replay_key: child.shape.replay_keys[0].clone(),
                        committed: EffectGroupCommittedFinal::Held,
                    },
                )
                .await
                .expect("commit the child's seat");
            assert!(matches!(
                committed,
                EffectGroupCommitChildResponse::Committed { rank: 1 }
            ));
            let seated: EffectGroupRecordSettlementResponse = ingress
                .call_lash_object(
                    "EffectGroupIndex",
                    group_key,
                    "record_settlement",
                    &EffectGroupRecordSettlementRequest {
                        position: 0,
                        terminal: EffectGroupSettlementTerminal::Cancelled,
                    },
                )
                .await
                .expect("seat the child's durable outcome");
            assert!(matches!(
                seated,
                EffectGroupRecordSettlementResponse::Recorded { rank: 1 }
            ));
            if matches!(end, End::Retired) {
                let _: crate::effect_group::EffectGroupRetireResponse = ingress
                    .call_lash_object("EffectGroupIndex", group_key, "retire", &())
                    .await
                    .expect("retire the group");
                EffectGroupNotification::Retired
            } else {
                EffectGroupNotification::Settled
            }
        }
    };
    let notice: Option<EffectGroupNotification> = ingress
        .call_lash_object(
            "EffectGroupIndex",
            group_key,
            "child_cancel",
            &EffectGroupChildCancelRequest { position: 0 },
        )
        .await
        .expect("read the durable end");
    assert_eq!(notice, Some(expected));
    println!(
        "routing-miss {kind} {end:?}: durable end observed after {} misses",
        child.misses()
    );
    let invocation = await_end(harness, group_key, end).await;
    let attempts = child.misses();
    let ended = Ended {
        child,
        invocation,
        attempts,
    };
    ended.assert_nothing_started_since(harness).await;
    Some(ended)
}

/// Runs the law's children beside a still-needed child of another group that
/// keeps missing on the same endpoint, as an earlier law's group does on a
/// shared live server (FIG-4762). Each ended child's recorded facts are read
/// at its end and again after every later child ran; the bystander is never
/// released.
async fn children_end_beside_a_bystander(harness: &LiveConformanceHarness, cases: &[(&str, End)]) {
    let bystander = Uncarried::dispatch(harness, "atomic", "bystander").await;
    bystander
        .misses_past(0, "the bystander retries without an executor")
        .await;
    let mut ended = Vec::new();
    for (kind, end) in cases {
        let child = uncarried_child(harness, kind, Some(*end))
            .await
            .expect("the law's child ended");
        ended.push(child);
    }
    for child in &ended {
        child.assert_nothing_started_since(harness).await;
    }
    let foreign: usize = ended
        .iter()
        .map(|child| child.child.missing.misses_of(&bystander.replay_key))
        .sum();
    println!("routing-miss bystander: {foreign} misses beside the law's children");
    let status = bystander.invocations(harness).await;
    assert!(
        matches!(status.as_slice(), [status] if status.is_still_active()),
        "another group's routing misses never release a still-needed child: {status:?}"
    );
    bystander.close(harness).await;
    await_end(harness, &bystander.group_key, End::Cancel).await;
}

pub(super) async fn wait_children_end_on_routing_miss(target: HarnessServer) {
    let harness = LiveConformanceHarness::start_on(target).await;
    let cases = ["sleep", "await"]
        .into_iter()
        .flat_map(|kind| [End::Settled, End::Cancel, End::Retired].map(|end| (kind, end)))
        .collect::<Vec<_>>();
    children_end_beside_a_bystander(&harness, &cases).await;
    harness.finish().await;
}

pub(super) async fn atomic_children_end_on_routing_miss(target: HarnessServer) {
    let harness = LiveConformanceHarness::start_on(target).await;
    let cases = [End::Settled, End::Cancel, End::Retired].map(|end| ("atomic", end));
    children_end_beside_a_bystander(&harness, &cases).await;
    harness.finish().await;
}

pub(super) async fn still_needed_children_are_never_killed(target: HarnessServer) {
    let harness = LiveConformanceHarness::start_on(target).await;
    for kind in ["sleep", "await", "atomic"] {
        uncarried_child(&harness, kind, None).await;
    }
    harness.finish().await;
}

type SeatCut = (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);
static SEAT_CUTS: LazyLock<Mutex<HashMap<String, SeatCut>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn before_seat(group_key: &str) {
    if let Some((reached, _)) = SEAT_CUTS.lock_recover().get(group_key) {
        reached.notify_one();
    }
}

/// Lose the worker after its settlement became durable and before its invocation ended.
pub(crate) async fn after_seat(group_key: &str) -> HandlerResult<()> {
    let cut = SEAT_CUTS.lock_recover().remove(group_key);
    if let Some((reached, release)) = cut {
        reached.notify_one();
        release.notified().await;
        return Err(std::io::Error::other(
            "the settling worker was lost before completing its invocation",
        )
        .into());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_children_with_a_durable_end_stop_on_a_routing_miss() {
    wait_children_end_on_routing_miss(HarnessServer::in_process()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn atomic_children_with_a_durable_end_stop_on_a_routing_miss() {
    atomic_children_end_on_routing_miss(HarnessServer::in_process()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn still_needed_children_are_never_killed_on_a_routing_miss() {
    still_needed_children_are_never_killed(HarnessServer::in_process()).await;
}

/// The routing-miss laws on a live `restate-server`, registered where the
/// effect-group suite's module filter finds them.
macro_rules! live_routing_miss_tests {
    () => {
        $crate::tests::effect_group_routing_miss::live_routing_miss_tests! {
            live_restate_routing_miss_wait_children_end => wait_children_end_on_routing_miss,
            live_restate_routing_miss_atomic_children_end => atomic_children_end_on_routing_miss,
            live_restate_routing_miss_still_needed_children_are_never_killed => still_needed_children_are_never_killed,
        }
    };
    ($($test:ident => $law:ident),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "requires an isolated Restate server"]
            async fn $test() {
                $crate::tests::effect_group_routing_miss::$law(
                    $crate::tests::effect_group_conformance::HarnessServer::Live,
                ).await;
            }
        )*
    };
}
pub(super) use live_routing_miss_tests;
