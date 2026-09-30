//! The invocation chains an effect group's seats and admission cost
//! (FIG-4308, ADR 0099 §5 as amended).
//!
//! A root with a parallel 4-tool batch spent most of its live-server time in
//! Restate invocations issued one after another. Two chains dominated. Every
//! rank seated behind the previous one: the rank was allocated at the seat,
//! so a child's drain and its seat both waited at the §5 barrier for the last
//! lower-committed sibling to seat, whether or not the child declared an
//! intent, and the seat re-read its own commit before writing. And the group's
//! admission went through two exclusive registration handlers on the index
//! every child's own admission queues on.
//!
//! These laws read the journals a real width-4 tool batch leaves on the server
//! double, driven by the endpoint's own turn runner:
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

/// The batch width the ticket's root runs.
const WIDTH: usize = 4;

/// The most calls a child issues strictly after its §4 commit, through its
/// seat: its presentation's admission, its payload and the seat.
const SEAT_CHAIN_BOUND: usize = 3;

/// The group index calls a dispatch issues: adopting the group and
/// registering its children.
const DISPATCH_INDEX_CALLS: usize = 2;

/// One call a journal issued, by its target.
#[derive(Clone, Debug)]
struct IssuedCall {
    service: String,
    handler: String,
    replay_key: Option<String>,
}

impl IssuedCall {
    fn from_command(call: CallCommandMessage) -> Self {
        let replay_key = call
            .headers
            .iter()
            .find(|header| header.key == crate::durable_wait::LASH_REPLAY_KEY_HEADER)
            .map(|header| header.value.clone());
        Self {
            service: call.service_name,
            handler: call.handler_name,
            replay_key,
        }
    }

    fn is_index(&self, handler: &str) -> bool {
        self.service.starts_with("EffectGroupIndex") && self.handler == handler
    }

    /// A park on a sibling's drained wake: the §5 barrier's wait.
    fn is_drained_await(&self) -> bool {
        self.handler == "await_resolution"
            && self
                .replay_key
                .as_deref()
                .is_some_and(|key| key.contains(":drained:"))
    }
}

impl std::fmt::Display for IssuedCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.service, self.handler)
    }
}

/// The calls `invocation`'s journal issued, in journal order.
fn issued_calls(
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

fn names(calls: &[IssuedCall]) -> Vec<String> {
    calls.iter().map(ToString::to_string).collect()
}

/// A width-4 batch of intent-free model tool calls, run through the
/// endpoint's own turn runner on a fresh double: the group every law below
/// reads. The gated schedule holds every member until all four started, so
/// the members settle together — the case a serial seat chain costs most.
async fn run_width_four_batch(always_replay: bool) -> LiveConformanceHarness {
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("in_process names the server double");
    };
    let harness = LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
        seed,
        always_replay,
    })
    .await;
    run_batch_on(&harness).await;
    harness
}

/// The harness's turn runner, except that a finished scenario keeps its
/// completed journals: the laws read them after the batch.
struct JournalKeepingRunner(std::sync::Arc<dyn lash_conformance::ConformanceTurnRunner>);

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
}

async fn run_batch_on(harness: &LiveConformanceHarness) {
    let measured = lash_conformance::measure_gated_tool_batch(
        "seat-chain",
        harness.endpoint_host(),
        harness.law_stores(),
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
async fn group_invocations(
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

/// Every child of the batch seats without waiting on a sibling: it reads no
/// §5 barrier, parks on no drained wake, commits once, and issues at most
/// [`SEAT_CHAIN_BOUND`] calls after its commit through its seat.
fn assert_seat_chains(
    server: &lash_restate_test::RestateTestServer,
    children: &[lash_restate_test::InvocationView],
) {
    for child in children {
        let calls = issued_calls(server, &child.id);
        let journaled = names(&calls);
        assert!(
            !calls.iter().any(|call| call.is_index("drain_blockers")),
            "child {} declared no intent, so it reads no §5 barrier: {journaled:?}",
            child.target
        );
        assert!(
            !calls.iter().any(IssuedCall::is_drained_await),
            "child {} declared no intent, so it parks on no sibling's drained wake: \
             {journaled:?}",
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
fn assert_admission_chain(
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

/// L1: a seated rank waits on no sibling.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_seated_rank_waits_on_no_sibling_and_commits_once() {
    let harness = run_width_four_batch(false).await;
    let server = harness
        .server_double()
        .expect("the law reads journals on the server double");
    let (_, children) = group_invocations(&server).await;
    assert_seat_chains(&server, &children);
    harness.finish().await;
}

/// L2: a 4-child group is admitted through one registration.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_four_child_group_is_admitted_through_one_registration() {
    let harness = run_width_four_batch(false).await;
    let server = harness
        .server_double()
        .expect("the law reads journals on the server double");
    let (dispatch, _) = group_invocations(&server).await;
    assert_admission_chain(&server, &dispatch);
    harness.finish().await;
}

/// L5: both chains hold where every await suspends and every resumption
/// replays its journal: the skipped commit re-read and the unbarriered seat
/// are replay-stable.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_seat_and_admission_chains_hold_under_forced_replay() {
    let harness = run_width_four_batch(true).await;
    let server = harness
        .server_double()
        .expect("the law reads journals on the server double");
    let (dispatch, children) = group_invocations(&server).await;
    assert_seat_chains(&server, &children);
    assert_admission_chain(&server, &dispatch);
    harness.finish().await;
}

/// Every dispatch lane the double serves: the stable lane and the build's
/// own. A crash rule names a service exactly, so a law scripts its crash on
/// each lane and the lane the group runs on fires it.
fn dispatch_services(server: &lash_restate_test::RestateTestServer) -> Vec<String> {
    let lanes = server
        .service_names()
        .into_iter()
        .filter(|service| service.starts_with("EffectGroupDispatch"))
        .collect::<Vec<_>>();
    assert!(!lanes.is_empty(), "the double serves a dispatch lane");
    lanes
}

/// L3 end to end, and the new cut point: a child killed after its §4 commit
/// and its presentation ran, before the presentation's result was durable,
/// replays its commit's recorded answer, commits nothing again and seats its
/// reserved rank.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_child_killed_between_its_commit_and_its_seat_seats_once() {
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("in_process names the server double");
    };
    let harness = LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
        seed,
        always_replay: false,
    })
    .await;
    let server = harness
        .server_double()
        .expect("the law crashes a child on the server double");
    for lane in dispatch_services(&server) {
        server.crash_on(
            lash_restate_test::CrashRule::new(
                lash_restate_test::CrashPoint::BeforeRunResultEnding {
                    suffix: ":present".to_string(),
                },
            )
            .service(lane)
            .handler("child"),
        );
    }
    run_batch_on(&harness).await;
    let (_, children) = group_invocations(&server).await;
    assert!(
        children.iter().any(|child| child.attempts > 1),
        "the law's crash struck a child between its commit and its seat: {:?}",
        children
            .iter()
            .map(|child| (&child.target, child.attempts))
            .collect::<Vec<_>>()
    );
    assert_seat_chains(&server, &children);
    harness.finish().await;
}

/// L6: a dispatch killed after every child call is journaled and before it
/// registers them redrives to the same children and one registration.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_dispatch_killed_before_it_registers_redrives_to_one_registration() {
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("in_process names the server double");
    };
    let harness = LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
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

/// Scripts one crash of the first attempt at each cut point a seat crosses:
/// the index's §4 commit before its answer is recorded, the child after its
/// commit's answer and before its seat, the payload after it is stored, and
/// the seat after its state is written and before its wakes resolve.
fn crash_every_seat_cut(server: &lash_restate_test::RestateTestServer) {
    use lash_restate_test::{CrashPoint, CrashRule};
    for lane in dispatch_services(server) {
        server.crash_on(
            CrashRule::new(CrashPoint::BeforeRunResultEnding {
                suffix: ":present".to_string(),
            })
            .service(lane)
            .handler("child")
            .within_attempts(1),
        );
    }
    for rule in [
        CrashRule::new(CrashPoint::BeforeFrame {
            ty: MessageType::OutputCommand,
        })
        .service("EffectGroupIndex")
        .handler("commit_child"),
        CrashRule::new(CrashPoint::BeforeFrame {
            ty: MessageType::OutputCommand,
        })
        .service("EffectGroupPayload")
        .handler("put"),
        CrashRule::new(CrashPoint::BeforeFrame {
            ty: MessageType::CallCommand,
        })
        .service("EffectGroupIndex")
        .handler("record_settlement"),
    ] {
        server.crash_on(rule.within_attempts(1));
    }
}

async fn assert_batch_survives_every_seat_cut(always_replay: bool) {
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("in_process names the server double");
    };
    let harness = LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
        seed,
        always_replay,
    })
    .await;
    let server = harness
        .server_double()
        .expect("the law crashes seats on the server double");
    crash_every_seat_cut(&server);
    run_batch_on(&harness).await;
    let (dispatch, children) = group_invocations(&server).await;
    let crashed = server
        .invocations()
        .into_iter()
        .filter(|view| view.attempts > 1)
        .map(|view| view.target)
        .collect::<Vec<_>>();
    for handler in ["/commit_child", "/child", "/put", "/record_settlement"] {
        assert!(
            crashed.iter().any(|target| target.ends_with(handler)),
            "a crash struck {handler}: {crashed:?}"
        );
    }
    assert_seat_chains(&server, &children);
    assert_admission_chain(&server, &dispatch);
    harness.finish().await;
}

/// Every cut a seat crosses, crashed once: each child still commits once,
/// seats its reserved rank once, and the batch answers every member.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_batch_crashed_at_every_seat_cut_seats_each_rank_once() {
    assert_batch_survives_every_seat_cut(false).await;
}

/// The same crashes where every await suspends and every resumption replays
/// its journal: a replay serves the recorded commit answer and run.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_batch_crashed_at_every_seat_cut_seats_each_rank_once_under_forced_replay() {
    assert_batch_survives_every_seat_cut(true).await;
}

/// L6, the registration's own cut: `register_dispatch` crashed after it wrote
/// the ready state and before it resolved its ADMIT and READY wakes. Its
/// retry resolves every wake; the opener and every child proceed.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_registration_crashed_before_its_wakes_resolves_every_one_on_retry() {
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("in_process names the server double");
    };
    let harness = LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
        seed,
        always_replay: false,
    })
    .await;
    let server = harness
        .server_double()
        .expect("the law crashes the registration on the server double");
    server.crash_on(
        lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeFrame {
            ty: MessageType::CallCommand,
        })
        .service("EffectGroupIndex")
        .handler("register_dispatch")
        .within_attempts(1),
    );
    run_batch_on(&harness).await;
    let (dispatch, children) = group_invocations(&server).await;
    assert!(
        server
            .invocations()
            .iter()
            .any(|view| view.target.ends_with("/register_dispatch") && view.attempts > 1),
        "the law's crash struck the registration"
    );
    assert_admission_chain(&server, &dispatch);
    assert_seat_chains(&server, &children);
    harness.finish().await;
}
