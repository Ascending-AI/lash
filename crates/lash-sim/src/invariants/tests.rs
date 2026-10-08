//! Each global invariant catches a history broken on purpose.
//!
//! The base is a real history: the pending-tool scenario on the durable
//! engine, whose tool defers, whose host resolves the completion and whose
//! turn commits the result. It keeps every invariant. Each test breaks it the
//! way its checker exists to catch and proves that checker fails it.

use std::sync::OnceLock;

use super::*;

const SEED: u64 = 0x5eed_4086;

/// The pending-tool scenario's history on the durable engine, run once per
/// test binary.
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
                        .expect("the durable engine");
                    let recorder = HistoryRecorder::default();
                    crate::runner::prove_pending_tool_completion_for_invariants(
                        &engine, SEED, &recorder,
                    )
                    .await
                    .expect("pending tool proof");
                    let mut history = History::new("fixture/pending-tool", SEED);
                    history.extend_from(&recorder);
                    history
                        .capture_store_with_transcripts("engine", engine.stores())
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

#[test]
fn soak_history_host_admission_catches_unrequested_input_rows() {
    let mut history = History::new("chaos-soak", SEED);
    history.stores.push(StoreSnapshot {
        label: "engine".to_owned(),
        inputs: vec![InputRow {
            table: "pending_turn_inputs".to_owned(),
            session: "s".to_owned(),
            id: "phantom".to_owned(),
            state: Some("completed".to_owned()),
            admitted_run: Some("phantom".to_owned()),
        }],
        ..StoreSnapshot::default()
    });
    assert_caught(&history, "host-admission");
    let clean = host_history();
    assert!(check_with(&clean, &[checker("host-admission")]).passed());
    let mut history = clean.clone();
    history.stores[0].inputs.pop();
    assert_caught(&history, "host-admission");
    for outcome in [
        HostOutcome::Known,
        HostOutcome::Maybe,
        HostOutcome::Refused {
            code: HostRefusalCode::Runtime(lash_core::RuntimeErrorCode::WriterFenced),
        },
    ] {
        let mut history = clean.clone();
        if let Fact::HostOp {
            outcome: recorded, ..
        } = &mut history.records[0].fact
        {
            *recorded = outcome;
        }
        let duplicate = history.stores[0].inputs[0].clone();
        history.stores[0].inputs.push(duplicate);
        assert_caught(&history, "host-admission");
    }
    let mut refused = clean.clone();
    if let Fact::HostOp { outcome, .. } = &mut refused.records[0].fact {
        *outcome = HostOutcome::Refused {
            code: HostRefusalCode::Runtime(lash_core::RuntimeErrorCode::WriterFenced),
        };
    }
    assert_caught(&refused, "host-admission");
    refused.stores[0].inputs.remove(0);
    assert_caught(&refused, "host-admission");
    refused.stores[0].transcripts[0].messages.remove(0);
    refused.stores[0].transcripts[0]
        .messages
        .last_mut()
        .expect("answer")
        .1 = "answer:b;".to_owned();
    assert!(check_with(&refused, &[checker("host-admission")]).passed());
    let mut history = clean.clone();
    history.stores[0].transcripts[0]
        .messages
        .push(("user".to_owned(), "input:phantom;".to_owned()));
    assert_caught(&history, "host-admission");
    let mut history = clean.clone();
    history.stores[0].transcripts[0].messages[0].1 = "input:a;input:a;answer:a;".to_owned();
    assert_caught(&history, "host-admission");
    let mut history = clean;
    history.push(Fact::HostOp {
        op: HostOp::Delete,
        session: "s".to_owned(),
        runs: Vec::new(),
        outcome: HostOutcome::Known,
    });
    history.stores[0].inputs.clear();
    history.stores[0].transcripts.clear();
    let report = check_with(&history, &[checker("host-admission")]);
    assert!(report.passed(), "{}", report.failure());
    assert_eq!(report.downgraded, 2);
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
            || *invariant == "host-admission"
            || *invariant == "transcript-order"
            || *invariant == "redrive-resumes"
        {
            // The pending-tool turn stores no artifact, parks nothing and
            // makes no host start or sequential host sends; each red fixture
            // adds its own.
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

/// On the durable engine the one obligation ledger left is the artifact
/// cleanup outbox (FIG-5191): the pending-tool turn's cleanup guard is the
/// recovery pass's to deliver.
#[test]
fn an_obligation_left_due_or_stalled_untyped_breaks_settled_or_stalled() {
    fn cleanup(history: &mut History) -> &mut ObligationRow {
        history.stores[0]
            .obligations
            .iter_mut()
            .find(|row| row.table == "artifact_cleanup_obligations")
            .expect("the fixture's turn owes its cleanup to the relay")
    }
    // Due is owed in a history with no pass, and left undelivered in one
    // that ran it.
    let mut history = clean();
    assert_eq!(cleanup(&mut history).state.as_deref(), Some("due"));
    assert!(check_with(&history, &[checker("obligations-settled-or-stalled")]).passed());
    assert_caught(&history.after_relay(), "obligations-settled-or-stalled");

    // Delivered by the pass, it is settled; a claim the pass left is not.
    let mut history = clean().after_relay();
    cleanup(&mut history).state = Some("delivered".to_owned());
    assert!(check_with(&history, &[checker("obligations-settled-or-stalled")]).passed());
    cleanup(&mut history).state = Some("claimed".to_owned());
    assert_caught(&history, "obligations-settled-or-stalled");

    // Stalled with a typed reason is surfaced; stalled untyped is lost.
    let mut history = clean().after_relay();
    let row = cleanup(&mut history);
    row.state = Some("stalled".to_owned());
    row.stall_reason = Some("refused".to_owned());
    assert!(check_with(&history, &[checker("obligations-settled-or-stalled")]).passed());
    cleanup(&mut history).stall_reason = None;
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
fn an_input_left_open_or_settled_twice_breaks_input_settlement() {
    let mut history = clean();
    let input = history.stores[0]
        .inputs
        .iter_mut()
        .find(|input| input.table == "pending_turn_inputs")
        .expect("the fixture admitted an input");
    input.state = Some("pending_active".to_owned());
    assert_caught(&history, "input-settles-exactly-once");

    // Cancelled while the run that executes it still runs.
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
        .run_inputs
        .iter()
        .any(|(bound_session, bound, _)| *bound_session == session && *bound == id)
    {
        let run = store.runs[0].run.clone();
        store.run_inputs.push((session, id, run));
    }
    for run in &mut store.runs {
        run.terminal_kind = None;
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
/// frame opened twice, a frame opened from a frame that was not current, a
/// frame named with no open, and a parent cycle each break the frame chain,
/// on the active path or off it (FIG-4134).
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
    // Off the active path: a branch the head never reaches is checked too.
    let beside = |nodes: Vec<GraphNodeRow>| {
        let mut history = clean.clone();
        history.stores[0].graph_nodes.extend(nodes);
        history
    };
    let kept = beside(vec![
        node("side-f", Some(&leaf), "side-f", true),
        node("side-n", Some("side-f"), "side-f", false),
    ]);
    let report = check_with(&kept, &[checker("frames-form-one-chain")]);
    assert!(report.passed(), "{}", report.failure());
    // An off-path node placed in the frame its branch left.
    assert_caught(
        &beside(vec![
            node("side-f", Some(&leaf), "side-f", true),
            node("side-n", Some("side-f"), &first.node_id, false),
        ]),
        "frames-form-one-chain",
    );
    // An off-path parent cycle through a frame open.
    assert_caught(
        &beside(vec![
            node("cycle-f", Some("cycle-n"), "cycle-f", true),
            node("cycle-n", Some("cycle-f"), "cycle-f", false),
        ]),
        "frames-form-one-chain",
    );
}

#[test]
fn checker_failure_retains_history_and_registered_regression() {
    let mut history = clean();
    let (at, call, attempt, failed_attempts_before) = tool_run(&history);
    history.push(Fact::ToolExecuted {
        call,
        attempt,
        failed_attempts_before,
    });
    let report = check(&history);
    assert!(!report.passed());
    assert!(
        CHECKERS
            .iter()
            .any(|checker| checker.invariant() == "effect-at-least-once-window")
    );
    let retained = report
        .history
        .as_ref()
        .expect("failure retains full history");
    assert_eq!(retained.records, history.records);
    assert_eq!(retained.seed, SEED);
    assert!(report.failure().contains("full history:"));
    assert!(report.failure().contains(&format!("#{at:<5}")));
    assert_eq!(
        serde_json::to_value(&retained.stores).unwrap(),
        serde_json::to_value(&history.stores).unwrap()
    );
    let regression: fn() = a_rerun_outside_the_window_breaks_the_effect_window;
    regression();
}

#[test]
fn failure_artifact_keeps_seed_history_and_checker() {
    let mut history = clean();
    let (_, call, attempt, failed_attempts_before) = tool_run(&history);
    history.push(Fact::ToolExecuted {
        call,
        attempt,
        failed_attempts_before,
    });
    let report = check(&history);
    let path = tempfile::tempdir().unwrap();
    let artifact = path.path().join("failure.json");
    std::fs::write(&artifact, serde_json::to_vec(&report).unwrap()).unwrap();
    let restored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifact).unwrap()).unwrap();
    assert_eq!(restored["seed"], SEED);
    assert_eq!(restored["history"], serde_json::to_value(&history).unwrap());
    assert!(
        restored["violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["invariant"] == "effect-at-least-once-window")
    );
    assert!(restored["rendered"].as_array().unwrap().iter().any(|row| {
        row.as_str()
            .unwrap()
            .contains("effect-at-least-once-window")
    }));
}

#[test]
fn soak_history_transcript_order_catches_reversed_markers() {
    let mut history = host_history();
    let report = check_with(&history, &[checker("transcript-order")]);
    assert!(report.passed(), "{}", report.failure());
    assert_eq!(report.observed, [("transcript-order", 1)]);
    history.stores[0].transcripts[0].messages.swap(0, 1);
    let report = assert_caught(&history, "transcript-order");
    assert!(report.failure().contains("host_op"));
    assert!(report.failure().contains("fault"));
    // Both inputs may be in one user message. Byte order remains observable.
    history.stores[0].transcripts[0].messages =
        vec![("user".to_owned(), "input:b;input:a;".to_owned())];
    assert_caught(&history, "transcript-order");
    history.stores[0].transcripts[0].messages[0].1 = "input:a;input:b;".to_owned();
    assert!(check_with(&history, &[checker("transcript-order")]).passed());
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

#[test]
fn soak_history_empty_history_cannot_pass_vacuously() {
    let mut report = check(&History::new("chaos-soak", SEED));
    report.require_observed();
    assert!(!report.passed(), "{}", report.summary());
}

fn host_history() -> History {
    let mut history = History::new("chaos-soak", SEED);
    for run in ["a", "b"] {
        history.push(Fact::HostOp {
            op: HostOp::Send,
            session: "s".to_owned(),
            runs: vec![run.to_owned()],
            outcome: HostOutcome::Known,
        });
    }
    history.push(Fact::Fault {
        kind: FaultKind::Kill,
        detail: "fixture crash".to_owned(),
    });
    history.stores.push(StoreSnapshot {
        label: "engine".to_owned(),
        inputs: ["a", "b"]
            .into_iter()
            .map(|run| InputRow {
                table: "pending_turn_inputs".to_owned(),
                session: "s".to_owned(),
                id: lash_core::PendingTurnInputDraft::keyed_input_id(
                    &lash_core::SessionId::from("s"),
                    run,
                )
                .to_string(),
                state: Some("completed".to_owned()),
                admitted_run: Some(run.to_owned()),
            })
            .collect(),
        transcripts: vec![TranscriptSession {
            session: "s".to_owned(),
            messages: vec![
                ("user".to_owned(), "input:a;".to_owned()),
                ("user".to_owned(), "input:b;".to_owned()),
                ("assistant".to_owned(), "answer:a;answer:b;".to_owned()),
            ],
            ..Default::default()
        }],
        ..Default::default()
    });
    history
}
