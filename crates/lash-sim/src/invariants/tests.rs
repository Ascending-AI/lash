//! Each global invariant catches a history broken on purpose.
//!
//! The base is a real history: the pending-tool scenario on the Restate
//! server double, whose tool defers, whose host resolves the completion and
//! whose turn commits the result. It keeps every invariant. Each test breaks
//! it the way its checker exists to catch and proves that checker fails it.

use std::sync::OnceLock;

use super::*;

const SEED: u64 = 0x5eed_4086;

/// The pending-tool scenario's history, run once per test binary.
fn clean() -> History {
    static CLEAN: OnceLock<History> = OnceLock::new();
    CLEAN
        .get_or_init(|| {
            std::thread::spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                runtime.block_on(async {
                    let engine = crate::backend::SimEngine::new(SEED)
                        .await
                        .expect("Restate test backend");
                    let recorder = HistoryRecorder::default();
                    crate::runner::prove_pending_tool_completion_for_invariants(
                        &engine, SEED, &recorder,
                    )
                    .await
                    .expect("pending tool proof");
                    let mut history = History::new("fixture/pending-tool", SEED);
                    history.extend_from(&recorder);
                    history
                        .capture_store_with_transcripts("engine", engine.restate().stores())
                        .await
                        .expect("capture the store");
                    history
                })
            })
            .join()
            .expect("the fixture's thread")
        })
        .clone()
}

fn checker(invariant: &str) -> &'static dyn HistoryChecker {
    *CHECKERS
        .iter()
        .find(|checker| checker.invariant() == invariant)
        .unwrap_or_else(|| panic!("no checker `{invariant}`"))
}

/// `history` breaks `invariant` and only the checker for it says so.
fn assert_caught(history: &History, invariant: &str) -> Report {
    let report = check_with(history, &[checker(invariant)]);
    assert!(
        !report.passed(),
        "`{invariant}` passed a history broken for it: {}",
        report.summary()
    );
    assert!(
        report
            .violations
            .iter()
            .all(|violation| violation.invariant == invariant)
    );
    let failure = report.failure();
    assert!(failure.contains(&format!("{SEED:#018x}")), "{failure}");
    assert!(failure.contains(invariant), "{failure}");
    report
}

fn tool_run(history: &History) -> (usize, CallRef, u32, u64) {
    history
        .records
        .iter()
        .find_map(|record| match &record.fact {
            Fact::ToolExecuted {
                call,
                attempt,
                failed_attempts_before,
            } => Some((record.at, call.clone(), *attempt, *failed_attempts_before)),
            _ => None,
        })
        .expect("the fixture's tool ran")
}

#[test]
fn the_clean_history_keeps_every_invariant_and_gives_each_facts() {
    let history = clean();
    let report = check(&history);
    assert!(report.passed(), "{}", report.failure());
    for (invariant, observed) in &report.observed {
        if *invariant == "artifact-reachable-or-collected"
            || *invariant == "an-existing-start-is-answered-only-to-its-originator"
        {
            // The pending-tool turn stores no artifact and makes no host
            // start; each red fixture adds its own.
            continue;
        }
        assert!(
            *observed > 0,
            "`{invariant}` judged nothing: {}",
            report.summary()
        );
    }
    let (_, call, _, _) = tool_run(&history);
    assert!(!call.identity.0.is_empty(), "the tool saw its call id");
    assert!(
        !call.logical.is_empty(),
        "the tool saw its replay key: {call:?}"
    );
}

/// FIG-4073's shape: a second deferred call in the same scope registers the
/// first one's completion key.
#[test]
fn two_calls_sharing_a_completion_key_break_completion_ownership() {
    let mut history = clean();
    let key = history
        .records
        .iter()
        .find_map(|record| match &record.fact {
            Fact::CompletionRegistered { key, .. } => Some(key.clone()),
            _ => None,
        })
        .expect("the fixture registered a completion");
    let (_, mut call, _, _) = tool_run(&history);
    call.logical.push_str(":second-call");
    call.identity = CallIdentity("call-2".to_owned());
    history.push(Fact::CompletionRegistered { key, call });
    let report = assert_caught(&history, "completion-ownership");
    assert!(
        report
            .failure()
            .contains("distinct calls registered one completion key"),
        "{}",
        report.failure()
    );
}

/// The resolution reaches the transcript as another call's result.
#[test]
fn a_resolution_consumed_by_another_call_breaks_completion_ownership() {
    let mut history = clean();
    for record in &mut history.records {
        if let Fact::CompletionRegistered { call, .. } = &mut record.fact {
            call.identity = CallIdentity("some-other-call".to_owned());
        }
    }
    let report = assert_caught(&history, "completion-ownership");
    assert!(
        report.failure().contains("was consumed by call"),
        "{}",
        report.failure()
    );
}

/// The tool body runs again for the same attempt with no failed engine
/// attempt in between: outside the at-least-once window.
#[test]
fn a_rerun_outside_the_window_breaks_the_effect_window() {
    let mut history = clean();
    let (_, call, attempt, failed_attempts_before) = tool_run(&history);
    history.push(Fact::ToolExecuted {
        call,
        attempt,
        failed_attempts_before,
    });
    assert_caught(&history, "effect-at-least-once-window");

    // The same rerun across a failed attempt is inside the window.
    let mut inside = clean();
    let (_, call, attempt, failed_attempts_before) = tool_run(&inside);
    inside.push(Fact::ToolExecuted {
        call,
        attempt,
        failed_attempts_before: failed_attempts_before + 1,
    });
    assert!(check_with(&inside, &[checker("effect-at-least-once-window")]).passed());

    // A harness effect that ran twice with no attempt dying in its window.
    let mut twice = clean();
    twice.push(Fact::EffectRan {
        effect: "fixture/durable-effect".to_owned(),
        executions: 2,
        unrecorded_attempts: 0,
    });
    assert_caught(&twice, "effect-at-least-once-window");
}

#[test]
fn an_obligation_left_due_or_stalled_untyped_breaks_settled_or_stalled() {
    let mut history = clean();
    let row = history.stores[0]
        .obligations
        .iter_mut()
        .find(|row| row.state.as_deref() == Some("delivered"))
        .expect("the fixture settled an obligation");
    row.state = Some("due".to_owned());
    assert_caught(&history, "obligations-settled-or-stalled");

    // The turn's artifact cleanup guard is the recovery pass's to deliver:
    // due is owed in a history with no pass, and left undelivered in one
    // that ran it.
    let history = clean();
    assert!(
        history.stores[0]
            .obligations
            .iter()
            .any(|row| row.table == "artifact_cleanup_obligations"
                && row.state.as_deref() == Some("due")),
        "the fixture's turn owes its cleanup to the relay"
    );
    assert!(check_with(&history, &[checker("obligations-settled-or-stalled")]).passed());
    assert_caught(&history.after_relay(), "obligations-settled-or-stalled");

    let mut history = clean();
    let row = history.stores[0]
        .obligations
        .iter_mut()
        .find(|row| row.state.as_deref() == Some("delivered"))
        .expect("the fixture settled an obligation");
    row.state = Some("stalled".to_owned());
    row.stall_reason = None;
    assert_caught(&history, "obligations-settled-or-stalled");

    // An admitted input whose admission armed nothing was dropped silently.
    let mut history = clean();
    let row = history.stores[0]
        .obligations
        .iter_mut()
        .find(|row| row.table == "pending_turn_inputs")
        .expect("the fixture admitted an input");
    row.state = None;
    row.id = None;
    assert_caught(&history, "obligations-settled-or-stalled");
}

#[test]
fn an_artifact_nobody_holds_breaks_reachability() {
    let mut history = clean();
    history.stores[0].artifacts.push(ArtifactRow {
        namespace: "fixture".to_owned(),
        artifact_ref: "orphan".to_owned(),
        referrers: Vec::new(),
    });
    assert_caught(&history, "artifact-reachable-or-collected");

    let mut history = clean();
    let referrer = ("execution".to_owned(), "fixture-scope".to_owned());
    history.stores[0].artifacts.push(ArtifactRow {
        namespace: "fixture".to_owned(),
        artifact_ref: "held-by-the-dead".to_owned(),
        referrers: vec![referrer.clone()],
    });
    history.stores[0].fences.insert(referrer.clone());
    assert_caught(&history, "artifact-reachable-or-collected");

    // A typed stall of the ended referrer's cleanup excuses it.
    history.stores[0].cleanups.push(CleanupRow {
        referrer_kind: referrer.0,
        referrer_id: referrer.1,
        state: "stalled".to_owned(),
    });
    assert!(check_with(&history, &[checker("artifact-reachable-or-collected")]).passed());
}

#[test]
fn a_replay_under_another_id_or_a_reused_id_breaks_tool_call_identity() {
    // The replay of one logical call sees another id.
    let mut history = clean();
    let (_, mut call, attempt, failed_attempts_before) = tool_run(&history);
    call.identity = CallIdentity("replayed-under-a-new-id".to_owned());
    history.push(Fact::ToolExecuted {
        call,
        attempt,
        failed_attempts_before: failed_attempts_before + 1,
    });
    assert_caught(&history, "tool-call-identity");

    // Two logical calls run under one id.
    let mut history = clean();
    let (_, mut call, attempt, failed_attempts_before) = tool_run(&history);
    call.logical.push_str(":another-position");
    history.push(Fact::ToolExecuted {
        call,
        attempt,
        failed_attempts_before,
    });
    assert_caught(&history, "tool-call-identity");

    // The transcript commits two calls under one id.
    let mut history = clean();
    let transcript = history.stores[0]
        .transcripts
        .iter_mut()
        .find(|transcript| !transcript.calls.is_empty())
        .expect("the fixture committed a tool call");
    let mut repeated = transcript.calls[0].clone();
    repeated.message += 1;
    transcript.calls.push(repeated);
    assert_caught(&history, "tool-call-identity");
}

#[test]
fn an_input_left_open_or_settled_twice_breaks_input_settlement() {
    let mut history = clean();
    let input = history.stores[0]
        .inputs
        .iter_mut()
        .find(|input| input.table == "pending_turn_inputs")
        .expect("the fixture admitted an input");
    input.state = Some("pending_active".to_owned());
    assert_caught(&history, "input-settles-exactly-once");

    // Left open because its delivery stalled typed: surfaced, not lost.
    let mut stalled = history.clone();
    for input in &mut stalled.stores[0].inputs {
        if input.state.as_deref() == Some("pending_active") {
            input.obligation_state = Some("stalled".to_owned());
        }
    }
    assert!(check_with(&stalled, &[checker("input-settles-exactly-once")]).passed());

    // Cancelled while the root that drives it still runs.
    let mut history = clean();
    let store = &mut history.stores[0];
    let input = store
        .inputs
        .iter_mut()
        .find(|input| input.table == "pending_turn_inputs")
        .expect("the fixture admitted an input");
    input.state = Some("cancelled".to_owned());
    let (session, id) = (input.session.clone(), input.id.clone());
    if !store
        .root_inputs
        .iter()
        .any(|(bound_session, bound, _)| *bound_session == session && *bound == id)
    {
        let root = store.roots[0].root.clone();
        store.root_inputs.push((session, id, root));
    }
    for root in &mut store.roots {
        root.terminal_kind = None;
    }
    assert_caught(&history, "input-settles-exactly-once");
}

#[test]
fn a_violation_renders_its_seed_invariant_and_excerpt() {
    let mut history = clean();
    let (at, call, attempt, failed_attempts_before) = tool_run(&history);
    history.push(Fact::ToolExecuted {
        call,
        attempt,
        failed_attempts_before,
    });
    let report = check(&history);
    let failure = report.failure();
    assert!(failure.contains("global invariant `effect-at-least-once-window`"));
    assert!(failure.contains(&format!("{SEED:#018x}")));
    assert!(failure.contains("trace excerpt:"));
    assert!(failure.contains(&format!("#{at:<5}")), "{failure}");
    let verdict = report.verdict();
    assert!(!verdict.is_passed());
    assert_eq!(verdict.oracle_id, GLOBAL_INVARIANTS_ORACLE);
}

#[test]
fn a_quarantine_entry_covers_only_its_own_violation() {
    let violation = Violation::new("tool-call-identity", "id x named 2 logical calls");
    for entry in quarantine::QUARANTINE {
        assert!(!entry.name.is_empty() && !entry.reason.is_empty());
        assert!(
            CHECKERS
                .iter()
                .any(|checker| checker.invariant() == entry.invariant),
            "quarantine `{}` names no registered invariant",
            entry.name
        );
    }
    assert!(
        quarantine::covering("fixture/none", &violation).is_none_or(|entry| {
            entry.invariant == "tool-call-identity"
                && violation.detail.contains(entry.detail_contains)
        })
    );
}

/// FIG-4111: a host key is global, so an `Existing` answer to a start from one
/// originator for a process another originator started is the leak the
/// registrar's originator comparison exists to prevent.
#[test]
fn an_existing_start_answered_to_another_originator_is_caught() {
    let mut history = clean();
    let answered = |disposition: &str, requested: &str| Fact::ProcessStartAnswered {
        process: "p_0192000000007000800000000000abcd".to_owned(),
        disposition: disposition.to_owned(),
        requested_originator: requested.to_owned(),
        answered_originator: r#"{"type":"session","session_id":"a"}"#.to_owned(),
    };
    history.push(answered(
        "created",
        r#"{"type":"session","session_id":"a"}"#,
    ));
    history.push(answered(
        "existing",
        r#"{"type":"session","session_id":"a"}"#,
    ));
    let kept = check_with(
        &history,
        &[checker(
            "an-existing-start-is-answered-only-to-its-originator",
        )],
    );
    assert!(kept.passed(), "{}", kept.failure());
    history.push(answered(
        "existing",
        r#"{"type":"session","session_id":"b"}"#,
    ));
    let at = history.records.len() - 1;
    let report = assert_caught(
        &history,
        "an-existing-start-is-answered-only-to-its-originator",
    );
    assert_eq!(report.violations[0].records, vec![at]);
}

/// FIG-4110 F8: a node placed in a frame that is not its nearest open, a
/// frame opened twice, a frame opened from a frame that was not current, and
/// a frame named with no open each break the frame chain.
#[test]
fn a_broken_frame_chain_breaks_frame_lineage() {
    let clean = clean();
    let store = &clean.stores[0];
    let (session, leaf) = store
        .heads
        .first()
        .cloned()
        .expect("the fixture has a head");
    let first = store
        .graph_nodes
        .iter()
        .find(|node| node.session == session && node.frame_open)
        .cloned()
        .expect("the fixture's session opens a frame");
    let node = |id: &str, parent: Option<&str>, frame: &str, frame_open: bool| GraphNodeRow {
        session: session.clone(),
        node_id: id.to_owned(),
        parent: parent.map(str::to_owned),
        frame: frame.to_owned(),
        frame_open,
    };
    let extend = |nodes: Vec<GraphNodeRow>| {
        let mut history = clean.clone();
        let store = &mut history.stores[0];
        let head = nodes.last().expect("a new leaf").node_id.clone();
        store.graph_nodes.extend(nodes);
        for (named, leaf) in &mut store.heads {
            if *named == session {
                *leaf = head.clone();
            }
        }
        history
    };

    // A second frame opened in order from the first keeps the chain.
    let kept = extend(vec![
        node("f2", Some(&leaf), "f2", true),
        node("n2", Some("f2"), "f2", false),
    ]);
    let report = check_with(&kept, &[checker("frames-form-one-chain")]);
    assert!(report.passed(), "{}", report.failure());

    // A node placed in the frame it left.
    assert_caught(
        &extend(vec![
            node("f2", Some(&leaf), "f2", true),
            node("n2", Some("f2"), &first.node_id, false),
        ]),
        "frames-form-one-chain",
    );
    // A frame opened twice on one path.
    assert_caught(
        &extend(vec![
            node("f2", Some(&leaf), "f2", true),
            node("n2", Some("f2"), "f2", false),
            node("f2-again", Some("n2"), "f2", true),
        ]),
        "frames-form-one-chain",
    );
    // A frame opened from a frame that was not current: its parent sits in
    // a frame the path already left.
    assert_caught(
        &extend(vec![
            node("f2", Some(&leaf), "f2", true),
            node("n2", Some("f2"), "f2", false),
            node("stale", Some("n2"), &first.node_id, false),
            node("f3", Some("stale"), "f3", true),
        ]),
        "frames-form-one-chain",
    );
    // A frame named with no open.
    assert_caught(
        &extend(vec![node("orphan", Some(&leaf), "no-such-frame", false)]),
        "frames-form-one-chain",
    );
}
