//! No effect runs more than once outside the at-least-once window.
//!
//! ADR 0042, ADR 0110 §3 and ADR 0117: the engine replays a recorded outcome
//! and never re-runs its effect. The one window no journal covers is an
//! attempt that ran the effect and died before its outcome was recorded; only
//! there may the effect run again. So a tool body's run for one attempt of
//! one logical call may repeat only across an attempt the engine saw fail in
//! between, and a harness-run effect may run at most once more than the
//! attempts that died inside its window.

use std::collections::BTreeMap;

use super::{Fact, History, HistoryChecker, Violation};

pub(super) struct EffectWindow;

/// One attempt of one logical call: session, scope, call, attempt number.
type AttemptKey<'a> = (&'a str, &'a str, &'a str, u32);

const INVARIANT: &str = "effect-at-least-once-window";

impl HistoryChecker for EffectWindow {
    fn invariant(&self) -> &'static str {
        INVARIANT
    }

    fn observed(&self, history: &History) -> usize {
        history
            .records
            .iter()
            .filter(|record| {
                matches!(
                    record.fact,
                    Fact::ToolExecuted { .. } | Fact::EffectRan { .. }
                )
            })
            .count()
    }

    fn check(&self, history: &History) -> Vec<Violation> {
        let mut violations = Vec::new();
        let mut runs: BTreeMap<AttemptKey<'_>, Vec<(usize, u64)>> = BTreeMap::new();
        for record in &history.records {
            match &record.fact {
                Fact::ToolExecuted {
                    call,
                    attempt,
                    failed_attempts_before,
                } => {
                    // A call is told apart by its `ToolCallId` (ADR 0117).
                    runs.entry((
                        &call.session,
                        &call.scope,
                        call.identity.0.as_str(),
                        *attempt,
                    ))
                    .or_default()
                    .push((record.at, *failed_attempts_before));
                }
                Fact::EffectRan {
                    effect,
                    executions,
                    unrecorded_attempts,
                } if *executions > 1 + unrecorded_attempts => {
                    violations.push(
                        Violation::new(
                            INVARIANT,
                            format!(
                                "effect {effect} ran {executions} times with {unrecorded_attempts} attempt(s) dying inside its window"
                            ),
                        )
                        .records([record.at]),
                    );
                }
                _ => {}
            }
        }
        for ((session, scope, logical, attempt), executions) in runs {
            let (Some((_, first)), Some((_, last))) = (executions.first(), executions.last())
            else {
                continue;
            };
            let window = last.saturating_sub(*first);
            let repeats = executions.len() as u64 - 1;
            if repeats > window {
                violations.push(
                    Violation::new(
                        INVARIANT,
                        format!(
                            "attempt {attempt} of call {logical} in scope {scope} ran its body {} times across {window} failed engine attempt(s)",
                            executions.len()
                        ),
                    )
                    .records(executions.iter().map(|(at, _)| *at))
                    .session(session),
                );
            }
        }
        violations
    }
}
