//! A seat resolves its wakes in one round trip (FIG-4269).
//!
//! `record_settlement` is the exclusive index handler a group child seats its
//! rank through. Seating resolves three independent wakes: the opener's rank
//! wake, the drained wake a §5 barrier of a later-committed sibling parks on,
//! and the child's own cancel wake. Each resolve is a call through the
//! session's durable-wait index and its wait workflow. Awaited one after
//! another they cost three sequential round trips while the handler holds
//! the group's exclusive lock, and the drained wake that releases the next
//! sibling waited behind the rank wake's round trip. A width-4 tool batch
//! seats its ranks one after another, so on a live server the load smoke's
//! root 1/8 spent about 0.9 to 1.4 seconds per seat.
//!
//! The law reads the seat's journal on the server double: every resolve call
//! is journaled before the first of them completes, so the handler awaits
//! the three wakes together.

use crate::effect_group::{
    EffectGroupCommitChildRequest, EffectGroupCommitChildResponse, EffectGroupOpenRequest,
    EffectGroupOpenResponse, EffectGroupRecordSettlementRequest,
    EffectGroupRecordSettlementResponse, EffectGroupSettlementTerminal,
};
use lash_restate_test::protocol::MessageType;

use super::effect_group_conformance::{
    HarnessServer, LiveConformanceHarness, witness_child, witness_key, witness_membership,
    witness_shape,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_issues_its_rank_drained_and_cancel_wakes_before_awaiting_any() {
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
    let committed: EffectGroupCommitChildResponse = ingress
        .call_lash_object(
            "EffectGroupIndex",
            &group_key,
            "commit_child",
            &EffectGroupCommitChildRequest {
                replay_key: shape.replay_keys[0].clone(),
            },
        )
        .await
        .expect("the law's first child commits");
    assert!(
        matches!(committed, EffectGroupCommitChildResponse::Committed { .. }),
        "the law's first child commits: {committed:?}"
    );
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
    let calls = journal
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.ty == MessageType::CallCommand)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let completions = journal
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.ty == MessageType::CallCompletionNotification)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let journaled = journal
        .iter()
        .map(|entry| format!("{:?}", entry.ty))
        .collect::<Vec<_>>();
    assert_eq!(
        calls.len(),
        3,
        "a seat resolves its rank, drained and cancel wakes: {journaled:?}"
    );
    assert_eq!(
        completions.len(),
        3,
        "each of the seat's wake resolves completes: {journaled:?}"
    );
    assert!(
        calls.iter().max() < completions.iter().min(),
        "the seat issues every wake resolve before it awaits any, one round trip rather \
         than three: {journaled:?}"
    );

    ingress
        .call_lash_workflow::<_, ()>("EffectGroupDispatch", &group_key, "retire", &group_key)
        .await
        .expect("the law's group retires");
    harness.finish().await;
}
