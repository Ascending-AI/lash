//! An effect group's own notifications are owned by its group index
//! (FIG-4344, ADR 0099 §2 and §5).
//!
//! READY, RANK, the §5 barrier and a child's cancel fact are answered by the
//! group index that holds the facts they report: a waiter subscribes with an
//! awakeable of its own journal, and the index handler whose state change
//! makes the notice true completes that awakeable in its own journal. No
//! generic wait-index or wait-workflow round trip carries a group-internal
//! notification.
//!
//! These laws read the journals a real width-4 tool batch leaves on the server
//! double, driven by the endpoint's own turn runner:
//!
//! - a seat invokes no other service: it writes its settlement, one exclusive
//!   decision per child, and completes its subscribers in its own journal;
//! - no invocation of the batch reaches the generic durable-wait services for
//!   a group-internal notification, and each child's step-boundary cancel read
//!   is one shared read of the group index;
//! - the dispatch issues every child call before it awaits anything.

use lash_restate_test::protocol::MessageType;

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use super::effect_group_seat_chain::{
    WIDTH, group_invocations, issued_calls, names, run_batch_on, run_width_four_batch,
};

/// The index record's state key: the one decision a seat writes.
const INDEX_STATE_KEY: &str = "effect-group/v1/state";

/// Every call and send `invocation`'s journal issued, as `service/handler`.
fn outgoing(server: &lash_restate_test::RestateTestServer, invocation: &str) -> Vec<String> {
    outgoing_with_parameters(server, invocation)
        .into_iter()
        .map(|(target, _)| target)
        .collect()
}

/// Every call and send `invocation`'s journal issued, as `service/handler`
/// with its JSON parameter.
fn outgoing_with_parameters(
    server: &lash_restate_test::RestateTestServer,
    invocation: &str,
) -> Vec<(String, serde_json::Value)> {
    let parameter = |bytes: &[u8]| serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null);
    server
        .journal(invocation)
        .expect("the invocation's journal is retained")
        .iter()
        .filter_map(|entry| {
            entry
                .call_command()
                .map(|call| {
                    (
                        format!("{}/{}", call.service_name, call.handler_name),
                        parameter(&call.parameter),
                    )
                })
                .or_else(|| {
                    entry.one_way_call_command().map(|send| {
                        (
                            format!("send {}/{}", send.service_name, send.handler_name),
                            parameter(&send.parameter),
                        )
                    })
                })
        })
        .collect()
}

/// The invocations of `handler` on the group index.
pub(super) fn index_invocations(
    server: &lash_restate_test::RestateTestServer,
    handler: &str,
) -> Vec<lash_restate_test::InvocationView> {
    server
        .invocations()
        .into_iter()
        .filter(|view| {
            view.target.starts_with("EffectGroupIndex/")
                && view.target.ends_with(&format!("/{handler}"))
        })
        .collect()
}

/// The count of the batch's invocations by `service/handler`, printed for the
/// report and returned for the ceilings below.
fn inventory(
    server: &lash_restate_test::RestateTestServer,
) -> std::collections::BTreeMap<String, usize> {
    let mut counts = std::collections::BTreeMap::new();
    for view in server.invocations() {
        let mut parts = view.target.split('/');
        let service = parts.next().unwrap_or_default();
        let service = service.split("_g").next().unwrap_or(service);
        let handler = view.target.rsplit('/').next().unwrap_or_default();
        *counts.entry(format!("{service}/{handler}")).or_insert(0) += 1;
    }
    println!(
        "FIG-4344 inventory ({} invocations): {counts:?}",
        counts.values().sum::<usize>()
    );
    counts
}

/// A seat invokes no other service for its notifications: every seat of the
/// batch issues no call and no send, and writes the index record once — the
/// child's one exclusive decision.
pub(super) fn assert_seats_invoke_nothing(server: &lash_restate_test::RestateTestServer) {
    let seats = index_invocations(server, "record_settlement");
    assert_eq!(
        seats.len(),
        WIDTH,
        "each of the {WIDTH} children seats once: {:?}",
        seats.iter().map(|view| &view.target).collect::<Vec<_>>()
    );
    for seat in &seats {
        let issued = outgoing(server, &seat.id);
        assert!(
            issued.is_empty(),
            "seat {} invokes no other service for its notifications: {issued:?}",
            seat.target
        );
        let decisions = server
            .journal(&seat.id)
            .expect("the seat's journal is retained")
            .iter()
            .filter_map(lash_restate_test::JournalEntryView::written_state_key)
            .filter(|key| key == INDEX_STATE_KEY)
            .count();
        assert_eq!(
            decisions, 1,
            "seat {} writes its one exclusive decision once",
            seat.target
        );
    }
}

/// The generic durable-wait handlers a group child still calls: its
/// membership in the turn-cancel gate (ADR 0099 §4), which is the turn's
/// authority, not a group notification.
const CHILD_GATE_MEMBERSHIP: [&str; 2] = [
    "LashDurableWaitIndex/group_child_membership",
    "LashDurableWaitIndex/record_group_child",
];

/// No invocation of the batch is a generic durable-wait round trip for a
/// group-internal notification: no durable-wait key any invocation names is a
/// group notice (the turn's own keys, its cancel gate and its terminal, stay
/// on the generic services), no group index handler calls the durable-wait
/// services, and a child's durable-wait calls are its gate membership. A
/// child's step-boundary cancel read is one shared read of the group index.
pub(super) fn assert_no_generic_group_waits(
    server: &lash_restate_test::RestateTestServer,
    children: &[lash_restate_test::InvocationView],
) {
    inventory(server);
    for view in server.invocations() {
        let issued = outgoing_with_parameters(server, &view.id);
        for (target, parameter) in &issued {
            if !target.contains("LashDurableWait") {
                continue;
            }
            assert!(
                !parameter.to_string().contains("effect-group:"),
                "{} carries no group notification through {target}: {parameter}",
                view.target
            );
            if view.target.starts_with("EffectGroupIndex/") {
                panic!(
                    "group index handler {} calls no durable-wait service for its own notices: {target}",
                    view.target
                );
            }
            if view.target.starts_with("EffectGroupDispatch/") {
                assert!(
                    CHILD_GATE_MEMBERSHIP.contains(&target.as_str()),
                    "{} calls the durable-wait services only for its gate membership: {target}",
                    view.target
                );
            }
        }
    }
    for child in children {
        let calls = issued_calls(server, &child.id);
        let reads = calls
            .iter()
            .filter(|call| call.is_index("child_cancel"))
            .count();
        assert!(
            reads >= 1
                && calls
                    .iter()
                    .all(|call| !call.service.starts_with("LashDurableWaitWorkflow")),
            "child {} reads its cancel fact from the group index, never a wait workflow: {:?}",
            child.target,
            names(&calls)
        );
    }
}

/// The dispatch issues every child call, consecutively, before it registers
/// the group and before it awaits a child: all child calls are issued before
/// any wait.
pub(super) fn assert_children_issued_before_any_wait(
    server: &lash_restate_test::RestateTestServer,
    dispatch: &lash_restate_test::InvocationView,
) {
    let calls = issued_calls(server, &dispatch.id);
    let children = calls
        .iter()
        .enumerate()
        .filter(|(_, call)| {
            call.service.starts_with("EffectGroupDispatch") && call.handler == "child"
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        children.len(),
        WIDTH,
        "the dispatch calls every child: {:?}",
        names(&calls)
    );
    assert!(
        children.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "the child calls are issued together: {:?}",
        names(&calls)
    );
    let registration = calls
        .iter()
        .position(|call| call.is_index("register_dispatch"))
        .unwrap_or_else(|| panic!("the dispatch registers: {:?}", names(&calls)));
    assert!(
        children.iter().all(|&index| index < registration),
        "every child call precedes the registration: {:?}",
        names(&calls)
    );
    let journal = server
        .journal(&dispatch.id)
        .expect("the dispatch's journal is retained");
    let first_child = journal
        .iter()
        .position(|entry| {
            entry
                .call_command()
                .is_some_and(|call| call.handler_name == "child")
        })
        .expect("a child call entry");
    let last_child = journal
        .iter()
        .rposition(|entry| {
            entry
                .call_command()
                .is_some_and(|call| call.handler_name == "child")
        })
        .expect("a child call entry");
    let awaited = journal[first_child..=last_child]
        .iter()
        .filter(|entry| entry.ty == MessageType::CallCompletionNotification)
        .count();
    assert_eq!(
        awaited, 0,
        "no child result lands between the first and the last child call"
    );
}

async fn assert_notification_ownership(always_replay: bool) {
    let harness = run_width_four_batch(always_replay).await;
    let server = harness
        .server_double()
        .expect("the law reads journals on the server double");
    let (dispatch, children) = group_invocations(&server).await;
    assert_seats_invoke_nothing(&server);
    assert_no_generic_group_waits(&server, &children);
    assert_children_issued_before_any_wait(&server, &dispatch);
    harness.finish().await;
}

/// The structural law of the ticket, on the double.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_seat_invokes_no_other_service_for_its_notifications() {
    assert_notification_ownership(false).await;
}

/// The same law where every await suspends and every resumption replays its
/// journal: the subscriptions and their completions are replay-stable.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_seat_invokes_no_other_service_for_its_notifications_under_forced_replay() {
    assert_notification_ownership(true).await;
}

/// A seat crashed after its decision is written and before it completes its
/// subscribers completes each of them on its retry; every rank is still
/// served once and no generic wait appears.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_seat_crashed_before_its_notifications_completes_them_on_retry() {
    for always_replay in [false, true] {
        let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
            unreachable!("in_process names the server double");
        };
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
                seed,
                always_replay,
            })
            .await;
        let server = harness
            .server_double()
            .expect("the law crashes seats on the server double");
        for ty in [
            MessageType::CompleteAwakeableCommand,
            MessageType::OutputCommand,
        ] {
            server.crash_on(
                lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeFrame {
                    ty,
                })
                .service("EffectGroupIndex")
                .handler("record_settlement")
                .within_attempts(1),
            );
        }
        run_batch_on(&harness).await;
        let (_, children) = group_invocations(&server).await;
        assert!(
            index_invocations(&server, "record_settlement")
                .iter()
                .any(|view| view.attempts > 1),
            "the law's crash struck a seat"
        );
        assert_seats_invoke_nothing(&server);
        assert_no_generic_group_waits(&server, &children);
        harness.finish().await;
    }
}
