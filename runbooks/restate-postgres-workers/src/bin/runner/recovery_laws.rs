//! Recovery laws 1-4 as pure checkers over witness-ledger rows (FIG-608).
//!
//! These functions read only rows from the witness database (`witness.sql`);
//! Lash's store is never an input. Each law is a list of named rules, and each
//! rule either finds a violation, reports evidence it needs but does not have,
//! or says nothing. Missing evidence makes a law inconclusive, and an
//! inconclusive law fails the run exactly as a violation does: a run passes
//! only on complete evidence. `tests.rs` holds legal histories, a rejecting
//! fixture for every rule, and proves each fixture is accepted once its rule
//! is removed.

use super::*;

/// A client's submission, witnessed before it was sent.
#[derive(Clone, Debug)]
pub(crate) struct SubmissionRow {
    pub(crate) workflow_id: String,
    pub(crate) request_digest: String,
    pub(crate) at_us: i64,
}

/// The ingress acknowledgement of a submission.
#[derive(Clone, Debug)]
pub(crate) struct AckRow {
    pub(crate) workflow_id: String,
    pub(crate) at_us: i64,
}

/// Terminal bytes a client read from a workflow's address.
#[derive(Clone, Debug)]
pub(crate) struct TerminalRow {
    pub(crate) workflow_id: String,
    pub(crate) phase: String,
    pub(crate) output: Vec<u8>,
    pub(crate) at_us: i64,
}

/// A fault the harness injected.
#[derive(Clone, Debug)]
pub(crate) struct NemesisRow {
    pub(crate) kind: String,
    pub(crate) subject: String,
    pub(crate) at_us: i64,
}

/// One physical attempt at a witnessed effect.
#[derive(Clone, Debug)]
pub(crate) struct AttemptRow {
    pub(crate) attempt_id: String,
    pub(crate) logical_key: String,
    pub(crate) parent_workflow_id: String,
    pub(crate) call_id: String,
    pub(crate) request_digest: String,
    pub(crate) at_us: i64,
}

/// The idempotent receiver's accepted commit for a logical key.
#[derive(Clone, Debug)]
pub(crate) struct CommitRow {
    pub(crate) logical_key: String,
    pub(crate) parent_workflow_id: String,
    pub(crate) first_attempt_id: String,
    pub(crate) request_digest: String,
    pub(crate) response: Vec<u8>,
    pub(crate) response_digest: String,
    pub(crate) at_us: i64,
}

/// The receiver's answer to one attempt.
#[derive(Clone, Debug)]
pub(crate) struct ReplyRow {
    pub(crate) attempt_id: String,
    pub(crate) logical_key: String,
    pub(crate) accepted: bool,
    pub(crate) response_digest: String,
    pub(crate) at_us: i64,
}

/// A completion the provider served, receipted by the provider.
#[derive(Clone, Debug)]
pub(crate) struct ProviderReceiptRow {
    pub(crate) request_id: String,
    pub(crate) workflow_id: String,
    pub(crate) at_us: i64,
}

/// One consistent snapshot of every witness ledger. Times are microseconds on
/// the witness database's clock, which stamped every row.
#[derive(Clone, Debug, Default)]
pub(crate) struct WitnessHistory {
    pub(crate) snapshot_at_us: i64,
    pub(crate) submissions: Vec<SubmissionRow>,
    pub(crate) acks: Vec<AckRow>,
    pub(crate) terminals: Vec<TerminalRow>,
    pub(crate) nemesis: Vec<NemesisRow>,
    pub(crate) attempts: Vec<AttemptRow>,
    pub(crate) commits: Vec<CommitRow>,
    pub(crate) replies: Vec<ReplyRow>,
    pub(crate) provider_receipts: Vec<ProviderReceiptRow>,
}

/// An effect a successful terminal requires a receipt for, and where the
/// client-visible terminal carries its result.
#[derive(Clone, Debug)]
pub(crate) struct ExpectedEffect {
    pub(crate) parent_workflow_id: String,
    pub(crate) logical_key: String,
    pub(crate) terminal_pointer: String,
}

/// What a run claims, and so what its evidence must show.
#[derive(Clone, Debug)]
pub(crate) struct LawScope {
    /// Law 1 applies: the run restarted both workers and Restate.
    pub(crate) restart: bool,
    pub(crate) recovery_bound_us: i64,
    /// Laws 2 and 4 apply to these effects.
    pub(crate) effects: Vec<ExpectedEffect>,
    /// Effects whose worker is lost after the commit and before the return.
    pub(crate) loss_keys: Vec<String>,
    /// Workflows whose worker exits after their effects completed.
    pub(crate) replayed: Vec<String>,
}

impl LawScope {
    /// Law 3 alone: every run's receipts name real, earlier parents.
    pub(crate) fn causal_only() -> Self {
        Self {
            restart: false,
            recovery_bound_us: 0,
            effects: Vec::new(),
            loss_keys: Vec::new(),
            replayed: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum Law {
    ResultStability,
    EffectIdentity,
    CausalIdentity,
    ReplayEquivalence,
}

impl Law {
    pub(crate) const ALL: [Law; 4] = [
        Law::ResultStability,
        Law::EffectIdentity,
        Law::CausalIdentity,
        Law::ReplayEquivalence,
    ];

    pub(crate) fn in_scope(self, scope: &LawScope) -> bool {
        match self {
            Law::ResultStability => scope.restart,
            Law::EffectIdentity | Law::ReplayEquivalence => !scope.effects.is_empty(),
            Law::CausalIdentity => true,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Law::ResultStability => "law1-durable-result-stability",
            Law::EffectIdentity => "law2-logical-effect-identity",
            Law::CausalIdentity => "law3-causal-identity",
            Law::ReplayEquivalence => "law4-replay-equivalence",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Verdict {
    Pass,
    /// Evidence the law needs is absent: never a pass.
    Inconclusive(Vec<String>),
    Violation(Vec<String>),
}

#[derive(Default)]
pub(crate) struct Findings {
    violations: Vec<String>,
    missing: Vec<String>,
}

impl Findings {
    fn violation(&mut self, finding: String) {
        self.violations.push(finding);
    }

    fn missing(&mut self, finding: String) {
        self.missing.push(finding);
    }
}

pub(crate) struct Rule {
    pub(crate) law: Law,
    pub(crate) name: &'static str,
    check: fn(&WitnessHistory, &LawScope, &mut Findings),
}

pub(crate) const RECOVERY_LAW_RULES: &[Rule] = &[
    Rule {
        law: Law::ResultStability,
        name: "restart_witnessed",
        check: rule_restart_witnessed,
    },
    Rule {
        law: Law::ResultStability,
        name: "observed_terminals_agree",
        check: rule_observed_terminals_agree,
    },
    Rule {
        law: Law::ResultStability,
        name: "terminals_reattached_after_restart",
        check: rule_terminals_reattached_after_restart,
    },
    Rule {
        law: Law::ResultStability,
        name: "terminal_bytes_survive_restart",
        check: rule_terminal_bytes_survive_restart,
    },
    Rule {
        law: Law::ResultStability,
        name: "unfinished_work_recovers_in_bound",
        check: rule_unfinished_work_recovers_in_bound,
    },
    Rule {
        law: Law::EffectIdentity,
        name: "logical_key_stable",
        check: rule_logical_key_stable,
    },
    Rule {
        law: Law::EffectIdentity,
        name: "request_bytes_identical",
        check: rule_request_bytes_identical,
    },
    Rule {
        law: Law::EffectIdentity,
        name: "at_most_one_commit",
        check: rule_at_most_one_commit,
    },
    Rule {
        law: Law::EffectIdentity,
        name: "replies_carry_the_commit",
        check: rule_replies_carry_the_commit,
    },
    Rule {
        law: Law::EffectIdentity,
        name: "commit_names_its_accepted_attempt",
        check: rule_commit_names_its_accepted_attempt,
    },
    Rule {
        law: Law::EffectIdentity,
        name: "expected_receipts_present",
        check: rule_expected_receipts_present,
    },
    Rule {
        law: Law::EffectIdentity,
        name: "lost_attempt_retried",
        check: rule_lost_attempt_retried,
    },
    Rule {
        law: Law::CausalIdentity,
        name: "causal_evidence_present",
        check: rule_causal_evidence_present,
    },
    Rule {
        law: Law::CausalIdentity,
        name: "receipts_name_known_parents",
        check: rule_receipts_name_known_parents,
    },
    Rule {
        law: Law::CausalIdentity,
        name: "receipts_follow_their_parents",
        check: rule_receipts_follow_their_parents,
    },
    Rule {
        law: Law::ReplayEquivalence,
        name: "replay_witnessed",
        check: rule_replay_witnessed,
    },
    Rule {
        law: Law::ReplayEquivalence,
        name: "no_new_receipts_after_replay",
        check: rule_no_new_receipts_after_replay,
    },
    Rule {
        law: Law::ReplayEquivalence,
        name: "terminal_carries_committed_effects",
        check: rule_terminal_carries_committed_effects,
    },
];

pub(crate) fn check_law(law: Law, history: &WitnessHistory, scope: &LawScope) -> Verdict {
    check_law_except(law, history, scope, None)
}

/// `check_law` with the rule named `skipped` removed: the mutation the rule
/// fixtures in `tests.rs` run to prove each rule is the one rejecting them.
pub(crate) fn check_law_except(
    law: Law,
    history: &WitnessHistory,
    scope: &LawScope,
    skipped: Option<&str>,
) -> Verdict {
    let mut findings = Findings::default();
    for rule in RECOVERY_LAW_RULES
        .iter()
        .filter(|rule| rule.law == law && Some(rule.name) != skipped)
    {
        (rule.check)(history, scope, &mut findings);
    }
    if !findings.violations.is_empty() {
        Verdict::Violation(findings.violations)
    } else if !findings.missing.is_empty() {
        Verdict::Inconclusive(findings.missing)
    } else {
        Verdict::Pass
    }
}

/// The verdict of every law `scope` claims.
pub(crate) fn check_recovery_laws(
    history: &WitnessHistory,
    scope: &LawScope,
) -> Vec<(Law, Verdict)> {
    Law::ALL
        .into_iter()
        .filter(|law| law.in_scope(scope))
        .map(|law| (law, check_law(law, history, scope)))
        .collect()
}

fn nemesis_of<'a>(
    history: &'a WitnessHistory,
    kind: &'a str,
) -> impl Iterator<Item = &'a NemesisRow> + 'a {
    history.nemesis.iter().filter(move |row| row.kind == kind)
}

/// The one restart the run injected, as (begin, complete).
fn restart_window(history: &WitnessHistory) -> Option<(i64, i64)> {
    let begins = nemesis_of(history, witness::NEMESIS_RESTART_BEGIN).collect::<Vec<_>>();
    let completes = nemesis_of(history, witness::NEMESIS_RESTART_COMPLETE).collect::<Vec<_>>();
    match (begins.as_slice(), completes.as_slice()) {
        ([begin], [complete]) if begin.at_us <= complete.at_us => {
            Some((begin.at_us, complete.at_us))
        }
        _ => None,
    }
}

fn first_submission_at(history: &WitnessHistory, workflow_id: &str) -> Option<i64> {
    history
        .submissions
        .iter()
        .filter(|row| row.workflow_id == workflow_id)
        .map(|row| row.at_us)
        .min()
}

fn terminals_of<'a>(
    history: &'a WitnessHistory,
    workflow_id: &'a str,
    phase: &'a str,
) -> impl Iterator<Item = &'a TerminalRow> + 'a {
    history
        .terminals
        .iter()
        .filter(move |row| row.workflow_id == workflow_id && row.phase == phase)
}

/// The first bytes a client read for each workflow that finished before the
/// restart began.
fn saved_before_restart(history: &WitnessHistory, begin: i64) -> BTreeMap<&str, &TerminalRow> {
    let mut saved = BTreeMap::new();
    for row in history
        .terminals
        .iter()
        .filter(|row| row.phase == witness::TERMINAL_OBSERVED && row.at_us < begin)
    {
        saved
            .entry(row.workflow_id.as_str())
            .and_modify(|first: &mut &TerminalRow| {
                if row.at_us < first.at_us {
                    *first = row;
                }
            })
            .or_insert(row);
    }
    saved
}

fn commit_for<'a>(history: &'a WitnessHistory, logical_key: &str) -> Option<&'a CommitRow> {
    history
        .commits
        .iter()
        .filter(|row| row.logical_key == logical_key)
        .min_by_key(|row| row.at_us)
}

fn has_terminal(history: &WitnessHistory, workflow_id: &str) -> bool {
    terminals_of(history, workflow_id, witness::TERMINAL_OBSERVED)
        .next()
        .is_some()
}

fn rule_restart_witnessed(history: &WitnessHistory, _: &LawScope, findings: &mut Findings) {
    if restart_window(history).is_none() {
        findings.missing(
            "no single restart-begin followed by one restart-complete was witnessed".to_string(),
        );
    }
}

fn rule_observed_terminals_agree(history: &WitnessHistory, _: &LawScope, findings: &mut Findings) {
    let mut first: BTreeMap<&str, &TerminalRow> = BTreeMap::new();
    let mut observed = history
        .terminals
        .iter()
        .filter(|row| row.phase == witness::TERMINAL_OBSERVED)
        .collect::<Vec<_>>();
    observed.sort_by_key(|row| row.at_us);
    for row in observed {
        match first.get(row.workflow_id.as_str()) {
            Some(earlier) if earlier.output != row.output => findings.violation(format!(
                "a client read two different terminals for `{}`",
                row.workflow_id
            )),
            Some(_) => {}
            None => {
                first.insert(row.workflow_id.as_str(), row);
            }
        }
    }
}

fn rule_terminals_reattached_after_restart(
    history: &WitnessHistory,
    _: &LawScope,
    findings: &mut Findings,
) {
    let Some((begin, complete)) = restart_window(history) else {
        return;
    };
    let saved = saved_before_restart(history, begin);
    if saved.is_empty() {
        findings.missing("no client terminal was saved before the restart".to_string());
    }
    for workflow_id in saved.keys() {
        if !terminals_of(history, workflow_id, witness::TERMINAL_REATTACHED)
            .any(|row| row.at_us > complete)
        {
            findings.missing(format!(
                "`{workflow_id}` was never reattached after the restart completed"
            ));
        }
    }
    for row in history
        .terminals
        .iter()
        .filter(|row| row.phase == witness::TERMINAL_REATTACHED)
    {
        if !saved.contains_key(row.workflow_id.as_str()) || row.at_us <= complete {
            findings.missing(format!(
                "reattachment of `{}` has no saved pre-restart terminal or predates the restart",
                row.workflow_id
            ));
        }
    }
}

fn rule_terminal_bytes_survive_restart(
    history: &WitnessHistory,
    _: &LawScope,
    findings: &mut Findings,
) {
    let Some((begin, complete)) = restart_window(history) else {
        return;
    };
    for (workflow_id, saved) in saved_before_restart(history, begin) {
        for row in terminals_of(history, workflow_id, witness::TERMINAL_REATTACHED)
            .filter(|row| row.at_us > complete)
        {
            if row.output != saved.output {
                findings.violation(format!(
                    "`{workflow_id}` terminal bytes changed across the restart"
                ));
            }
        }
    }
}

fn rule_unfinished_work_recovers_in_bound(
    history: &WitnessHistory,
    scope: &LawScope,
    findings: &mut Findings,
) {
    let Some((begin, complete)) = restart_window(history) else {
        return;
    };
    let unfinished = history
        .acks
        .iter()
        .filter(|ack| ack.at_us < begin)
        .map(|ack| ack.workflow_id.as_str())
        .filter(|workflow_id| {
            !history
                .terminals
                .iter()
                .any(|row| row.workflow_id == *workflow_id && row.at_us < begin)
        })
        .collect::<BTreeSet<_>>();
    if unfinished.is_empty() {
        findings
            .missing("no acknowledged invocation was unfinished across the restart".to_string());
    }
    let deadline = complete.saturating_add(scope.recovery_bound_us);
    for workflow_id in unfinished {
        let recovered = terminals_of(history, workflow_id, witness::TERMINAL_OBSERVED)
            .map(|row| row.at_us)
            .min();
        match recovered {
            Some(at) if at <= deadline => {}
            Some(at) => findings.violation(format!(
                "`{workflow_id}` recovered {}us after the restart, past the {}us bound",
                at - complete,
                scope.recovery_bound_us
            )),
            None if history.snapshot_at_us > deadline => findings.violation(format!(
                "`{workflow_id}` was acknowledged before the restart and never recovered within the bound"
            )),
            None => findings.missing(format!(
                "`{workflow_id}` has not recovered yet and its bound has not elapsed"
            )),
        }
    }
}

fn attempts_by_key(history: &WitnessHistory) -> BTreeMap<&str, Vec<&AttemptRow>> {
    let mut by_key: BTreeMap<&str, Vec<&AttemptRow>> = BTreeMap::new();
    for attempt in &history.attempts {
        by_key
            .entry(attempt.logical_key.as_str())
            .or_default()
            .push(attempt);
    }
    by_key
}

fn rule_logical_key_stable(history: &WitnessHistory, _: &LawScope, findings: &mut Findings) {
    for (key, attempts) in attempts_by_key(history) {
        let identities = attempts
            .iter()
            .map(|row| (row.parent_workflow_id.as_str(), row.call_id.as_str()))
            .collect::<BTreeSet<_>>();
        if identities.len() > 1 {
            findings.violation(format!(
                "logical key `{key}` was retried under different identities: {identities:?}"
            ));
        }
    }
    let mut keys_by_call: BTreeMap<(&str, &str), BTreeSet<&str>> = BTreeMap::new();
    for attempt in &history.attempts {
        keys_by_call
            .entry((&attempt.parent_workflow_id, &attempt.call_id))
            .or_default()
            .insert(&attempt.logical_key);
    }
    for (call, keys) in keys_by_call.into_iter().filter(|(_, keys)| keys.len() > 1) {
        findings.violation(format!(
            "call {call:?} was retried under different logical keys: {keys:?}"
        ));
    }
}

fn rule_request_bytes_identical(history: &WitnessHistory, _: &LawScope, findings: &mut Findings) {
    for (key, attempts) in attempts_by_key(history) {
        let digests = attempts
            .iter()
            .map(|row| row.request_digest.as_str())
            .collect::<BTreeSet<_>>();
        if digests.len() > 1 {
            findings.violation(format!(
                "logical key `{key}` was retried with different request bytes: {digests:?}"
            ));
        }
        if let Some(commit) = commit_for(history, key)
            && !digests.contains(commit.request_digest.as_str())
        {
            findings.violation(format!(
                "logical key `{key}` committed a request no attempt sent"
            ));
        }
    }
}

fn rule_at_most_one_commit(history: &WitnessHistory, _: &LawScope, findings: &mut Findings) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for commit in &history.commits {
        *counts.entry(commit.logical_key.as_str()).or_default() += 1;
    }
    for (key, count) in counts.into_iter().filter(|(_, count)| *count > 1) {
        findings.violation(format!("logical key `{key}` was committed {count} times"));
    }
}

fn rule_replies_carry_the_commit(history: &WitnessHistory, _: &LawScope, findings: &mut Findings) {
    let mut accepted: BTreeMap<&str, usize> = BTreeMap::new();
    for reply in &history.replies {
        let Some(commit) = commit_for(history, &reply.logical_key) else {
            findings.violation(format!(
                "attempt `{}` was answered for `{}`, which has no commit",
                reply.attempt_id, reply.logical_key
            ));
            continue;
        };
        if reply.response_digest != commit.response_digest {
            findings.violation(format!(
                "attempt `{}` was answered with a response other than `{}`'s commit",
                reply.attempt_id, reply.logical_key
            ));
        }
        if let Some(attempt) = history
            .attempts
            .iter()
            .find(|attempt| attempt.attempt_id == reply.attempt_id)
            && attempt.logical_key != reply.logical_key
        {
            findings.violation(format!(
                "attempt `{}` was answered under another logical key",
                reply.attempt_id
            ));
        }
        if reply.accepted {
            *accepted.entry(reply.logical_key.as_str()).or_default() += 1;
        }
    }
    for (key, count) in accepted.into_iter().filter(|(_, count)| *count > 1) {
        findings.violation(format!("logical key `{key}` accepted {count} attempts"));
    }
}

fn rule_commit_names_its_accepted_attempt(
    history: &WitnessHistory,
    _: &LawScope,
    findings: &mut Findings,
) {
    for commit in &history.commits {
        let first = history
            .attempts
            .iter()
            .find(|attempt| attempt.attempt_id == commit.first_attempt_id);
        match first {
            Some(attempt)
                if attempt.logical_key == commit.logical_key
                    && attempt.parent_workflow_id == commit.parent_workflow_id => {}
            _ => findings.violation(format!(
                "commit of `{}` names attempt `{}`, which is not one of its attempts",
                commit.logical_key, commit.first_attempt_id
            )),
        }
        if let Some(reply) = history.replies.iter().find(|reply| {
            reply.logical_key == commit.logical_key
                && reply.accepted
                && reply.attempt_id != commit.first_attempt_id
        }) {
            findings.violation(format!(
                "`{}` accepted attempt `{}` but committed attempt `{}`",
                commit.logical_key, reply.attempt_id, commit.first_attempt_id
            ));
        }
    }
}

fn rule_expected_receipts_present(
    history: &WitnessHistory,
    scope: &LawScope,
    findings: &mut Findings,
) {
    for effect in &scope.effects {
        if !has_terminal(history, &effect.parent_workflow_id) {
            findings.missing(format!(
                "`{}` has no client terminal, so its receipts cannot be required",
                effect.parent_workflow_id
            ));
        } else if commit_for(history, &effect.logical_key).is_none() {
            findings.violation(format!(
                "`{}` finished without a committed receipt for `{}`",
                effect.parent_workflow_id, effect.logical_key
            ));
        } else if !history
            .replies
            .iter()
            .any(|reply| reply.logical_key == effect.logical_key)
        {
            // The tool returns only after its reply is witnessed, so a
            // finished workflow whose effect has no reply lost evidence.
            findings.missing(format!(
                "`{}` finished but no attempt at `{}` has a witnessed reply",
                effect.parent_workflow_id, effect.logical_key
            ));
        }
    }
}

fn rule_lost_attempt_retried(history: &WitnessHistory, scope: &LawScope, findings: &mut Findings) {
    for key in &scope.loss_keys {
        let Some(loss) =
            nemesis_of(history, witness::NEMESIS_LOSS_AFTER_COMMIT).find(|row| row.subject == *key)
        else {
            findings.missing(format!(
                "the loss after `{key}`'s commit was never witnessed"
            ));
            continue;
        };
        let Some(commit) = commit_for(history, key) else {
            continue;
        };
        if loss.at_us < commit.at_us {
            findings.missing(format!(
                "the loss for `{key}` predates its commit, so the commit window went unexercised"
            ));
            continue;
        }
        let retried = history
            .attempts
            .iter()
            .any(|attempt| attempt.logical_key == *key && attempt.at_us > loss.at_us);
        if !retried && has_terminal(history, &commit.parent_workflow_id) {
            findings.violation(format!(
                "`{key}`'s worker was lost before the tool returned, yet its workflow finished with no retry"
            ));
        }
    }
}

fn rule_causal_evidence_present(history: &WitnessHistory, _: &LawScope, findings: &mut Findings) {
    if history.submissions.is_empty() {
        findings.missing("no client submission was witnessed".to_string());
    }
    if history.provider_receipts.is_empty() && history.attempts.is_empty() {
        findings.missing("no provider or effect receipt was witnessed".to_string());
    }
}

/// Every witnessed row that names a parent workflow, as (what, parent, at).
fn parented_rows(history: &WitnessHistory) -> Vec<(String, &str, i64)> {
    let mut rows = Vec::new();
    rows.extend(history.acks.iter().map(|row| {
        (
            format!("acknowledgement of `{}`", row.workflow_id),
            row.workflow_id.as_str(),
            row.at_us,
        )
    }));
    rows.extend(history.terminals.iter().map(|row| {
        (
            format!("{} terminal of `{}`", row.phase, row.workflow_id),
            row.workflow_id.as_str(),
            row.at_us,
        )
    }));
    rows.extend(history.provider_receipts.iter().map(|row| {
        (
            format!("provider receipt `{}`", row.request_id),
            row.workflow_id.as_str(),
            row.at_us,
        )
    }));
    rows.extend(history.attempts.iter().map(|row| {
        (
            format!("attempt `{}`", row.attempt_id),
            row.parent_workflow_id.as_str(),
            row.at_us,
        )
    }));
    rows.extend(history.commits.iter().map(|row| {
        (
            format!("commit of `{}`", row.logical_key),
            row.parent_workflow_id.as_str(),
            row.at_us,
        )
    }));
    rows
}

fn rule_receipts_name_known_parents(
    history: &WitnessHistory,
    _: &LawScope,
    findings: &mut Findings,
) {
    let mut requests: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for submission in &history.submissions {
        requests
            .entry(&submission.workflow_id)
            .or_default()
            .insert(&submission.request_digest);
    }
    for (workflow_id, digests) in requests
        .into_iter()
        .filter(|(_, digests)| digests.len() > 1)
    {
        findings.violation(format!(
            "`{workflow_id}` was submitted as {} different requests",
            digests.len()
        ));
    }
    for (what, parent, _) in parented_rows(history) {
        if first_submission_at(history, parent).is_none() {
            findings.violation(format!(
                "{what} names `{parent}`, which was never submitted"
            ));
        }
    }
    for reply in &history.replies {
        if !history
            .attempts
            .iter()
            .any(|attempt| attempt.attempt_id == reply.attempt_id)
        {
            findings.violation(format!(
                "reply to `{}` names no witnessed attempt",
                reply.attempt_id
            ));
        }
    }
}

fn rule_receipts_follow_their_parents(
    history: &WitnessHistory,
    _: &LawScope,
    findings: &mut Findings,
) {
    for (what, parent, at) in parented_rows(history) {
        if let Some(submitted) = first_submission_at(history, parent)
            && at < submitted
        {
            findings.violation(format!(
                "{what} was recorded {}us before `{parent}` was submitted",
                submitted - at
            ));
        }
    }
    for reply in &history.replies {
        if let Some(attempt) = history
            .attempts
            .iter()
            .find(|attempt| attempt.attempt_id == reply.attempt_id)
            && reply.at_us < attempt.at_us
        {
            findings.violation(format!(
                "reply to `{}` predates the attempt it answers",
                reply.attempt_id
            ));
        }
    }
    for commit in &history.commits {
        if let Some(attempt) = history
            .attempts
            .iter()
            .find(|attempt| attempt.attempt_id == commit.first_attempt_id)
            && commit.at_us < attempt.at_us
        {
            findings.violation(format!(
                "commit of `{}` predates the attempt it accepted",
                commit.logical_key
            ));
        }
    }
}

fn worker_exit_at(history: &WitnessHistory, workflow_id: &str) -> Option<i64> {
    nemesis_of(history, witness::NEMESIS_WORKER_EXIT)
        .filter(|row| row.subject == workflow_id)
        .map(|row| row.at_us)
        .min()
}

fn rule_replay_witnessed(history: &WitnessHistory, scope: &LawScope, findings: &mut Findings) {
    for workflow_id in &scope.replayed {
        let Some(exit) = worker_exit_at(history, workflow_id) else {
            findings.missing(format!("`{workflow_id}`'s worker exit was never witnessed"));
            continue;
        };
        if !has_terminal(history, workflow_id) {
            findings.missing(format!(
                "`{workflow_id}` has no client terminal after replay"
            ));
        }
        for effect in scope
            .effects
            .iter()
            .filter(|effect| effect.parent_workflow_id == *workflow_id)
        {
            if !commit_for(history, &effect.logical_key).is_some_and(|commit| commit.at_us < exit) {
                findings.missing(format!(
                    "`{}` had not completed before `{workflow_id}`'s worker exited",
                    effect.logical_key
                ));
            }
        }
    }
}

fn rule_no_new_receipts_after_replay(
    history: &WitnessHistory,
    scope: &LawScope,
    findings: &mut Findings,
) {
    for workflow_id in &scope.replayed {
        let Some(exit) = worker_exit_at(history, workflow_id) else {
            continue;
        };
        let keys = history
            .attempts
            .iter()
            .filter(|attempt| attempt.parent_workflow_id == *workflow_id)
            .map(|attempt| attempt.logical_key.as_str())
            .collect::<BTreeSet<_>>();
        let late = history
            .attempts
            .iter()
            .filter(|row| row.parent_workflow_id == *workflow_id && row.at_us > exit)
            .map(|row| format!("attempt `{}`", row.attempt_id))
            .chain(
                history
                    .commits
                    .iter()
                    .filter(|row| row.parent_workflow_id == *workflow_id && row.at_us > exit)
                    .map(|row| format!("commit of `{}`", row.logical_key)),
            )
            .chain(
                history
                    .replies
                    .iter()
                    .filter(|row| keys.contains(row.logical_key.as_str()) && row.at_us > exit)
                    .map(|row| format!("reply to `{}`", row.attempt_id)),
            )
            .collect::<Vec<_>>();
        if !late.is_empty() {
            findings.violation(format!(
                "replay of `{workflow_id}` produced new receipts for completed effects: {late:?}"
            ));
        }
    }
}

fn rule_terminal_carries_committed_effects(
    history: &WitnessHistory,
    scope: &LawScope,
    findings: &mut Findings,
) {
    for effect in &scope.effects {
        let Some(commit) = commit_for(history, &effect.logical_key) else {
            continue;
        };
        let committed = serde_json::from_slice::<Value>(&commit.response).ok();
        for terminal in terminals_of(
            history,
            &effect.parent_workflow_id,
            witness::TERMINAL_OBSERVED,
        ) {
            let carried = serde_json::from_slice::<Value>(&terminal.output)
                .ok()
                .and_then(|output| output.pointer(&effect.terminal_pointer).cloned());
            if carried.is_none() || carried != committed {
                findings.violation(format!(
                    "`{}`'s client terminal does not carry `{}`'s committed response at `{}`",
                    effect.parent_workflow_id, effect.logical_key, effect.terminal_pointer
                ));
            }
        }
    }
}
