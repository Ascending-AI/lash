use super::*;
use std::ffi::OsString;

#[test]
fn completed_workflow_manifest_path_resolves_unset_empty_and_set_values() {
    assert_eq!(completed_workflow_manifest_path(None), None);
    assert_eq!(
        completed_workflow_manifest_path(Some(OsString::new())),
        None
    );
    assert_eq!(
        completed_workflow_manifest_path(Some(OsString::from("/tmp/completed.txt"))),
        Some(PathBuf::from("/tmp/completed.txt"))
    );
}

#[test]
fn captured_settled_shape_is_success() {
    let await_output = serde_json::json!({
        "type": "settled",
        "output": {
            "outcome": {
                "status": "success",
                "payload": {
                    "$lash_tool_value": "untrusted_json",
                    "value": {
                        "first": {"phase": "first"},
                        "second": {"phase": "second"}
                    }
                }
            }
        }
    });
    assert_eq!(
        signal_process_output_value(await_output).unwrap(),
        json!({
            "first": {"phase": "first"},
            "second": {"phase": "second"}
        })
    );
}

#[test]
fn constructed_success_is_unwrapped_and_classified() {
    let await_output = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(json!({
            "first": {"phase": "first"},
            "second": {"phase": "second"}
        })),
    );

    assert_eq!(
        signal_process_output_value(serde_json::to_value(await_output).unwrap()).unwrap(),
        json!({
            "first": {"phase": "first"},
            "second": {"phase": "second"}
        })
    );
}

#[test]
fn failure_and_abandoned_outputs_are_rejected() {
    let failure = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::failure(lash_core::ToolFailure::runtime(
            lash_core::ToolFailureClass::External,
            "signal_failed",
            "signal failed",
        )),
    );
    let abandoned = lash_core::ProcessAwaitOutput::Abandoned {
        evidence: Box::new(lash_core::AbandonEvidence {
            writer: lash_core::AbandonWriter::Producer,
            owner: None,
            epoch_ms: 1,
        }),
        control: None,
    };

    assert!(signal_process_output_value(serde_json::to_value(failure).unwrap()).is_err());
    assert!(signal_process_output_value(serde_json::to_value(abandoned).unwrap()).is_err());
}

// ---------------------------------------------------------------------------
// Recovery-law checker credibility (FIG-608): legal histories pass, every rule
// rejects its own fixture, removing a rule lets that fixture through, and
// incomplete evidence is inconclusive, never a pass.
// ---------------------------------------------------------------------------

const TOOL: &str = witness::WITNESSED_EFFECT_TOOL;
const BOUND_US: i64 = 60_000_000;

/// The shape of a generated legal history.
#[derive(Clone, Copy, Debug)]
struct Shape {
    plain_before: usize,
    recovering: usize,
    effect_workflows: usize,
    extra_retries: usize,
    provider_receipts: usize,
    with_loss: bool,
    with_replay: bool,
}

const BASE: Shape = Shape {
    plain_before: 1,
    recovering: 1,
    effect_workflows: 2,
    extra_retries: 1,
    provider_receipts: 1,
    with_loss: true,
    with_replay: true,
};

struct Builder {
    now: i64,
    history: WitnessHistory,
    scope: LawScope,
    next_attempt: usize,
}

impl Builder {
    fn tick(&mut self) -> i64 {
        self.now += 1_000;
        self.now
    }

    fn submit(&mut self, workflow_id: &str, providers: usize) {
        let at_us = self.tick();
        self.history.submissions.push(SubmissionRow {
            workflow_id: workflow_id.to_string(),
            request_digest: format!("request:{workflow_id}"),
            at_us,
        });
        let at_us = self.tick();
        self.history.acks.push(AckRow {
            workflow_id: workflow_id.to_string(),
            at_us,
        });
        for n in 0..providers {
            let at_us = self.tick();
            self.history.provider_receipts.push(ProviderReceiptRow {
                request_id: format!("chatcmpl-{workflow_id}-{n}"),
                workflow_id: workflow_id.to_string(),
                at_us,
            });
        }
    }

    fn terminal(&mut self, workflow_id: &str, phase: &str, output: Vec<u8>) {
        let at_us = self.tick();
        self.history.terminals.push(TerminalRow {
            workflow_id: workflow_id.to_string(),
            phase: phase.to_string(),
            output,
            at_us,
        });
    }

    fn nemesis(&mut self, kind: &str, subject: &str) {
        let at_us = self.tick();
        self.history.nemesis.push(NemesisRow {
            kind: kind.to_string(),
            subject: subject.to_string(),
            at_us,
        });
    }

    fn attempt(&mut self, workflow_id: &str, key: &str) -> String {
        self.next_attempt += 1;
        let attempt_id = format!("attempt-{}", self.next_attempt);
        let at_us = self.tick();
        self.history.attempts.push(AttemptRow {
            attempt_id: attempt_id.clone(),
            logical_key: witness::effect_logical_key(workflow_id, TOOL, key),
            parent_workflow_id: workflow_id.to_string(),
            call_id: format!("call:{workflow_id}:{key}"),
            request_digest: format!("args:{workflow_id}:{key}"),
            at_us,
        });
        attempt_id
    }

    fn reply(&mut self, attempt_id: &str, workflow_id: &str, key: &str, accepted: bool) {
        let at_us = self.tick();
        self.history.replies.push(ReplyRow {
            attempt_id: attempt_id.to_string(),
            logical_key: witness::effect_logical_key(workflow_id, TOOL, key),
            accepted,
            response_digest: format!("response:{workflow_id}:{key}"),
            at_us,
        });
    }

    /// One witnessed effect: the first attempt commits, a lost attempt gets
    /// no reply and is retried, and extra retries find the commit.
    fn effect(&mut self, workflow_id: &str, key: &str, lose: bool, retries: usize) -> Value {
        let response =
            json!({ "key": key, "value": format!("batch:{key}"), "worker_id": "worker-a" });
        let first = self.attempt(workflow_id, key);
        let at_us = self.tick();
        self.history.commits.push(CommitRow {
            logical_key: witness::effect_logical_key(workflow_id, TOOL, key),
            parent_workflow_id: workflow_id.to_string(),
            first_attempt_id: first.clone(),
            request_digest: format!("args:{workflow_id}:{key}"),
            response: response.to_string().into_bytes(),
            response_digest: format!("response:{workflow_id}:{key}"),
            at_us,
        });
        if lose {
            self.nemesis(
                witness::NEMESIS_LOSS_AFTER_COMMIT,
                &witness::effect_logical_key(workflow_id, TOOL, key),
            );
            let retry = self.attempt(workflow_id, key);
            self.reply(&retry, workflow_id, key, false);
        } else {
            self.reply(&first, workflow_id, key, true);
        }
        for _ in 0..retries {
            let retry = self.attempt(workflow_id, key);
            self.reply(&retry, workflow_id, key, false);
        }
        response
    }
}

fn terminal_bytes(workflow_id: &str, batch: Option<Value>) -> Vec<u8> {
    let mut output = json!({ "workflow_id": workflow_id, "final_text": "done" });
    if let Some(batch) = batch {
        output["final_value"] = json!({ "batch": batch });
    }
    output.to_string().into_bytes()
}

/// A history in which every claimed law holds.
fn legal(shape: Shape) -> (WitnessHistory, LawScope) {
    let mut builder = Builder {
        now: 1_000_000,
        history: WitnessHistory::default(),
        scope: LawScope {
            restart: true,
            recovery_bound_us: BOUND_US,
            effects: Vec::new(),
            loss_keys: Vec::new(),
            replayed: Vec::new(),
        },
        next_attempt: 0,
    };
    let mut saved = Vec::new();
    for n in 0..shape.plain_before {
        let workflow_id = format!("plain-{n}");
        builder.submit(&workflow_id, shape.provider_receipts);
        let output = terminal_bytes(&workflow_id, None);
        builder.terminal(&workflow_id, witness::TERMINAL_OBSERVED, output.clone());
        saved.push((workflow_id, output));
    }
    for n in 0..shape.effect_workflows {
        let workflow_id = format!("batch-{n}");
        let lose = shape.with_loss && n == 0;
        let replay = shape.with_replay && n == 0;
        builder.submit(&workflow_id, shape.provider_receipts);
        let slow = builder.effect(&workflow_id, "slow", lose, shape.extra_retries);
        let fast = builder.effect(&workflow_id, "fast", false, shape.extra_retries);
        for key in ["slow", "fast"] {
            builder.scope.effects.push(ExpectedEffect {
                parent_workflow_id: workflow_id.clone(),
                logical_key: witness::effect_logical_key(&workflow_id, TOOL, key),
                terminal_pointer: format!("/final_value/batch/{key}"),
            });
        }
        if lose {
            builder
                .scope
                .loss_keys
                .push(witness::effect_logical_key(&workflow_id, TOOL, "slow"));
        }
        if replay {
            builder.nemesis(witness::NEMESIS_WORKER_EXIT, &workflow_id);
            builder.scope.replayed.push(workflow_id.clone());
        }
        let output = terminal_bytes(&workflow_id, Some(json!({ "slow": slow, "fast": fast })));
        builder.terminal(&workflow_id, witness::TERMINAL_OBSERVED, output.clone());
        saved.push((workflow_id, output));
    }
    let recovering = (0..shape.recovering)
        .map(|n| format!("recovering-{n}"))
        .collect::<Vec<_>>();
    for workflow_id in &recovering {
        builder.submit(workflow_id, shape.provider_receipts);
    }
    builder.nemesis(witness::NEMESIS_RESTART_BEGIN, "cluster");
    builder.nemesis(witness::NEMESIS_RESTART_COMPLETE, "cluster");
    for (workflow_id, output) in saved {
        builder.terminal(&workflow_id, witness::TERMINAL_REATTACHED, output);
    }
    for workflow_id in &recovering {
        builder.terminal(
            workflow_id,
            witness::TERMINAL_OBSERVED,
            terminal_bytes(workflow_id, None),
        );
    }
    builder.history.snapshot_at_us = builder.tick();
    (builder.history, builder.scope)
}

fn verdicts(history: &WitnessHistory, scope: &LawScope) -> Vec<(Law, Verdict)> {
    check_recovery_laws(history, scope)
}

fn all_pass(history: &WitnessHistory, scope: &LawScope) -> bool {
    verdicts(history, scope)
        .iter()
        .all(|(_, verdict)| *verdict == Verdict::Pass)
}

fn key(workflow_id: &str, name: &str) -> String {
    witness::effect_logical_key(workflow_id, TOOL, name)
}

fn restart_complete_at(history: &WitnessHistory) -> i64 {
    history
        .nemesis
        .iter()
        .find(|row| row.kind == witness::NEMESIS_RESTART_COMPLETE)
        .map(|row| row.at_us)
        .expect("legal history has a restart")
}

#[derive(Debug, Eq, PartialEq)]
enum Rejects {
    Violation,
    Inconclusive,
}

/// The fixture that rule `name` alone must reject.
fn rule_fixture(name: &str) -> (WitnessHistory, LawScope, Rejects) {
    let (mut history, mut scope) = legal(BASE);
    let expect = match name {
        "restart_witnessed" => {
            history
                .nemesis
                .retain(|row| row.kind != witness::NEMESIS_RESTART_COMPLETE);
            Rejects::Inconclusive
        }
        "observed_terminals_agree" => {
            let at_us = history.snapshot_at_us;
            history.terminals.push(TerminalRow {
                workflow_id: "plain-0".to_string(),
                phase: witness::TERMINAL_OBSERVED.to_string(),
                output: b"{\"final_text\":\"other\"}".to_vec(),
                at_us,
            });
            history.snapshot_at_us += 1;
            Rejects::Violation
        }
        "terminals_reattached_after_restart" => {
            history.terminals.retain(|row| {
                !(row.workflow_id == "plain-0" && row.phase == witness::TERMINAL_REATTACHED)
            });
            Rejects::Inconclusive
        }
        "terminal_bytes_survive_restart" => {
            let row = history
                .terminals
                .iter_mut()
                .find(|row| {
                    row.workflow_id == "plain-0" && row.phase == witness::TERMINAL_REATTACHED
                })
                .expect("plain-0 reattached");
            row.output.push(b' ');
            Rejects::Violation
        }
        "unfinished_work_recovers_in_bound" => {
            let late = restart_complete_at(&history) + BOUND_US + 1;
            let row = history
                .terminals
                .iter_mut()
                .find(|row| row.workflow_id == "recovering-0")
                .expect("recovering-0 observed");
            row.at_us = late;
            history.snapshot_at_us = late + 1;
            Rejects::Violation
        }
        "logical_key_stable" => {
            let fast = key("batch-1", "fast");
            let retry = history
                .attempts
                .iter_mut()
                .rfind(|row| row.logical_key == fast)
                .expect("fast retry");
            retry.call_id = "call:regenerated".to_string();
            Rejects::Violation
        }
        "request_bytes_identical" => {
            let fast = key("batch-1", "fast");
            let retry = history
                .attempts
                .iter_mut()
                .rfind(|row| row.logical_key == fast)
                .expect("fast retry");
            retry.request_digest = "args:drifted".to_string();
            Rejects::Violation
        }
        "at_most_one_commit" => {
            let fast = key("batch-1", "fast");
            let mut second = history
                .commits
                .iter()
                .find(|row| row.logical_key == fast)
                .cloned()
                .expect("fast commit");
            second.at_us = history.snapshot_at_us;
            history.snapshot_at_us += 1;
            history.commits.push(second);
            Rejects::Violation
        }
        "replies_carry_the_commit" => {
            let fast = key("batch-1", "fast");
            let reply = history
                .replies
                .iter_mut()
                .rfind(|row| row.logical_key == fast)
                .expect("fast retry reply");
            reply.response_digest = "response:fresh-effect".to_string();
            Rejects::Violation
        }
        "commit_names_its_accepted_attempt" => {
            let fast = key("batch-1", "fast");
            let commit = history
                .commits
                .iter_mut()
                .find(|row| row.logical_key == fast)
                .expect("fast commit");
            commit.first_attempt_id = "attempt-ghost".to_string();
            Rejects::Violation
        }
        "expected_receipts_present" => {
            let fast = key("batch-1", "fast");
            history.attempts.retain(|row| row.logical_key != fast);
            history.commits.retain(|row| row.logical_key != fast);
            history.replies.retain(|row| row.logical_key != fast);
            Rejects::Violation
        }
        "lost_attempt_retried" => {
            let slow = key("batch-0", "slow");
            let last = history
                .attempts
                .iter()
                .filter(|row| row.logical_key == slow)
                .map(|row| row.at_us)
                .chain(
                    history
                        .replies
                        .iter()
                        .filter(|row| row.logical_key == slow)
                        .map(|row| row.at_us),
                )
                .max()
                .expect("slow attempts");
            let loss = history
                .nemesis
                .iter_mut()
                .find(|row| row.kind == witness::NEMESIS_LOSS_AFTER_COMMIT)
                .expect("loss nemesis");
            loss.at_us = last + 1;
            Rejects::Violation
        }
        "causal_evidence_present" => {
            history.provider_receipts.clear();
            history.attempts.clear();
            history.commits.clear();
            history.replies.clear();
            scope = LawScope::causal_only();
            Rejects::Inconclusive
        }
        "receipts_name_known_parents" => {
            let at_us = history.snapshot_at_us;
            history.provider_receipts.push(ProviderReceiptRow {
                request_id: "chatcmpl-orphan".to_string(),
                workflow_id: "never-submitted".to_string(),
                at_us,
            });
            Rejects::Violation
        }
        "receipts_follow_their_parents" => {
            let submitted = history
                .submissions
                .iter()
                .find(|row| row.workflow_id == "plain-0")
                .map(|row| row.at_us)
                .expect("plain-0 submitted");
            let receipt = history
                .provider_receipts
                .iter_mut()
                .find(|row| row.workflow_id == "plain-0")
                .expect("plain-0 receipt");
            receipt.at_us = submitted - 1;
            Rejects::Violation
        }
        "replay_witnessed" => {
            history
                .nemesis
                .retain(|row| row.kind != witness::NEMESIS_WORKER_EXIT);
            Rejects::Inconclusive
        }
        "no_new_receipts_after_replay" => {
            let fast = key("batch-0", "fast");
            let template = history
                .attempts
                .iter()
                .find(|row| row.logical_key == fast)
                .cloned()
                .expect("fast attempt");
            let at_us = history.snapshot_at_us;
            history.attempts.push(AttemptRow {
                attempt_id: "attempt-after-replay".to_string(),
                at_us,
                ..template
            });
            history.replies.push(ReplyRow {
                attempt_id: "attempt-after-replay".to_string(),
                logical_key: fast,
                accepted: false,
                response_digest: "response:batch-0:fast".to_string(),
                at_us: at_us + 1,
            });
            history.snapshot_at_us = at_us + 2;
            Rejects::Violation
        }
        "terminal_carries_committed_effects" => {
            let fast = key("batch-1", "fast");
            let commit = history
                .commits
                .iter_mut()
                .find(|row| row.logical_key == fast)
                .expect("fast commit");
            commit.response =
                br#"{"key":"fast","value":"batch:other","worker_id":"worker-b"}"#.to_vec();
            Rejects::Violation
        }
        other => panic!("rule `{other}` has no rejecting fixture"),
    };
    (history, scope, expect)
}

#[test]
fn legal_recovery_history_passes_every_law() {
    let (history, scope) = legal(BASE);
    for (law, verdict) in verdicts(&history, &scope) {
        assert_eq!(
            verdict,
            Verdict::Pass,
            "{} rejected a legal history",
            law.label()
        );
    }
    assert_eq!(verdicts(&history, &scope).len(), 4);
}

#[test]
fn every_rule_rejects_its_fixture_and_accepts_it_once_removed() {
    for rule in RECOVERY_LAW_RULES {
        let (history, scope, expect) = rule_fixture(rule.name);
        let verdict = check_law(rule.law, &history, &scope);
        match expect {
            Rejects::Violation => assert!(
                matches!(verdict, Verdict::Violation(_)),
                "rule `{}` did not reject its fixture: {verdict:?}",
                rule.name
            ),
            Rejects::Inconclusive => assert!(
                matches!(verdict, Verdict::Inconclusive(_)),
                "rule `{}` did not report its fixture's missing evidence: {verdict:?}",
                rule.name
            ),
        }
        // The mutation: delete this one assertion and the same fixture is
        // accepted, so the rejection above is this rule's and no other's.
        assert_eq!(
            check_law_except(rule.law, &history, &scope, Some(rule.name)),
            Verdict::Pass,
            "fixture for `{}` is still rejected with the rule removed",
            rule.name
        );
    }
}

#[test]
fn law1_rejects_a_terminal_whose_bytes_changed_across_the_restart() {
    let (history, scope, _) = rule_fixture("terminal_bytes_survive_restart");
    assert!(matches!(
        check_law(Law::ResultStability, &history, &scope),
        Verdict::Violation(_)
    ));
}

#[test]
fn law2_rejects_a_second_commit_for_one_logical_key() {
    let (history, scope, _) = rule_fixture("at_most_one_commit");
    assert!(matches!(
        check_law(Law::EffectIdentity, &history, &scope),
        Verdict::Violation(_)
    ));
}

#[test]
fn law3_rejects_a_receipt_that_predates_its_parent() {
    let (history, scope, _) = rule_fixture("receipts_follow_their_parents");
    assert!(matches!(
        check_law(Law::CausalIdentity, &history, &scope),
        Verdict::Violation(_)
    ));
}

#[test]
fn law3_rejects_a_workflow_submitted_as_two_requests() {
    let (mut history, scope) = legal(BASE);
    let mut again = history.submissions[0].clone();
    again.request_digest = "request:other".to_string();
    history.submissions.push(again);
    assert!(matches!(
        check_law(Law::CausalIdentity, &history, &scope),
        Verdict::Violation(_)
    ));
}

#[test]
fn law4_rejects_a_new_receipt_for_a_replayed_effect() {
    let (history, scope, _) = rule_fixture("no_new_receipts_after_replay");
    assert!(matches!(
        check_law(Law::ReplayEquivalence, &history, &scope),
        Verdict::Violation(_)
    ));
}

#[test]
fn an_empty_snapshot_is_inconclusive_for_every_law_never_a_pass() {
    let scope = recovery_law_scope(SegmentSelection::All);
    let empty = WitnessHistory::default();
    for (law, verdict) in verdicts(&empty, &scope) {
        assert!(
            matches!(verdict, Verdict::Inconclusive(_)),
            "{} judged an empty snapshot {verdict:?}",
            law.label()
        );
    }
    assert!(matches!(
        check_law(Law::CausalIdentity, &empty, &LawScope::causal_only()),
        Verdict::Inconclusive(_)
    ));
}

#[test]
fn missing_reattachment_is_inconclusive_not_a_pass() {
    let (history, scope, _) = rule_fixture("terminals_reattached_after_restart");
    assert!(matches!(
        check_law(Law::ResultStability, &history, &scope),
        Verdict::Inconclusive(_)
    ));
}

#[test]
fn unrecovered_work_is_inconclusive_inside_its_bound_and_violated_after_it() {
    let (mut history, scope) = legal(BASE);
    history
        .terminals
        .retain(|row| row.workflow_id != "recovering-0");
    let complete = restart_complete_at(&history);
    history.snapshot_at_us = complete + BOUND_US;
    assert!(matches!(
        check_law(Law::ResultStability, &history, &scope),
        Verdict::Inconclusive(_)
    ));
    history.snapshot_at_us = complete + BOUND_US + 1;
    assert!(matches!(
        check_law(Law::ResultStability, &history, &scope),
        Verdict::Violation(_)
    ));
}

#[test]
fn a_loss_that_was_never_witnessed_is_inconclusive() {
    let (mut history, scope) = legal(BASE);
    history
        .nemesis
        .retain(|row| row.kind != witness::NEMESIS_LOSS_AFTER_COMMIT);
    assert!(matches!(
        check_law(Law::EffectIdentity, &history, &scope),
        Verdict::Inconclusive(_)
    ));
}

#[test]
fn the_e2e_scope_claims_all_four_laws_only_with_segment_two() {
    let full = recovery_law_scope(SegmentSelection::All);
    assert!(Law::ALL.iter().all(|law| law.in_scope(&full)));
    assert_eq!(full.effects.len(), 4);
    assert_eq!(full.loss_keys, [key(LOSS_AFTER_COMMIT_WORKFLOW, "slow")]);
    assert_eq!(full.replayed, [LOSS_AFTER_COMMIT_WORKFLOW]);
    let two = recovery_law_scope(SegmentSelection::Two);
    assert_eq!(two.effects.len(), 4);
    let one = recovery_law_scope(SegmentSelection::One);
    assert_eq!(
        Law::ALL
            .into_iter()
            .filter(|law| law.in_scope(&one))
            .collect::<Vec<_>>(),
        [Law::CausalIdentity]
    );
}

/// A small deterministic generator, so property cases are reproducible from
/// their seed without a property-testing dependency.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn shuffle<T>(&mut self, rows: &mut [T]) {
        for index in (1..rows.len()).rev() {
            rows.swap(index, self.below(index + 1));
        }
    }
}

const PROPERTY_CASES: u64 = 256;

/// One injected fault and the law that must reject it.
type Fault = Box<dyn Fn(&mut WitnessHistory)>;

fn random_shape(rng: &mut XorShift) -> Shape {
    Shape {
        plain_before: rng.below(4),
        recovering: 1 + rng.below(3),
        effect_workflows: 1 + rng.below(3),
        extra_retries: rng.below(3),
        provider_receipts: rng.below(3),
        with_loss: rng.below(2) == 1,
        with_replay: rng.below(2) == 1,
    }
}

/// Row order in a ledger carries no meaning; only the witness clock does.
fn shuffled(mut history: WitnessHistory, rng: &mut XorShift) -> WitnessHistory {
    rng.shuffle(&mut history.submissions);
    rng.shuffle(&mut history.acks);
    rng.shuffle(&mut history.terminals);
    rng.shuffle(&mut history.nemesis);
    rng.shuffle(&mut history.attempts);
    rng.shuffle(&mut history.commits);
    rng.shuffle(&mut history.replies);
    rng.shuffle(&mut history.provider_receipts);
    history
}

#[test]
fn property_generated_legal_histories_pass_in_any_row_order() {
    for seed in 1..=PROPERTY_CASES {
        let mut rng = XorShift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let shape = random_shape(&mut rng);
        let (history, scope) = legal(shape);
        let history = shuffled(history, &mut rng);
        assert!(
            all_pass(&history, &scope),
            "seed {seed} shape {shape:?}: {:#?}",
            verdicts(&history, &scope)
        );
    }
}

#[test]
fn property_removing_any_required_row_never_passes() {
    for seed in 1..=PROPERTY_CASES {
        let mut rng = XorShift(seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
        let shape = random_shape(&mut rng);
        let (history, scope) = legal(shape);
        let streams = [
            history.submissions.len(),
            history.terminals.len(),
            history.nemesis.len(),
            history.attempts.len(),
            history.commits.len(),
        ];
        for (stream, len) in streams.into_iter().enumerate() {
            for index in 0..len {
                let mut incomplete = history.clone();
                match stream {
                    0 => drop(incomplete.submissions.remove(index)),
                    1 => drop(incomplete.terminals.remove(index)),
                    2 => drop(incomplete.nemesis.remove(index)),
                    3 => drop(incomplete.attempts.remove(index)),
                    _ => drop(incomplete.commits.remove(index)),
                }
                assert!(
                    !all_pass(&incomplete, &scope),
                    "seed {seed} shape {shape:?}: dropping row {index} of stream {stream} still passed"
                );
            }
        }
    }
}

#[test]
fn property_every_single_fault_is_rejected_by_its_law() {
    for seed in 1..=PROPERTY_CASES {
        let mut rng = XorShift(seed.wrapping_mul(0x94D0_49BB_1331_11EB));
        let shape = random_shape(&mut rng);
        let (legal_history, scope) = legal(shape);
        let batch = format!("batch-{}", rng.below(shape.effect_workflows));
        let effect_key = key(&batch, ["slow", "fast"][rng.below(2)]);
        let faults: Vec<(Law, Fault)> = vec![
            (
                Law::ResultStability,
                Box::new(|history| {
                    let row = history
                        .terminals
                        .iter_mut()
                        .find(|row| row.phase == witness::TERMINAL_REATTACHED)
                        .expect("a reattached terminal");
                    row.output.insert(0, b' ');
                }),
            ),
            (
                Law::ResultStability,
                Box::new(|history| {
                    let complete = restart_complete_at(history);
                    let row = history
                        .terminals
                        .iter_mut()
                        .find(|row| row.workflow_id.starts_with("recovering-"))
                        .expect("a recovering terminal");
                    row.at_us = complete + BOUND_US + 1;
                    history.snapshot_at_us = complete + BOUND_US + 2;
                }),
            ),
            (
                Law::EffectIdentity,
                Box::new(|history| {
                    let commit = history.commits[0].clone();
                    history.commits.push(CommitRow {
                        at_us: history.snapshot_at_us,
                        ..commit
                    });
                    history.snapshot_at_us += 1;
                }),
            ),
            (
                Law::EffectIdentity,
                Box::new({
                    let effect_key = effect_key.clone();
                    move |history| {
                        let attempt = history
                            .attempts
                            .iter_mut()
                            .find(|row| row.logical_key == effect_key)
                            .expect("an attempt");
                        attempt.request_digest.push_str(":drift");
                    }
                }),
            ),
            (
                Law::EffectIdentity,
                Box::new({
                    let effect_key = effect_key.clone();
                    move |history| {
                        let reply = history
                            .replies
                            .iter_mut()
                            .find(|row| row.logical_key == effect_key)
                            .expect("a reply");
                        reply.response_digest.push_str(":fresh");
                    }
                }),
            ),
            (
                Law::CausalIdentity,
                Box::new(|history| {
                    history.attempts[0].parent_workflow_id = "never-submitted".to_string();
                }),
            ),
            (
                Law::CausalIdentity,
                Box::new(|history| {
                    let parent = history.attempts[0].parent_workflow_id.clone();
                    let submitted = history
                        .submissions
                        .iter()
                        .find(|row| row.workflow_id == parent)
                        .map(|row| row.at_us)
                        .expect("submitted parent");
                    history.attempts[0].at_us = submitted - 1;
                }),
            ),
            (
                Law::ReplayEquivalence,
                Box::new({
                    let effect_key = effect_key.clone();
                    move |history| {
                        let commit = history
                            .commits
                            .iter_mut()
                            .find(|row| row.logical_key == effect_key)
                            .expect("a commit");
                        commit.response = b"{\"key\":\"forged\"}".to_vec();
                    }
                }),
            ),
        ];
        for (index, (law, fault)) in faults.iter().enumerate() {
            let mut history = legal_history.clone();
            fault(&mut history);
            let history = shuffled(history, &mut rng);
            assert_ne!(
                check_law(*law, &history, &scope),
                Verdict::Pass,
                "seed {seed} shape {shape:?}: fault {index} passed {}",
                law.label()
            );
        }
        if shape.with_replay {
            let mut history = legal_history.clone();
            let template = history
                .attempts
                .iter()
                .find(|row| row.parent_workflow_id == "batch-0")
                .cloned()
                .expect("replayed attempt");
            let at_us = history.snapshot_at_us;
            history.attempts.push(AttemptRow {
                attempt_id: "attempt-after-replay".to_string(),
                at_us,
                ..template
            });
            history.snapshot_at_us += 1;
            assert!(
                matches!(
                    check_law(Law::ReplayEquivalence, &history, &scope),
                    Verdict::Violation(_)
                ),
                "seed {seed} shape {shape:?}: a re-executed completed effect passed law 4"
            );
        }
    }
}
