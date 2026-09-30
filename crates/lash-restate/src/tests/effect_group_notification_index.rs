//! The group index answers its own notices (FIG-4344, ADR 0099 §2 and §5),
//! driven through the index's handlers.
//!
//! - Every transition answers a subscriber recorded before it, and a
//!   subscriber that arrives after it is answered from the record at once:
//!   READY at registration, a refusal, RANK at a seat and not past a hole, the
//!   §5 barrier, a child's cancel fact at its seat or its close decision, and
//!   retirement for everything still outstanding.
//! - A subscription is idempotent by awakeable, bounded by a ceiling the
//!   index refuses past typed, and withdrawn by `unsubscribe`.
//! - A group the index has no record of answers `Absent`.
//!
//! The laws run on the server double, recorded and under forced replay, and
//! on live Restate through the effect-group suite.
//!
//! Each waiter below is a real `await_notice` invocation: it creates an
//! awakeable in its own journal, subscribes it, and is answered by the
//! handler whose state change makes its notice true. A direct `subscribe`
//! with a stand-in awakeable id is used only where the index must answer from
//! the record, or must refuse, without recording anything.

use std::time::Duration;

use crate::effect_group::{
    EffectGroupNotice, EffectGroupNotification, EffectGroupRecordSettlementResponse,
    EffectGroupRefusal, EffectGroupRefusalRequest, EffectGroupRegisterRefusalResponse,
    EffectGroupSubscribeRequest, EffectGroupSubscribeResponse, EffectGroupUnsubscribeRequest,
    subscription_ceiling,
};

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness, await_group_wait};
use super::effect_group_rank_reservation::{Group, rank_of};

/// The state key the index keeps its outstanding subscribers under.
const SUBSCRIPTIONS_STATE_KEY: &str = "effect-group/v1/subscriptions";

async fn harness(always_replay: bool) -> LiveConformanceHarness {
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("in_process names the server double");
    };
    LiveConformanceHarness::start_on(HarnessServer::InProcess {
        seed,
        always_replay,
    })
    .await
}

/// A waiter on `notice`: a real `await_notice` invocation, answered by the
/// index.
fn waiter(
    group: &Group,
    notice: EffectGroupNotice,
) -> tokio::task::JoinHandle<EffectGroupNotification> {
    let ingress = group.ingress.clone();
    let key = group.key.clone();
    tokio::spawn(async move { await_group_wait(&ingress, &key, notice).await })
}

/// Where a law reads the subscriber list the index stores: the double's
/// object state, or a live server's `state` table.
enum Subscribers {
    Double(lash_restate_test::RestateTestServer),
    Live(crate::RestateAdminClient),
}

impl Subscribers {
    fn of(harness: &LiveConformanceHarness) -> Self {
        match harness.server_double() {
            Some(server) => Self::Double(server),
            None => Self::Live(harness.admin_client()),
        }
    }

    /// The stored subscriber list of `group`, if any.
    async fn stored(&self, group: &Group) -> Option<serde_json::Value> {
        match self {
            Self::Double(server) => server
                .object_state("EffectGroupIndex", &group.key)
                .get(SUBSCRIPTIONS_STATE_KEY)
                .map(|bytes| serde_json::from_slice(bytes).expect("the subscriber list decodes")),
            Self::Live(admin) => {
                #[derive(serde::Deserialize)]
                struct Row {
                    value_utf8: String,
                }
                let literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
                let rows: Vec<Row> = admin
                    .query_json(&format!(
                        "SELECT value_utf8 FROM state WHERE service_name = 'EffectGroupIndex' \
                         AND service_key = {} AND key = {}",
                        literal(&group.key),
                        literal(SUBSCRIPTIONS_STATE_KEY),
                    ))
                    .await
                    .expect("read the index's subscriber list");
                rows.into_iter().next().map(|row| {
                    serde_json::from_str(&row.value_utf8).expect("the subscriber list decodes")
                })
            }
        }
    }

    /// The subscribers the index holds for `group`.
    async fn outstanding(&self, group: &Group) -> usize {
        self.stored(group).await.map_or(0, |stamped| {
            stamped["body"]["entries"]
                .as_array()
                .expect("a stored subscriber list holds entries")
                .len()
        })
    }
}

/// Waits until the index holds `count` subscribers for `group`.
async fn await_outstanding(subscribers: &Subscribers, group: &Group, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let held = subscribers.outstanding(group).await;
        if held == count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the index holds {count} subscribers for {}; it holds {held}",
            group.key
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The answer of `waiter`, which its notice's transition must deliver.
async fn answered(
    waiter: tokio::task::JoinHandle<EffectGroupNotification>,
) -> EffectGroupNotification {
    tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .expect("the transition answers its subscriber")
        .expect("the waiter task joins")
}

/// A direct subscription with a stand-in awakeable id.
async fn subscribe(
    group: &Group,
    notice: EffectGroupNotice,
    awakeable_id: &str,
) -> EffectGroupSubscribeResponse {
    group
        .ingress
        .call_lash_object(
            "EffectGroupIndex",
            &group.key,
            "subscribe",
            &EffectGroupSubscribeRequest {
                notice,
                awakeable_id: awakeable_id.to_owned(),
            },
        )
        .await
        .expect("the index answers a subscription")
}

async fn unsubscribe(group: &Group, awakeable_id: &str) {
    group
        .ingress
        .call_lash_object::<_, ()>(
            "EffectGroupIndex",
            &group.key,
            "unsubscribe",
            &EffectGroupUnsubscribeRequest {
                awakeable_id: awakeable_id.to_owned(),
            },
        )
        .await
        .expect("the index withdraws a subscription");
}

fn notified(notification: EffectGroupNotification) -> EffectGroupSubscribeResponse {
    EffectGroupSubscribeResponse::Notified { notification }
}

pub(super) async fn every_transition_answers_its_subscribers(harness: &LiveConformanceHarness) {
    let subscribers = Subscribers::of(harness);
    let group = Group::open(harness.ingress(), "notices", 3).await;

    // Before anything: every notice the group's life will make true.
    let ready = waiter(&group, EffectGroupNotice::Ready);
    let rank_one = waiter(&group, EffectGroupNotice::Rank { rank: 1 });
    let rank_two = waiter(&group, EffectGroupNotice::Rank { rank: 2 });
    let seated = waiter(&group, EffectGroupNotice::ChildCancel { position: 0 });
    let decided = waiter(&group, EffectGroupNotice::ChildCancel { position: 2 });
    let never = waiter(&group, EffectGroupNotice::Rank { rank: 4 });
    await_outstanding(&subscribers, &group, 6).await;
    assert_eq!(
        subscribe(
            &group,
            EffectGroupNotice::Drained { rank: 3 },
            "no-commit-yet"
        )
        .await,
        notified(EffectGroupNotification::Drained),
        "a barrier no committed sibling holds is answered from the record"
    );
    assert_eq!(
        subscribe(&group, EffectGroupNotice::Ready, "late-ready").await,
        EffectGroupSubscribeResponse::Subscribed,
        "a preparing group records a READY subscriber"
    );
    unsubscribe(&group, "late-ready").await;
    await_outstanding(&subscribers, &group, 6).await;

    // READY: the registration answers the opener's and the children's wait.
    group.make_ready().await;
    assert_eq!(answered(ready).await, EffectGroupNotification::Ready);
    await_outstanding(&subscribers, &group, 5).await;
    assert_eq!(
        subscribe(&group, EffectGroupNotice::Ready, "after-ready").await,
        notified(EffectGroupNotification::Ready),
        "a READY subscriber after the registration is answered from the record"
    );

    // A hole: rank 2 seats while rank 1 is reserved and unseated. Nothing
    // reads past it, and the barrier at 3, subscribed once both committed,
    // still owes rank 1.
    assert_eq!(rank_of(&group.commit(0).await), 1);
    assert_eq!(rank_of(&group.commit(1).await), 2);
    let barrier = waiter(&group, EffectGroupNotice::Drained { rank: 3 });
    await_outstanding(&subscribers, &group, 6).await;
    assert!(matches!(
        group.seat(1).await,
        EffectGroupRecordSettlementResponse::Recorded { rank: 2 }
    ));
    await_outstanding(&subscribers, &group, 6).await;
    assert_eq!(
        subscribe(&group, EffectGroupNotice::Rank { rank: 2 }, "past-the-hole").await,
        EffectGroupSubscribeResponse::Subscribed,
        "no rank past the hole is answered"
    );
    unsubscribe(&group, "past-the-hole").await;
    assert_eq!(
        subscribe(
            &group,
            EffectGroupNotice::ChildCancel { position: 1 },
            "seated-child"
        )
        .await,
        notified(EffectGroupNotification::Settled),
        "a seated child's cancel fact is answered from the record"
    );

    // The hole's own seat answers the prefix, the barrier, and the seated
    // child's cancel fact at once.
    assert!(matches!(
        group.seat(0).await,
        EffectGroupRecordSettlementResponse::Recorded { rank: 1 }
    ));
    assert_eq!(answered(rank_one).await, EffectGroupNotification::Rank);
    assert_eq!(answered(rank_two).await, EffectGroupNotification::Rank);
    assert_eq!(answered(barrier).await, EffectGroupNotification::Drained);
    assert_eq!(answered(seated).await, EffectGroupNotification::Settled);
    await_outstanding(&subscribers, &group, 2).await;
    assert_eq!(
        subscribe(
            &group,
            EffectGroupNotice::Drained { rank: 3 },
            "after-drain"
        )
        .await,
        notified(EffectGroupNotification::Drained),
        "a lifted barrier is answered from the record"
    );

    // The close's cancel decision answers the undecided child's cancel fact.
    group.close_cancel().await;
    assert_eq!(answered(decided).await, EffectGroupNotification::Cancel);
    await_outstanding(&subscribers, &group, 1).await;
    assert_eq!(
        subscribe(
            &group,
            EffectGroupNotice::ChildCancel { position: 2 },
            "after-close"
        )
        .await,
        notified(EffectGroupNotification::Cancel),
        "a decided cancel is answered from the record"
    );
    assert_eq!(
        subscribe(&group, EffectGroupNotice::Rank { rank: 3 }, "cancel-seat").await,
        notified(EffectGroupNotification::Rank),
        "the close seated the decided child's rank"
    );

    // Retirement answers what nothing else ever will, and everything after.
    group.retire().await;
    assert_eq!(answered(never).await, EffectGroupNotification::Retired);
    await_outstanding(&subscribers, &group, 0).await;
    for (index, notice) in [
        EffectGroupNotice::Ready,
        EffectGroupNotice::Rank { rank: 9 },
        EffectGroupNotice::Drained { rank: 9 },
        EffectGroupNotice::ChildCancel { position: 0 },
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            subscribe(&group, notice, &format!("after-retire-{index}")).await,
            notified(EffectGroupNotification::Retired),
            "a subscriber after retirement is answered from the tombstone"
        );
    }
}

/// Every transition answers its subscribers, before and after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_transition_answers_its_subscribers_before_and_after() {
    let harness = harness(false).await;
    every_transition_answers_its_subscribers(&harness).await;
    harness.finish().await;
}

/// The same law where every await suspends and every resumption replays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_transition_answers_its_subscribers_before_and_after_under_forced_replay() {
    let harness = harness(true).await;
    every_transition_answers_its_subscribers(&harness).await;
    harness.finish().await;
}

/// A refusal answers every READY subscriber `Refused`, and a later one from
/// the record.
pub(super) async fn a_refusal_answers_its_ready_subscribers(harness: &LiveConformanceHarness) {
    let subscribers = Subscribers::of(harness);
    let group = Group::open(harness.ingress(), "refused", 2).await;
    let ready = waiter(&group, EffectGroupNotice::Ready);
    await_outstanding(&subscribers, &group, 1).await;
    let reason = EffectGroupRefusal::NoExecutor { position: 1 };
    let refused: EffectGroupRegisterRefusalResponse = group
        .ingress
        .call_lash_object(
            "EffectGroupIndex",
            &group.key,
            "register_refusal",
            &EffectGroupRefusalRequest {
                reason: reason.clone(),
            },
        )
        .await
        .expect("the index records the refusal");
    assert_eq!(refused, EffectGroupRegisterRefusalResponse::Refused);
    assert_eq!(
        answered(ready).await,
        EffectGroupNotification::Refused {
            reason: reason.clone()
        }
    );
    await_outstanding(&subscribers, &group, 0).await;
    assert_eq!(
        subscribe(&group, EffectGroupNotice::Ready, "after-refusal").await,
        notified(EffectGroupNotification::Refused { reason })
    );
    group.retire().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_answers_its_ready_subscribers_on_the_double() {
    let harness = harness(false).await;
    a_refusal_answers_its_ready_subscribers(&harness).await;
    harness.finish().await;
}

/// A subscription is idempotent by awakeable, refused typed past the
/// group's ceiling, and withdrawn by `unsubscribe`; a group the index holds
/// no record of answers `Absent`.
pub(super) async fn a_subscription_is_idempotent_bounded_and_withdrawable(
    harness: &LiveConformanceHarness,
) {
    let subscribers = Subscribers::of(harness);
    let group = Group::open(harness.ingress(), "ceiling", 2).await;
    for _ in 0..2 {
        assert_eq!(
            subscribe(&group, EffectGroupNotice::Ready, "one").await,
            EffectGroupSubscribeResponse::Subscribed
        );
    }
    assert_eq!(
        subscribers.outstanding(&group).await,
        1,
        "a retried subscription records nothing twice"
    );
    let ceiling = subscription_ceiling(2);
    for index in 1..ceiling {
        assert_eq!(
            subscribe(&group, EffectGroupNotice::Ready, &format!("fill-{index}")).await,
            EffectGroupSubscribeResponse::Subscribed
        );
    }
    assert_eq!(subscribers.outstanding(&group).await, ceiling);
    assert_eq!(
        subscribe(&group, EffectGroupNotice::Ready, "one-too-many").await,
        EffectGroupSubscribeResponse::Refused {
            outstanding: ceiling
        },
        "the index refuses a subscriber past its ceiling, typed"
    );
    assert_eq!(
        subscribe(&group, EffectGroupNotice::Ready, "one").await,
        EffectGroupSubscribeResponse::Subscribed,
        "a recorded subscriber's retry is still answered at the ceiling"
    );
    unsubscribe(&group, "one").await;
    unsubscribe(&group, "never-subscribed").await;
    for index in 1..ceiling {
        unsubscribe(&group, &format!("fill-{index}")).await;
    }
    assert!(
        subscribers.stored(&group).await.is_none(),
        "the last withdrawal clears the subscriber list"
    );

    let absent = Group::unopened(harness.ingress(), "absent");
    assert_eq!(
        subscribe(&absent, EffectGroupNotice::Ready, "absent").await,
        notified(EffectGroupNotification::Absent)
    );
    group.retire().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscription_is_idempotent_bounded_and_withdrawable_on_the_double() {
    let harness = harness(false).await;
    a_subscription_is_idempotent_bounded_and_withdrawable(&harness).await;
    harness.finish().await;
}
