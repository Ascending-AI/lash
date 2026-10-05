//! The replay corpus's Run-behaviour scenarios (FIG-4902 Z04): each one
//! drives a lash session turn through the real handlers on its own
//! in-process server double over SQLite memory, asserts the behaviour's
//! semantic law against the tool bodies' out-of-journal delivery log, and
//! returns what every lash service journaled for the fixture.
//!
//! Every wait is on durable state or a recorded body delivery — never on a
//! sleep — so the generation run and the comparison run record identical
//! journals.

use std::collections::{BTreeMap, BTreeSet};

use super::service_journals::{
    BOUND, BodyDelivery, HandlerJournals, TOOLS_PLUGIN, collect, lash_service, open_scenario_world,
    turn_run_settled, until,
};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, RestateTestServer};
use serde_json::json;

/// The scenarios the corpus records, by fixture directory name.
pub(super) const RUN_SCENARIOS: &[&str] = &[
    "run-partial-result",
    "run-retry-backoff",
    "run-cancel",
    "run-handover",
    "run-plugin-state",
];

/// Runs `scenario` on a fresh double and returns the per-service handler
/// journals its invocations wrote.
pub(super) async fn record(scenario: &str) -> BTreeMap<String, HandlerJournals> {
    match scenario {
        "run-partial-result" => Box::pin(run_partial_result()).await,
        "run-retry-backoff" => run_retry_backoff().await,
        "run-cancel" => Box::pin(run_cancel()).await,
        "run-handover" => run_handover().await,
        "run-plugin-state" => run_plugin_state().await,
        other => panic!("unimplemented replay corpus run scenario `{other}`"),
    }
}

/// Reads back the deployment's journals once every invocation has ended.
async fn finish(backend: &RestateTestBackend) -> BTreeMap<String, HandlerJournals> {
    let server = backend.server().clone();
    server.settle().await;
    until(&server, "a handler never ended", |views| {
        views.iter().all(|view| view.status == "completed")
    })
    .await;
    collect(&server).1
}

/// Whether some invocation journaled the RunCommand `name` and the
/// notification answering it — i.e. the step's result is durable.
fn run_step_completed(server: &RestateTestServer, name: &str) -> bool {
    server.invocations().iter().any(|view| {
        let entries = server.journal(&view.id).unwrap_or_default();
        let pending: BTreeSet<u32> = entries
            .iter()
            .filter(|entry| {
                entry.ty == MessageType::RunCommand && entry.name.as_deref() == Some(name)
            })
            .filter_map(|entry| entry.completion_id())
            .collect();
        entries.iter().any(|entry| {
            entry.ty == MessageType::RunCompletionNotification
                && entry
                    .completion_id()
                    .is_some_and(|id| pending.contains(&id))
                && matches!(entry.run_completion(), Some(Ok(_)))
        })
    })
}

fn deliveries_of<'a>(deliveries: &'a [BodyDelivery], tool: &str) -> Vec<&'a BodyDelivery> {
    deliveries
        .iter()
        .filter(|delivery| delivery.tool == tool)
        .collect()
}

/// Sends `text` as turn `id` without following it, then attaches for its
/// output once the turn's settled lifecycle step is durable. A late attach
/// peeks the resolved terminal, so no TurnTerminal wait registers at the
/// index — attaching while the turn runs races its end over who settles it.
async fn send_settled(
    server: &RestateTestServer,
    session: &lash::LashSession,
    text: &str,
    id: &'static str,
) -> lash::SendHandle {
    let handle = session
        .send(lash::TurnInput::text(text))
        .id(id)
        .await
        .expect("the turn is accepted");
    until(server, "the turn never settled", |_| {
        turn_run_settled(server, id)
    })
    .await;
    handle
}

/// [`send_settled`]'s settled turn output.
async fn settled_output(
    server: &RestateTestServer,
    session: &lash::LashSession,
    text: &str,
    id: &'static str,
) -> lash::TurnOutput {
    tokio::time::timeout(
        BOUND,
        send_settled(server, session, text, id).await.output(),
    )
    .await
    .expect("the turn's output resolves")
    .expect("the turn succeeds")
}

/// L02: two calls in one model reply; "b" blocks on a gate. Once "a"'s
/// attempt record is durable and "b"'s body has entered, a one-shot crash
/// between "b"'s result proposal and its ACK forces a replay: "b"'s body
/// runs twice under the same call id and attempt, "a"'s once.
async fn run_partial_result() -> BTreeMap<String, HandlerJournals> {
    let (backend, _core, session, tools) =
        open_scenario_world(None, "replay-corpus-run-partial").await;
    let server = backend.server().clone();
    let turn = session
        .send(lash::TurnInput::text("pair"))
        .id("run-partial-result")
        .await
        .expect("the turn is accepted");

    tools
        .wait_for_delivery(|d| d.tool == "pair_call" && d.label.as_deref() == Some("a"))
        .await;
    let a_call = deliveries_of(&tools.deliveries(), "pair_call")
        .into_iter()
        .find(|d| d.label.as_deref() == Some("a"))
        .expect("a's body delivered")
        .call_id
        .clone();
    tools
        .wait_for_delivery(|d| d.tool == "pair_call" && d.label.as_deref() == Some("b"))
        .await;
    let b_call = deliveries_of(&tools.deliveries(), "pair_call")
        .into_iter()
        .find(|d| d.label.as_deref() == Some("b"))
        .expect("b's body delivered")
        .call_id
        .clone();
    // A's partial result must be durable before the crash; the gate keeps
    // b's body from finishing until the rule is armed.
    until(
        &server,
        "a's attempt record never became durable",
        |_views| {
            run_step_completed(
                &server,
                &crate::controller::attempt_journal_name(format!("lash:run:{a_call}:attempt:1")),
            )
        },
    )
    .await;
    server.crash_on(CrashRule::new(CrashPoint::BeforeRunResult {
        name: Some(crate::controller::attempt_journal_name(format!(
            "lash:run:{b_call}:attempt:1"
        ))),
    }));
    tools.open_pair_gate();

    until(&server, "the turn never settled", |_| {
        turn_run_settled(&server, "run-partial-result")
    })
    .await;
    let output = tokio::time::timeout(BOUND, turn.output())
        .await
        .expect("the turn's output resolves")
        .expect("the turn succeeds");
    assert_eq!(output.assistant_message(), Some("answered"));
    let deliveries = tools.deliveries();
    let a = deliveries_of(&deliveries, "pair_call")
        .into_iter()
        .filter(|d| d.label.as_deref() == Some("a"))
        .collect::<Vec<_>>();
    let b = deliveries_of(&deliveries, "pair_call")
        .into_iter()
        .filter(|d| d.label.as_deref() == Some("b"))
        .collect::<Vec<_>>();
    assert_eq!(a.len(), 1, "a's body ran once: {deliveries:?}");
    assert_eq!(b.len(), 2, "b's body ran twice: {deliveries:?}");
    assert!(
        b.iter().all(|d| d.call_id == b_call && d.attempt == 1),
        "b's replay kept its call id and attempt: {b:?}"
    );
    finish(&backend).await
}

/// L17: a retryable first attempt is delivered again under the same call id
/// at attempt 2, after its declared backoff.
async fn run_retry_backoff() -> BTreeMap<String, HandlerJournals> {
    let (backend, _core, session, tools) =
        open_scenario_world(None, "replay-corpus-run-retry").await;
    let server = backend.server().clone();
    let output = settled_output(&server, &session, "retry", "run-retry-backoff").await;
    assert_eq!(output.assistant_message(), Some("answered"));
    let all = tools.deliveries();
    let deliveries = deliveries_of(&all, "retry_once");
    assert_eq!(
        deliveries.len(),
        2,
        "one call, two attempts: {deliveries:?}"
    );
    assert_eq!(deliveries[0].attempt, 1);
    assert_eq!(deliveries[1].attempt, 2);
    assert_eq!(
        deliveries[0].call_id, deliveries[1].call_id,
        "a retry keeps its call id"
    );
    finish(&backend).await
}

/// L07/L10: a cancelled run's in-flight body is never adopted — the second
/// identical turn runs a fresh call under a new call id.
async fn run_cancel() -> BTreeMap<String, HandlerJournals> {
    let (backend, _core, session, tools) =
        open_scenario_world(None, "replay-corpus-run-cancel").await;
    let server = backend.server().clone();
    let turn_id = lash::TurnId::from("run-cancel-turn");
    let handle = session
        .send(lash::TurnInput::text("cancel"))
        .id(turn_id.clone())
        .await
        .expect("the turn is accepted");
    tools.wait_for_delivery(|d| d.tool == "gate_call").await;
    let receipt = tokio::time::timeout(
        BOUND,
        session
            .cancel(lash::CancelTarget::Run(turn_id))
            .reason("replay-corpus"),
    )
    .await
    .expect("the cancel applies")
    .expect("the cancel succeeds");
    assert!(
        matches!(receipt, lash::CancelReceipt::Requested { .. }),
        "the running run received the cancellation: {receipt:?}"
    );
    until(&server, "the cancelled turn never settled", |_| {
        turn_run_settled(&server, "run-cancel-turn")
    })
    .await;
    let outcome = tokio::time::timeout(BOUND, handle.outcome())
        .await
        .expect("the cancelled turn settles")
        .expect("the cancelled turn commits");
    assert_eq!(
        outcome.status(),
        lash::TurnStatus::Cancelled,
        "the turn ends cancelled"
    );
    let first = deliveries_of(&tools.deliveries(), "gate_call")
        .iter()
        .map(|d| d.call_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(first.len(), 1, "the cancelled body ran once: {first:?}");

    let output = settled_output(&server, &session, "cancel", "run-cancel-turn-again").await;
    assert_eq!(output.assistant_message(), Some("answered"));
    let second = deliveries_of(&tools.deliveries(), "gate_call")
        .iter()
        .map(|d| d.call_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        second.len(),
        2,
        "the second turn ran a fresh body: {second:?}"
    );
    assert_ne!(
        second[0], second[1],
        "the cancelled Run is not adopted: {second:?}"
    );
    finish(&backend).await
}

/// L09: a run whose journal hits its effect budget goes on in a new
/// invocation — the tool body still ran exactly once.
async fn run_handover() -> BTreeMap<String, HandlerJournals> {
    let (backend, _core, session, tools) =
        open_scenario_world(Some(2), "replay-corpus-run-handover").await;
    let server = backend.server().clone();
    let output = settled_output(&server, &session, "handover", "run-handover").await;
    assert_eq!(output.assistant_message(), Some("answered"));
    assert_eq!(
        deliveries_of(&tools.deliveries(), "count_call").len(),
        1,
        "the tool body ran once across the cut: {:?}",
        tools.deliveries()
    );
    let runs = server
        .invocations()
        .iter()
        .filter(|view| {
            let mut target = view.target.split('/');
            target.next().and_then(lash_service) == Some("LashTurn")
                && target.next_back() == Some("run")
        })
        .count();
    assert!(
        runs >= 2,
        "the run cut by its journal budget into {runs} LashTurn run invocations"
    );
    finish(&backend).await
}

/// Q5/L19: two turns apply one `increment` each to the plugin's `total`
/// state key, and the published value afterwards is exactly 2.
async fn run_plugin_state() -> BTreeMap<String, HandlerJournals> {
    const SESSION: &str = "replay-corpus-run-state";
    let (backend, _core, session, tools) = open_scenario_world(None, SESSION).await;
    let server = backend.server().clone();
    for id in ["run-plugin-state-0", "run-plugin-state-1"] {
        let output = settled_output(&server, &session, "state", id).await;
        assert_eq!(output.assistant_message(), Some("answered"));
    }
    assert_eq!(
        deliveries_of(&tools.deliveries(), "state_add").len(),
        2,
        "each state_add body ran once: {:?}",
        tools.deliveries()
    );
    let store = lash_core::runtime::live_session_view(
        &backend.lash_backend().session_store_factory(),
        &lash_core::SessionId::from(SESSION),
    )
    .await
    .expect("read the session store")
    .expect("an opened session has a store");
    let head = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .expect("load the session head")
    .expect("the completed turns have a head");
    let state = head
        .state
        .plugin_state()
        .expect("the head carries plugin namespaces");
    assert_eq!(
        state.plugins[TOOLS_PLUGIN].values["total"],
        json!(2),
        "two increments publish exactly 2: {state:?}"
    );
    finish(&backend).await
}
