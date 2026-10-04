//! The invocation chains an effect group's seats and admission cost
//! (FIG-4308, ADR 0099 §5 as amended).
//!
//! A run with a parallel 4-tool batch spent most of its live-server time in
//! Restate invocations issued one after another. Two chains dominated. Every
//! rank seated behind the previous one: the rank was allocated at the seat,
//! so a child's drain and its seat both waited at the §5 barrier for the last
//! lower-committed sibling to seat, whether or not the child declared an
//! intent, and the seat re-read its own commit before writing. And the group's
//! admission went through two exclusive registration handlers on the index
//! every child's own admission queues on.
//!
//! These laws read the journals a real width-4 tool batch leaves on the server
//! double, executed by the endpoint's own turn runner:
//!
//! - a seated rank waits on no sibling: no child of an intent-free batch reads
//!   the §5 barrier or parks on a drained wake, each commits once, and after
//!   its commit it issues at most its presentation's admission, its payload
//!   and its seat;
//! - a 4-child group is admitted through one registration;
//! - the same chains hold under forced replay, across a crash at every cut a
//!   seat crosses, across a dispatch killed before it registers, and across a
//!   registration killed before its wakes resolve.
//!
//! The index-level laws of the reserved rank are in
//! `effect_group_rank_reservation`.

use lash_restate_test::protocol::MessageType;
use lash_restate_test::protocol::generated::CallCommandMessage;

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

/// The batch width the ticket's run executes.
pub(super) const WIDTH: usize = 4;

/// The most calls a child issues strictly after its §4 commit, through its
/// seat: its presentation's admission, its payload and the seat.
const SEAT_CHAIN_BOUND: usize = 3;

/// The group index calls a dispatch issues: adopting the group and
/// registering its children.
const DISPATCH_INDEX_CALLS: usize = 2;

/// One call a journal issued, by its target and its JSON parameter.
#[derive(Clone, Debug)]
pub(super) struct IssuedCall {
    pub(super) service: String,
    pub(super) handler: String,
    pub(super) parameter: serde_json::Value,
}

impl IssuedCall {
    fn from_command(call: CallCommandMessage) -> Self {
        Self {
            parameter: serde_json::from_slice(&call.parameter).unwrap_or(serde_json::Value::Null),
            service: call.service_name,
            handler: call.handler_name,
        }
    }

    pub(super) fn is_index(&self, handler: &str) -> bool {
        self.service.starts_with("EffectGroupIndex") && self.handler == handler
    }

    /// A subscription to the §5 barrier: the wait a seat whose commit
    /// declared an intent parks on before it publishes.
    fn is_drained_subscription(&self) -> bool {
        (self.is_index("subscribe") || self.is_index("await_notice"))
            && self.parameter.to_string().contains(r#""type":"drained""#)
    }
}

impl std::fmt::Display for IssuedCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.service, self.handler)
    }
}

/// The calls `invocation`'s journal issued, in journal order.
pub(super) fn issued_calls(
    server: &lash_restate_test::RestateTestServer,
    invocation: &str,
) -> Vec<IssuedCall> {
    server
        .journal(invocation)
        .expect("the invocation's journal is retained")
        .iter()
        .filter(|entry| entry.ty == MessageType::CallCommand)
        .filter_map(lash_restate_test::JournalEntryView::call_command)
        .map(IssuedCall::from_command)
        .collect()
}

pub(super) fn names(calls: &[IssuedCall]) -> Vec<String> {
    calls.iter().map(ToString::to_string).collect()
}

/// The harness's turn runner, except that a finished scenario keeps its
/// completed journals: the laws read them after the batch.
pub(super) struct JournalKeepingRunner(
    pub(super) std::sync::Arc<dyn lash_conformance::ConformanceTurnRunner>,
);

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for JournalKeepingRunner {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.0.run_turn(admitted, attempt).await;
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.0
            .run_crashed_then_redriven_turn(admitted, crashing, redrive)
            .await;
    }

    async fn await_group_quiescence(&self, group_keys: &[String]) {
        self.0.await_group_quiescence(group_keys).await;
    }
}

pub(super) async fn run_batch_on(harness: &LiveConformanceHarness) {
    run_batch_over(harness, "seat-chain", harness.law_stores()).await;
}

/// The law's width-4 gated batch, its sessions named from `label`, over
/// `stores`.
pub(super) async fn run_batch_over(
    harness: &LiveConformanceHarness,
    label: &str,
    stores: std::sync::Arc<dyn lash_core::StoreSet>,
) {
    let measured = lash_conformance::measure_gated_tool_batch(
        label,
        harness.endpoint_host(),
        stores,
        std::sync::Arc::new(JournalKeepingRunner(harness.turn_runner())),
        &lash_conformance::parallel_model_tool_calls_producer(
            super::tool_batch_parallelism_on_the_double::standard_factories(),
        ),
        WIDTH,
        WIDTH,
    )
    .await;
    assert_eq!(
        measured.leaves_answered, WIDTH,
        "every member of the law's batch answered: {measured:?}"
    );
}

/// The batch's group-child invocations and its dispatch, once every one of
/// them has completed.
pub(super) async fn group_invocations(
    server: &lash_restate_test::RestateTestServer,
) -> (
    lash_restate_test::InvocationView,
    Vec<lash_restate_test::InvocationView>,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let invocations = server.invocations();
        let dispatches = invocations
            .iter()
            .filter(|view| {
                view.target.starts_with("EffectGroupDispatch") && view.target.ends_with("/run")
            })
            .cloned()
            .collect::<Vec<_>>();
        let children = invocations
            .iter()
            .filter(|view| {
                view.target.starts_with("EffectGroupDispatch") && view.target.ends_with("/child")
            })
            .cloned()
            .collect::<Vec<_>>();
        if dispatches.len() == 1
            && children.len() == WIDTH
            && dispatches
                .iter()
                .chain(&children)
                .all(|view| view.status == "completed")
        {
            let dispatch = dispatches.into_iter().next().expect("one dispatch");
            return (dispatch, children);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the law's group did not finish: {} dispatches and {} children, {:?}",
            dispatches.len(),
            children.len(),
            dispatches
                .iter()
                .chain(&children)
                .map(|view| (&view.target, view.status))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Every child of the batch seats without waiting on a sibling: it subscribes
/// to no §5 barrier, commits once, and issues at most
/// [`SEAT_CHAIN_BOUND`] calls after its commit through its seat.
pub(super) fn assert_seat_chains(
    server: &lash_restate_test::RestateTestServer,
    children: &[lash_restate_test::InvocationView],
) {
    for child in children {
        let calls = issued_calls(server, &child.id);
        let journaled = names(&calls);
        assert!(
            !calls.iter().any(IssuedCall::is_drained_subscription),
            "child {} declared no intent, so it subscribes to no §5 barrier: {journaled:?}",
            child.target
        );
        let commits = calls
            .iter()
            .enumerate()
            .filter(|(_, call)| call.is_index("commit_child"))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(
            commits.len(),
            1,
            "child {} commits its final once; its seat does not re-read the commit: \
             {journaled:?}",
            child.target
        );
        let seat = calls
            .iter()
            .position(|call| call.is_index("record_settlement"))
            .unwrap_or_else(|| panic!("child {} seats: {journaled:?}", child.target));
        let chain = seat - commits[0];
        assert!(
            chain <= SEAT_CHAIN_BOUND,
            "child {} issued {chain} calls after its commit through its seat, at most \
             {SEAT_CHAIN_BOUND} (presentation admission, payload, seat): {journaled:?}",
            child.target
        );
    }
}

/// The batch's dispatch adopts its group and registers its children in one
/// index call each.
pub(super) fn assert_admission_chain(
    server: &lash_restate_test::RestateTestServer,
    dispatch: &lash_restate_test::InvocationView,
) {
    let dispatch_calls = issued_calls(server, &dispatch.id);
    let index_calls = dispatch_calls
        .iter()
        .filter(|call| call.service.starts_with("EffectGroupIndex"))
        .collect::<Vec<_>>();
    assert_eq!(
        index_calls.len(),
        DISPATCH_INDEX_CALLS,
        "a width-{WIDTH} dispatch adopts its group and registers its children in one \
         index call each: {:?}",
        names(&dispatch_calls)
    );
}

/// Every dispatch lane the double serves: the stable lane and the build's
/// own. A crash rule names a service exactly, so a law scripts its crash on
/// each lane and the lane the group runs on fires it.
pub(super) fn dispatch_services(server: &lash_restate_test::RestateTestServer) -> Vec<String> {
    let lanes = server
        .service_names()
        .into_iter()
        .filter(|service| service.starts_with("EffectGroupDispatch"))
        .collect::<Vec<_>>();
    assert!(!lanes.is_empty(), "the double serves a dispatch lane");
    lanes
}

/// L6: a dispatch killed after every child call is journaled and before it
/// registers them redrives to the same children and one registration.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_dispatch_killed_before_it_registers_redrives_to_one_registration() {
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("in_process names the server double");
    };
    let harness = LiveConformanceHarness::start_for_tools_on(HarnessServer::InProcess {
        seed,
        always_replay: false,
    })
    .await;
    let server = harness
        .server_double()
        .expect("the law crashes the dispatch on the server double");
    // The dispatch journal: its input, its generation sentinel, the adoption,
    // its preflight run, one call per child, then the registration.
    let registration = 4 + WIDTH;
    for lane in dispatch_services(&server) {
        server.crash_on(
            lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeCommand {
                index: registration,
            })
            .service(lane)
            .handler("run"),
        );
    }
    run_batch_on(&harness).await;
    let (dispatch, children) = group_invocations(&server).await;
    assert!(
        dispatch.attempts > 1,
        "the law's crash struck the dispatch before it registered: {dispatch:?}"
    );
    let calls = issued_calls(&server, &dispatch.id);
    assert!(
        calls
            .iter()
            .rev()
            .find(|call| call.service.starts_with("EffectGroupIndex"))
            .is_some_and(|call| call.is_index("register_dispatch")),
        "the dispatch's last index call is its one registration: {:?}",
        names(&calls)
    );
    assert_admission_chain(&server, &dispatch);
    assert_seat_chains(&server, &children);
    harness.finish().await;
}
