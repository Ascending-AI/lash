//! A seat completes its subscribers in its own journal (FIG-4344).
//!
//! `record_settlement` is the exclusive index handler a group child seats its
//! rank through. Seating can make three notices true at once: the opener's
//! RANK, the §5 barrier a later-committed sibling waits at, and the child's
//! own cancel fact. Each waiter subscribed an awakeable of its own journal
//! with the group index; the seat stores its decision, then the subscriber
//! list it keeps, then completes every satisfied awakeable. A completion is
//! a command of the seat's own journal: the seat calls no other service.
//!
//! The law subscribes three real waiters, seats the child, and reads the
//! seat's journal on the server double.

use std::time::Duration;

use crate::effect_group::{
    EffectGroupCommitChildRequest, EffectGroupCommitChildResponse, EffectGroupNotice,
    EffectGroupNotification, EffectGroupOpenRequest, EffectGroupOpenResponse,
    EffectGroupRecordSettlementRequest, EffectGroupRecordSettlementResponse,
    EffectGroupSettlementTerminal,
};
use lash_restate_test::protocol::MessageType;

use super::effect_group_conformance::{
    HarnessServer, LiveConformanceHarness, await_group_wait, witness_child, witness_key,
    witness_membership, witness_shape,
};

/// The index record's state key and the subscriber list's.
const INDEX_STATE_KEY: &str = "effect-group/v1/state";
const SUBSCRIPTIONS_STATE_KEY: &str = "effect-group/v1/subscriptions";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_completes_its_rank_drained_and_cancel_subscribers_in_its_own_journal() {
    let harness = LiveConformanceHarness::start_on(HarnessServer::in_process()).await;
    let server = harness
        .server_double()
        .expect("the law reads the seat's journal on the server double");
    let ingress = harness.ingress();
    let group_key = witness_key("seat-wakes");
    let children = (0..2)
        .map(|position| witness_child(&group_key, position))
        .collect::<Vec<_>>();
    let shape = witness_shape(&group_key, &children);
    let opened: EffectGroupOpenResponse = ingress
        .call_lash_object(
            "EffectGroupIndex",
            &group_key,
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
    for (position, rank) in [(0usize, 1u64), (1, 2)] {
        let committed: EffectGroupCommitChildResponse = ingress
            .call_lash_object(
                "EffectGroupIndex",
                &group_key,
                "commit_child",
                &EffectGroupCommitChildRequest {
                    replay_key: shape.replay_keys[position].clone(),
                },
            )
            .await
            .expect("the law's child commits");
        assert!(
            matches!(committed, EffectGroupCommitChildResponse::Committed { rank: reserved } if reserved == rank),
            "the law's child {position} commits at rank {rank}: {committed:?}"
        );
    }

    // Three waiters, each on a notice the first seat makes true: the
    // opener's rank 1, the barrier of the sibling ranked 2, and the first
    // child's own cancel fact.
    let notices = [
        (
            EffectGroupNotice::Rank { rank: 1 },
            EffectGroupNotification::Rank,
        ),
        (
            EffectGroupNotice::Drained { rank: 2 },
            EffectGroupNotification::Drained,
        ),
        (
            EffectGroupNotice::ChildCancel { position: 0 },
            EffectGroupNotification::Settled,
        ),
    ];
    let waiters = notices
        .iter()
        .map(|(notice, _)| {
            let ingress = ingress.clone();
            let group_key = group_key.clone();
            let notice = notice.clone();
            tokio::spawn(async move { await_group_wait(&ingress, &group_key, notice).await })
        })
        .collect::<Vec<_>>();
    let subscribe_target = format!("EffectGroupIndex/{group_key}/subscribe");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let subscribed = server
            .invocations()
            .into_iter()
            .filter(|view| view.target == subscribe_target && view.status == "completed")
            .count();
        if subscribed == notices.len() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "every waiter subscribes before the seat; {subscribed} have"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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
        .expect("the law's first child seats");
    assert!(
        matches!(seated, EffectGroupRecordSettlementResponse::Recorded { .. }),
        "the law's first child seats: {seated:?}"
    );

    let seat = server
        .find_invocation(
            "EffectGroupIndex",
            &group_key,
            "record_settlement",
            "completed",
        )
        .expect("the seat's invocation is retained");
    let journal = server
        .journal(&seat.id)
        .expect("the seat's journal is retained");
    let journaled = journal
        .iter()
        .map(|entry| format!("{:?}", entry.ty))
        .collect::<Vec<_>>();
    assert!(
        journal.iter().all(|entry| !matches!(
            entry.ty,
            MessageType::CallCommand | MessageType::OneWayCallCommand
        )),
        "a seat calls no other service for its notifications: {journaled:?}"
    );
    let position_of = |key: &str| {
        journal
            .iter()
            .position(|entry| entry.written_state_key().as_deref() == Some(key))
            .unwrap_or_else(|| panic!("the seat writes {key}: {journaled:?}"))
    };
    let decision = position_of(INDEX_STATE_KEY);
    let kept = position_of(SUBSCRIPTIONS_STATE_KEY);
    let completions = journal
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.ty == MessageType::CompleteAwakeableCommand)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        completions.len(),
        notices.len(),
        "the seat completes each of its three subscribers: {journaled:?}"
    );
    assert!(
        decision < kept && completions.iter().all(|&index| kept < index),
        "the seat stores its decision, then the subscribers it keeps, then completes \
         the rest: {journaled:?}"
    );
    for (waiter, (notice, expected)) in waiters.into_iter().zip(notices) {
        assert_eq!(
            waiter.await.expect("the waiter task joins"),
            expected,
            "the {notice:?} waiter is answered by the seat"
        );
    }

    ingress
        .call_lash_workflow::<_, ()>("EffectGroupDispatch", &group_key, "retire", &group_key)
        .await
        .expect("the law's group retires");
    harness.finish().await;
}
