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

#[derive(Default)]
struct MissingExecutor {
    misses: AtomicUsize,
}

impl lash_core::GroupExecutors for MissingExecutor {
    fn executor_for(
        &self,
        _: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        self.misses.fetch_add(1, Ordering::SeqCst);
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

async fn await_end(harness: &LiveConformanceHarness, group_key: &str, end: End) {
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
                    return;
                }
                if status.completed_successfully() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await;
    assert!(
        completed.is_ok(),
        "a child whose seat is no longer needed ends on its routing miss: {last:?}"
    );
}

async fn uncarried_child(harness: &LiveConformanceHarness, kind: &str, end: Option<End>) {
    let group_key = witness_key(&format!("routing-miss-{kind}-{end:?}"));
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
    let children = vec![child];
    let mut shape = witness_shape(&group_key, &children);
    shape.loser_disposition = lash_core::LoserPolicy::Cancel;
    let missing = Arc::new(MissingExecutor::default());
    harness.install_current_executors(Arc::clone(&missing) as Arc<dyn lash_core::GroupExecutors>);
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

    tokio::time::timeout(BUDGET, async {
        while missing.misses.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a still-needed child keeps retrying without an executor");
    let notice: Option<EffectGroupNotification> = ingress
        .call_lash_object(
            "EffectGroupIndex",
            &group_key,
            "child_cancel",
            &EffectGroupChildCancelRequest { position: 0 },
        )
        .await
        .expect("read the child's still-needed seat");
    assert_eq!(
        notice, None,
        "routing misses do not settle a still-needed child"
    );

    let misses = missing.misses.load(Ordering::SeqCst);
    tokio::time::timeout(BUDGET, async {
        while missing.misses.load(Ordering::SeqCst) <= misses {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the still-needed invocation keeps retrying");
    let status = harness
        .admin_client()
        .workflow_invocation_status("EffectGroupDispatch", &group_key, "child")
        .await
        .expect("read the still-needed invocation")
        .expect("the still-needed invocation exists");
    assert!(
        status.is_still_active(),
        "a still-needed child is never killed: {status:?}"
    );
    assert!(
        missing.misses.load(Ordering::SeqCst) > misses,
        "the still-needed invocation keeps retrying"
    );
    let Some(end) = end else {
        let _: EffectGroupCloseResponse = ingress
            .call_lash_object(
                "EffectGroupIndex",
                &group_key,
                "close",
                &EffectGroupCloseRequest {
                    disposition: lash_core::LoserPolicy::Cancel,
                },
            )
            .await
            .expect("clean up after the still-needed observation");
        await_end(harness, &group_key, End::Cancel).await;
        return;
    };

    let expected = match end {
        End::Cancel => {
            let closed: EffectGroupCloseResponse = ingress
                .call_lash_object(
                    "EffectGroupIndex",
                    &group_key,
                    "close",
                    &EffectGroupCloseRequest {
                        disposition: lash_core::LoserPolicy::Cancel,
                    },
                )
                .await
                .expect("decide the child's cancellation");
            assert_eq!(closed, EffectGroupCloseResponse::Closed);
            EffectGroupNotification::Cancel
        }
        End::Settled | End::Retired => {
            let committed: EffectGroupCommitChildResponse = ingress
                .call_lash_object(
                    "EffectGroupIndex",
                    &group_key,
                    "commit_child",
                    &EffectGroupCommitChildRequest {
                        replay_key: shape.replay_keys[0].clone(),
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
                    &group_key,
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
                    .call_lash_object("EffectGroupIndex", &group_key, "retire", &())
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
            &group_key,
            "child_cancel",
            &EffectGroupChildCancelRequest { position: 0 },
        )
        .await
        .expect("read the durable end");
    assert_eq!(notice, Some(expected));
    println!(
        "routing-miss {kind} {end:?}: durable end observed after {} misses",
        missing.misses.load(Ordering::SeqCst)
    );
    await_end(harness, &group_key, end).await;
    let ended = missing.misses.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        missing.misses.load(Ordering::SeqCst),
        ended,
        "a completed invocation never retries"
    );
}

pub(super) async fn wait_children_end_on_routing_miss(target: HarnessServer) {
    let harness = LiveConformanceHarness::start_on(target).await;
    for kind in ["sleep", "await"] {
        for end in [End::Settled, End::Cancel, End::Retired] {
            uncarried_child(&harness, kind, Some(end)).await;
        }
    }
    harness.finish().await;
}

pub(super) async fn atomic_children_end_on_routing_miss(target: HarnessServer) {
    let harness = LiveConformanceHarness::start_on(target).await;
    for end in [End::Settled, End::Cancel, End::Retired] {
        uncarried_child(&harness, "atomic", Some(end)).await;
    }
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

pub(super) async fn settled_tool_child_ends_on_routing_miss(target: HarnessServer) {
    let harness = LiveConformanceHarness::start_for_tool_children_on(target).await;
    let prefix = format!("routing-miss-{}", harness.run_nonce());
    let group_key = format!("{prefix}-protected-group");
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    SEAT_CUTS.lock_recover().insert(
        group_key.clone(),
        (Arc::clone(&reached), Arc::clone(&release)),
    );
    let fixture = harness.tool_child_law_fixture();
    let law = lash_conformance::registration_macro_support::a_committed_childs_final_is_protected_and_its_drain_is_finished(&fixture, &prefix);
    tokio::time::timeout(BUDGET, async {
        tokio::join!(law, reached.notified());
    })
    .await
    .expect("the protected drain seats before the worker is lost");
    let notice: Option<EffectGroupNotification> = harness
        .ingress()
        .call_lash_object(
            "EffectGroupIndex",
            &group_key,
            "child_cancel",
            &EffectGroupChildCancelRequest { position: 0 },
        )
        .await
        .expect("read the protected child's durable seat");
    assert_eq!(notice, Some(EffectGroupNotification::Settled));
    // The law dropped its opener registration. Lose the worker's retained
    // context too, so its successor has neither a live opener nor a pin.
    harness.release_group_context(&group_key);
    release.notify_one();
    await_end(&harness, &group_key, End::Settled).await;
    let admin = harness.admin_client();
    let ended = admin
        .workflow_invocation_status("EffectGroupDispatch", &group_key, "child")
        .await
        .expect("read the ended invocation")
        .expect("the ended invocation is retained");
    let id = ended.invocation_id();
    let (first, second) = tokio::join!(admin.kill_invocation(&id), admin.kill_invocation(&id));
    first.expect("a second release of the same child is idempotent");
    second.expect("racing releases of the same child are idempotent");
    let notice: Option<EffectGroupNotification> = harness
        .ingress()
        .call_lash_object(
            "EffectGroupIndex",
            &group_key,
            "child_cancel",
            &EffectGroupChildCancelRequest { position: 0 },
        )
        .await
        .expect("read the seat after engine release");
    assert_eq!(
        notice,
        Some(EffectGroupNotification::Settled),
        "release never changes the seat into cancellation"
    );
    SEAT_CUTS.lock_recover().remove(&group_key);
    harness.finish().await;
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
async fn a_settled_tool_child_redelivered_without_an_executor_ends() {
    settled_tool_child_ends_on_routing_miss(HarnessServer::in_process()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn still_needed_children_are_never_killed_on_a_routing_miss() {
    still_needed_children_are_never_killed(HarnessServer::in_process()).await;
}

macro_rules! live_routing_miss_tests {
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
