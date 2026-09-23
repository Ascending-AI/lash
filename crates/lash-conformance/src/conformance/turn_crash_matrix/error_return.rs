//! The error-return placements and fail-stop oracle of the turn-crash
//! matrix (FIG-3524).
//!
//! Split out of `turn_crash_matrix.rs` to keep that file under the line
//! budget; the law and oracle keep their previous paths through the
//! parent's re-exports.

use super::*;
use pretty_assertions::assert_eq;

/// Where the fail-stop sweep substitutes a returned store error for the real
/// call during one scripted turn (FIG-3524).
///
/// These are error *returns*, not crashes: the seam answers the backend's
/// typed store error and the turn must stop — no durable commit, no tool
/// dispatch and no provider request may follow an unretried one. The three
/// journal placements fire inside the tool attempt's claim/execute/finalize
/// loop through the controller's `EffectJournalFaults`; `ToolAttempt`
/// injects the same typed error at the controller seam itself, which is the
/// only error-return coverage a non-journaled controller can offer.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ErrorReturnPlacement {
    /// `execute_effect(ToolAttempt)` returns the store-typed controller error.
    ToolAttempt,
    /// The journal's `claim` for the tool attempt's replay key returns the
    /// backend's `Store` vocabulary error.
    EffectJournalClaim,
    /// The journal's `finalize` for the same key returns it.
    EffectJournalFinalize,
    /// The effect-lease `renew` inside the tool attempt's execution loop
    /// returns it.
    EffectJournalRenew,
}

impl ErrorReturnPlacement {
    /// The fault point this placement arms on the journal, when it is a
    /// journal placement.
    pub(super) fn journal_point(
        self,
    ) -> Option<lash_core::facade_support::effect_replay_driver::EffectJournalFaultPoint> {
        use lash_core::facade_support::effect_replay_driver::EffectJournalFaultPoint;
        match self {
            Self::ToolAttempt => None,
            Self::EffectJournalClaim => Some(EffectJournalFaultPoint::Claim),
            Self::EffectJournalFinalize => Some(EffectJournalFaultPoint::Finalize),
            Self::EffectJournalRenew => Some(EffectJournalFaultPoint::Renew),
        }
    }

    /// Stable scenario-key suffix.
    fn key(self) -> &'static str {
        match self {
            Self::ToolAttempt => "tool-attempt",
            Self::EffectJournalClaim => "effect-journal-claim",
            Self::EffectJournalFinalize => "effect-journal-finalize",
            Self::EffectJournalRenew => "effect-journal-renew",
        }
    }
}

/// What the fail-stop oracle observed after one injected error return
/// (FIG-3524).
///
/// The conformance sweep fills this from the seam trace and the invocation's
/// journal-fault injector; `lash-sim` runners fill it from their own
/// instrumentation and call the same oracle.
#[derive(Clone, Debug, Default)]
pub struct FailStopObservation {
    /// Durable commits crossing the store or turn-control seam after the
    /// error returned.
    pub durable_commits: usize,
    /// Effect dispatches crossing the controller seam after the error
    /// returned.
    pub tool_dispatches: usize,
    /// Provider requests crossing the provider seam after the error returned.
    pub provider_requests: usize,
    /// The error code the turn's caller received, when the call failed.
    pub caller_error: Option<crate::RuntimeErrorCode>,
    /// Whether the faulted seam was called again after the error fired. A
    /// retried error is not *unretried*: work the turn does after the retry
    /// succeeds is the runtime honoring the retry, not a fail-stop breach.
    pub retried: bool,
}

/// One way a run broke the fail-stop law after an unretried error return.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailStopViolation {
    /// A durable commit followed the error.
    DurableCommit,
    /// A tool or effect dispatch followed the error.
    ToolDispatch,
    /// A provider request followed the error.
    ProviderRequest,
    /// The typed error did not reach the caller: the call either succeeded
    /// or failed with a different code.
    CallerMissedTypedError,
}

/// The fail-stop oracle (FIG-3524): after an *unretried* error return from a
/// durability seam, no durable commit, no tool dispatch and no provider
/// request may follow, and the typed error must reach the caller.
pub fn fail_stop_violations(
    observation: &FailStopObservation,
    expected_code: crate::RuntimeErrorCode,
) -> Vec<FailStopViolation> {
    let mut violations = Vec::new();
    if observation.retried {
        // The seam re-attempted the faulted call. Continued work past a
        // retried error is legal; what must not happen is the injected error
        // still surfacing as the caller's outcome.
        if observation.caller_error == Some(expected_code) {
            violations.push(FailStopViolation::CallerMissedTypedError);
        }
        return violations;
    }
    if observation.durable_commits > 0 {
        violations.push(FailStopViolation::DurableCommit);
    }
    if observation.tool_dispatches > 0 {
        violations.push(FailStopViolation::ToolDispatch);
    }
    if observation.provider_requests > 0 {
        violations.push(FailStopViolation::ProviderRequest);
    }
    if observation.caller_error != Some(expected_code) {
        violations.push(FailStopViolation::CallerMissedTypedError);
    }
    violations
}

/// One error-return placement's reviewed fail-stop ruling (FIG-3524).
///
/// Rows live in `turn_crash_outcomes.json` beside the crash-point and
/// durable-recovery rulings, under the same level-2 known-defect rule: a
/// non-empty `violations` requires a `ticket`, pins the exact defective
/// behavior, and flips to an empty list when the ticket lands — it is not a
/// skip.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct ErrorReturnRuling {
    /// The placement this row rules on.
    pub(super) placement: ErrorReturnPlacement,
    /// Reviewable prose explaining the ruling.
    pub(super) outcome: String,
    /// The fix ticket, when `violations` pins a known defect.
    #[serde(default)]
    pub(super) ticket: Option<String>,
    /// The fail-stop violations the placement exhibits today, pinned
    /// exactly; empty asserts the fail-stop law already holds.
    pub(super) violations: Vec<FailStopViolation>,
}

/// A ruling-table row carrying an error-return ruling. The `error_return`
/// key keeps untagged-row decoding unambiguous against crash-point and
/// durable-recovery rows.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct ErrorReturnRulingEntry {
    pub(super) error_return: ErrorReturnRuling,
}

/// Whether the seam op is a durable commit for fail-stop purposes: the
/// terminal head commit, a cancel-closure commit or authorization, and the
/// durable turn-control promise resolutions.
fn is_commit_seam(operation: &TurnSeamOperation) -> bool {
    matches!(
        operation,
        TurnSeamOperation::Store(
            StoreOperation::CommitFinalHead { .. }
                | StoreOperation::ApplyTurnCancelEffectsAndConsume
                | StoreOperation::AuthorizeTurnCancelClosure
        ) | TurnSeamOperation::TurnControl(_)
    )
}

/// Whether the seam op dispatches new effect work. The failed child's own
/// group settles and closes after its attempt errors: consuming that
/// settlement and releasing the group is how the error reaches the turn, the
/// group-path twin of a batch returning its failed reply (FIG-3397), not a
/// dispatch.
fn is_dispatch_seam(operation: &TurnSeamOperation) -> bool {
    matches!(operation, TurnSeamOperation::Effect(_))
        && !matches!(
            operation,
            TurnSeamOperation::Effect(EffectOperation::GroupSettle | EffectOperation::GroupClose)
        )
}

/// Sweep every error-return placement through one scripted turn per backend
/// and hold the fail-stop oracle on each (FIG-3524).
///
/// `make_error_invocation` supplies the controller under test: a journaled
/// controller with its [`crate::ConformanceInvocation::effect_journal_faults`]
/// attached on the SQL tiers, a native or foreign controller elsewhere. The
/// journal placements run only where the invocation exposes a journal; the
/// tool-attempt placement runs everywhere.
pub async fn turn_crash_matrix_error_return_fail_stop<F, I>(make: F, make_error_invocation: I)
where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
    I: Fn(&str, crate::ExecutionScope) -> crate::ConformanceInvocation,
{
    let rulings = error_return_rulings();
    validate_error_return_rulings(&rulings)
        .unwrap_or_else(|error| panic!("invalid error-return rulings: {error}"));
    for ruling in &rulings {
        let scenario = format!("error-return-{}", ruling.placement.key());
        let identity = ReferenceIdentity::for_scenario(&scenario);
        let scope =
            crate::ExecutionScope::queue_drain(&identity.session_id, identity.turn_id.as_str());
        let invocation = make_error_invocation(&scenario, scope);
        if ruling.placement.journal_point().is_some()
            && invocation.effect_journal_faults().is_none()
        {
            // No effect journal behind this controller: the placement does
            // not exist on this backend.
            continue;
        }
        Box::pin(run_error_return_case(
            &make, invocation, ruling, &scenario, &identity,
        ))
        .await;
    }
}

/// Run one scripted turn with `ruling.placement` armed, then hold the
/// fail-stop oracle on what the turn did after the error returned.
async fn run_error_return_case<F>(
    make: &F,
    invocation: crate::ConformanceInvocation,
    ruling: &ErrorReturnRuling,
    scenario: &str,
    identity: &ReferenceIdentity,
) where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let raw = make(scenario);
    seed_reference_ingress(&raw, identity, scenario).await;
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let decorated = SeamStore::wrap(raw, control.clone());
    let journal_faults = invocation.effect_journal_faults();
    let effect_controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
        inner: invocation.controller_handle(),
        control: control.clone(),
        executions: Arc::clone(&executions),
        journal_faults: journal_faults.clone(),
    });
    let trace_tool = TraceTool {
        journal_faults: journal_faults.clone(),
        ..TraceTool::default()
    };
    let runtime = Box::pin(build_runtime(
        decorated,
        control.clone(),
        Arc::clone(&effect_controller),
        identity,
        trace_tool,
    ))
    .await;
    control.arm_error_return(ruling.placement);
    let result = Box::pin(drive_turn(runtime, effect_controller, identity)).await;
    invocation.end();

    if let Some(faults) = &journal_faults
        && ruling.placement.journal_point().is_some()
    {
        assert!(
            faults.fired(),
            "{scenario} ({:?}): the armed journal fault never fired; the placement covered nothing",
            ruling.placement
        );
    }
    let trace = control.trace();
    let error_index = trace
        .iter()
        .position(|operation| {
            matches!(
                operation,
                TurnSeamOperation::Effect(EffectOperation::ToolAttempt { .. })
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "{scenario} ({:?}): the tool-attempt seam was never reached",
                ruling.placement
            )
        });
    let continued = &trace[error_index + 1..];
    let observation = FailStopObservation {
        durable_commits: continued.iter().filter(|op| is_commit_seam(op)).count(),
        tool_dispatches: continued.iter().filter(|op| is_dispatch_seam(op)).count(),
        provider_requests: continued
            .iter()
            .filter(|op| matches!(op, TurnSeamOperation::Provider(_)))
            .count(),
        caller_error: result.err().map(|error| error.code),
        retried: journal_faults
            .as_ref()
            .is_some_and(|faults| faults.calls_after_fire() > 0),
    };
    let expected_code = journal_faults.map_or(crate::RuntimeErrorCode::RuntimeStore, |faults| {
        faults.store_code()
    });
    let violations = fail_stop_violations(&observation, expected_code);
    match &ruling.ticket {
        None => assert!(
            violations.is_empty(),
            "{scenario} ({:?}): fail-stop violated: {violations:?} \
             (observation: {observation:?})",
            ruling.placement
        ),
        Some(ticket) => assert_eq!(
            violations, ruling.violations,
            "{scenario} ({:?}): known defect {ticket} drifted: observed {violations:?}, \
             expected {:?} — if the fix landed, flip this ruling to fail-stop",
            ruling.placement, ruling.violations
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_stop_oracle_accepts_a_clean_unretried_stop() {
        let observation = FailStopObservation {
            caller_error: Some(crate::RuntimeErrorCode::RuntimeStore),
            ..FailStopObservation::default()
        };
        assert!(
            fail_stop_violations(&observation, crate::RuntimeErrorCode::RuntimeStore).is_empty()
        );
    }

    #[test]
    fn fail_stop_oracle_flags_each_continued_seam() {
        let observation = FailStopObservation {
            durable_commits: 1,
            tool_dispatches: 1,
            provider_requests: 1,
            caller_error: None,
            retried: false,
        };
        pretty_assertions::assert_eq!(
            fail_stop_violations(&observation, crate::RuntimeErrorCode::RuntimeStore),
            vec![
                FailStopViolation::DurableCommit,
                FailStopViolation::ToolDispatch,
                FailStopViolation::ProviderRequest,
                FailStopViolation::CallerMissedTypedError,
            ]
        );
    }

    #[test]
    fn fail_stop_oracle_flags_a_wrong_error_code_reaching_the_caller() {
        let observation = FailStopObservation {
            caller_error: Some(crate::RuntimeErrorCode::RuntimeEffectScopeMismatch),
            ..FailStopObservation::default()
        };
        pretty_assertions::assert_eq!(
            fail_stop_violations(&observation, crate::RuntimeErrorCode::RuntimeStore),
            vec![FailStopViolation::CallerMissedTypedError]
        );
    }

    #[test]
    fn fail_stop_oracle_ignores_continuation_after_a_retried_error() {
        // The seam re-attempted the faulted call: continued work is the
        // runtime honoring the retry, not a fail-stop breach.
        let observation = FailStopObservation {
            durable_commits: 3,
            tool_dispatches: 1,
            provider_requests: 2,
            caller_error: None,
            retried: true,
        };
        assert!(
            fail_stop_violations(&observation, crate::RuntimeErrorCode::RuntimeStore).is_empty()
        );
        // But the injected error must still not surface as the outcome.
        let leaked = FailStopObservation {
            retried: true,
            caller_error: Some(crate::RuntimeErrorCode::RuntimeStore),
            ..FailStopObservation::default()
        };
        pretty_assertions::assert_eq!(
            fail_stop_violations(&leaked, crate::RuntimeErrorCode::RuntimeStore),
            vec![FailStopViolation::CallerMissedTypedError]
        );
    }

    #[test]
    fn error_return_validation_rejects_violations_without_a_ticket() {
        let mut rulings = error_return_rulings();
        assert!(
            validate_error_return_rulings(&rulings).is_ok(),
            "the synthetic test must start from valid rulings"
        );
        let defective = rulings
            .iter_mut()
            .find(|ruling| !ruling.violations.is_empty())
            .expect("a defective ruling fixture");
        defective.ticket = None;
        assert!(
            validate_error_return_rulings(&rulings).is_err(),
            "pinned violations without a ticket must invalidate the oracle"
        );
    }

    #[test]
    fn error_return_validation_rejects_a_dropped_placement() {
        let mut rulings = error_return_rulings();
        rulings.remove(0);
        assert!(
            validate_error_return_rulings(&rulings).is_err(),
            "dropping a placement's ruling must invalidate the oracle"
        );
    }
}
