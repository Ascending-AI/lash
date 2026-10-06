//! Cold-reopen selection laws (FIG-4998).

use super::*;
use lash_restate_test::{JournalEntryView, RestateTestServer};

const BUDGET: Duration = Duration::from_secs(50);

/// Poll `ready` until it holds, within `within`.
async fn wait_for(within: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while !ready() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    true
}

/// Poll `ready` until it holds; panic with `what` past `within`.
async fn wait_until(within: Duration, what: &str, ready: impl FnMut() -> bool) {
    assert!(
        wait_for(within, ready).await,
        "timed out waiting for {what}"
    );
}

/// The turn invocation's view, when the handler attempt exists.
fn turn_view(server: &RestateTestServer) -> Option<lash_restate_test::InvocationView> {
    server
        .invocations()
        .into_iter()
        .find(|view| view.target.ends_with("/run"))
}

/// The turn invocation's journal, when the handler attempt exists.
fn turn_journal(server: &RestateTestServer) -> Option<Vec<JournalEntryView>> {
    turn_view(server).and_then(|view| server.journal(&view.id))
}

/// Whether the turn journal holds the run completion of the journal step
/// named `journal_name`.
fn run_completion_landed(server: &RestateTestServer, journal_name: &str) -> bool {
    let Some(journal) = turn_journal(server) else {
        return false;
    };
    let Some(command) = journal.iter().find(|entry| {
        entry.ty == MessageType::RunCommand && entry.name.as_deref() == Some(journal_name)
    }) else {
        return false;
    };
    journal.iter().any(|entry| {
        entry.ty == MessageType::RunCompletionNotification
            && entry.completion_id() == command.completion_id()
            && entry.run_completion().is_some()
    })
}

/// One event of a record, compactly.
fn brief_event(event: &RunEvent) -> String {
    match event {
        RunEvent::Admitted { round } => format!("Admitted(members={})", round.members.len()),
        RunEvent::AttemptRecorded {
            call_id, attempt, ..
        } => format!("AttemptRecorded({call_id}@{})", attempt.get()),
        RunEvent::Decided {
            call_id, decision, ..
        } => format!("Decided({call_id} {decision:?})"),
        RunEvent::DeclarationsIssued { call_id } => format!("DeclarationsIssued({call_id})"),
        RunEvent::DeclarationsSettled { call_id } => format!("DeclarationsSettled({call_id})"),
        RunEvent::Presented { call_id, .. } => format!("Presented({call_id})"),
        RunEvent::Consumed { call_id } => format!("Consumed({call_id})"),
        RunEvent::Incorporated { call_id } => format!("Incorporated({call_id})"),
        RunEvent::Lifecycle { state } => format!("Lifecycle({state:?})"),
        other => format!("{other:?}"),
    }
}

/// The turn journal, one line per entry: index, type, name, completion slot
/// and, for a run completion holding a record, its first ordinal and events.
fn journal_listing(server: &RestateTestServer) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(view) = turn_view(server) {
        lines.push(format!(
            "invocation {} {} status={} attempts={} suspensions={} last_failure={:?}",
            view.id, view.target, view.status, view.attempts, view.suspensions, view.last_failure
        ));
        if let Some(journal) = server.journal(&view.id) {
            for (index, entry) in journal.iter().enumerate() {
                let mut line = format!(
                    "  {index:>3} {:?} name={:?} cid={:?}",
                    entry.ty,
                    entry.name,
                    entry.completion_id()
                );
                if let Some(Ok(bytes)) = entry.run_completion()
                    && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                    && let Some(record) = value.get("record")
                    && let Ok(record) = serde_json::from_value::<RunRecord>(record.clone())
                {
                    let events: Vec<String> = record.events.iter().map(brief_event).collect();
                    line.push_str(&format!(" first={} events={events:?}", record.first.0));
                }
                lines.push(line);
            }
        }
    }
    lines
}

/// K3/K9 and R4 (FIG-4998): after a cold reopen with C's, D's and A's X
/// durable in that order and B unfinished, the Run decides C, D, A, B.
/// Three durable acknowledgments also pin the owner's queue order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k9_cold_reopen_decides_durable_attempts_in_acknowledgment_order() {
    let calls: Arc<Vec<_>> = Arc::new(
        ["a", "b", "c", "d"]
            .iter()
            .map(|label| (call(label, &Kind::IntentFree), Kind::IntentFree))
            .collect(),
    );
    let [a, b, c, d]: [ToolCallId; 4] = calls
        .iter()
        .map(|(call, _)| call.call_id.clone())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let mut probe = Probe::new(&calls);
    probe.always_replay = true;
    for id in [&a, &b, &c, &d] {
        probe.gates.insert(id.clone(), Arc::new(Gate::default()));
    }
    let (script_a, script_b, script_c, script_d) = (a.clone(), b.clone(), c.clone(), d.clone());
    probe.script = Some(Arc::new(move |server, probe| {
        let (a, b, c, d) = (
            script_a.clone(),
            script_b.clone(),
            script_c.clone(),
            script_d.clone(),
        );
        Box::pin(async move {
            wait_until(BUDGET, "all four gated bodies to enter", || {
                let entered = probe.executions.lock().unwrap();
                [&a, &b, &c, &d]
                    .iter()
                    .all(|id| entered.iter().any(|(executed, _)| executed == *id))
            })
            .await;
            probe.gates[&c].release();
            wait_until(BUDGET, "C's attempt:1 completion", || {
                run_completion_landed(&server, &attempt(&c, 1))
            })
            .await;
            probe.gates[&d].release();
            wait_until(BUDGET, "D's attempt:1 completion", || {
                run_completion_landed(&server, &attempt(&d, 1))
            })
            .await;
            probe.gates[&a].release();
            wait_until(BUDGET, "A's attempt:1 completion", || {
                run_completion_landed(&server, &attempt(&a, 1))
            })
            .await;
            let view = turn_view(&server).expect("the turn invocation exists");
            assert!(
                server.crash(&view.id),
                "the turn attempt crashed (status {})",
                view.status
            );
            probe.gates[&b].release();
        })
    }));
    let probe = Arc::new(probe);
    let driven = drive(
        0x4998,
        Vec::new(),
        Arc::clone(&calls),
        Arc::new(vec![Step::Concurrent]),
        Arc::clone(&probe),
    )
    .await;

    for line in journal_listing(driven.backend.server()) {
        eprintln!("{line}");
    }

    let records = driven.records();
    let decided: Vec<ToolCallId> = records
        .iter()
        .flat_map(|record| record.events.iter())
        .filter_map(|event| match event {
            RunEvent::Decided { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        decided,
        vec![c.clone(), d.clone(), a.clone(), b.clone()],
        "the cold owner decides in X acknowledgment order"
    );
    assert_eq!(probe.executions_of(&a), 1, "a's durable X never repeats");
    assert_eq!(probe.executions_of(&c), 1, "c's durable X never repeats");
    assert_eq!(probe.executions_of(&d), 1, "d's durable X never repeats");
    assert_eq!(probe.executions_of(&b), 2, "b's unfinished X redelivers");
}

/// Whether a durable record of the turn journal decides `call_id`.
fn decision_landed(server: &RestateTestServer, call_id: &ToolCallId) -> bool {
    turn_journal(server).is_some_and(|journal| {
        journal.iter().any(|entry| {
            let Some(Ok(bytes)) = entry.run_completion() else {
                return false;
            };
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|value| serde_json::from_value::<RunRecord>(value.get("record")?.clone()).ok())
                .is_some_and(|record| {
                    record.events.iter().any(|event| {
                        matches!(event, RunEvent::Decided { call_id: decided, .. } if decided == call_id)
                    })
                })
        })
    })
}

/// R4 (FIG-5065): D1 recorded B while A was unfinished. A's X and then the
/// unrelated effect U completed before the crash, with A's D still running.
/// On the cold reopen U's await pops A's acknowledgment outside the owner's
/// queue, so the fresh selection for D1's window picks A. The served D1 is
/// authoritative: B is decided first and the popped A is decided next.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r4_a_served_d_wins_over_a_fresh_choice_and_the_popped_source_decides_next() {
    let calls: Arc<Vec<_>> = Arc::new(
        ["a", "b"]
            .iter()
            .map(|label| (call(label, &Kind::IntentFree), Kind::IntentFree))
            .collect(),
    );
    let [a, b]: [ToolCallId; 2] = calls
        .iter()
        .map(|(call, _)| call.call_id.clone())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let mut probe = Probe::new(&calls);
    for id in [&a, &b] {
        probe.gates.insert(id.clone(), Arc::new(Gate::default()));
    }
    probe
        .after_gates
        .insert(a.clone(), Arc::new(Gate::default()));
    probe.unrelated_gate = Some(Arc::new(Gate::default()));
    let (script_a, script_b) = (a.clone(), b.clone());
    probe.script = Some(Arc::new(move |server, probe| {
        let (a, b) = (script_a.clone(), script_b.clone());
        Box::pin(async move {
            wait_until(BUDGET, "both gated bodies to enter", || {
                let entered = probe.executions.lock().unwrap();
                [&a, &b]
                    .iter()
                    .all(|id| entered.iter().any(|(executed, _)| executed == *id))
            })
            .await;
            probe.gates[&b].release();
            wait_until(BUDGET, "B's durable decision", || {
                decision_landed(&server, &b)
            })
            .await;
            probe.gates[&a].release();
            wait_until(BUDGET, "A's attempt:1 completion", || {
                run_completion_landed(&server, &attempt(&a, 1))
            })
            .await;
            probe.unrelated_gate.as_ref().unwrap().release();
            wait_until(BUDGET, "the unrelated effect's completion", || {
                run_completion_landed(&server, &unrelated())
            })
            .await;
            assert!(
                !decision_landed(&server, &a),
                "A's D is still running at the cut"
            );
            let view = turn_view(&server).expect("the turn invocation exists");
            assert!(
                server.crash(&view.id),
                "the turn attempt crashed (status {})",
                view.status
            );
            probe.after_gates[&a].release();
        })
    }));
    let probe = Arc::new(probe);
    let driven = drive(
        0x5065,
        Vec::new(),
        Arc::clone(&calls),
        Arc::new(vec![Step::ConcurrentBesideUnrelated]),
        Arc::clone(&probe),
    )
    .await;

    for line in journal_listing(driven.backend.server()) {
        eprintln!("{line}");
    }

    let decided: Vec<ToolCallId> = driven
        .records()
        .iter()
        .flat_map(|record| record.events.iter())
        .filter_map(|event| match event {
            RunEvent::Decided { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        decided,
        vec![b.clone(), a.clone()],
        "the served D decides B; the popped A decides next"
    );
    assert_eq!(probe.executions_of(&a), 1, "a's durable X never repeats");
    assert_eq!(probe.executions_of(&b), 1, "b's durable X never repeats");
    assert_eq!(
        probe.handler_attempts.load(Ordering::SeqCst),
        2,
        "one cold reopen"
    );
}
