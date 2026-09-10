use super::*;

pub const LIVE_PROVIDER_FAILURE_ORACLE: &str = "sim.oracle.live-provider-failure-terminalizes.v1";

/// Observed facts from driving a non-retryable provider failure through a LIVE
/// runtime turn (a real `session.turn().run()` whose scripted-transport events
/// are released by a real `BoundaryScheduler`, not an isolated
/// `provider.complete()`). The fault arrives AFTER one or more valid prose
/// deltas, so "no committed output" is a non-vacuous assertion that can fail.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LiveProviderFailureFacts {
    pub provider_kind: String,
    /// The non-retryable fault injected mid-turn (e.g. `malformed_sse_chunk`).
    pub fault_kind: String,
    /// How many VALID prose deltas the wire script streamed BEFORE the fault. Must
    /// be > 0: it is the partial prose a leaky runtime could (wrongly) commit, so
    /// it makes the "no committed output" assertion meaningful rather than vacuous.
    pub offered_prose_deltas: usize,
    /// How many prose deltas the runtime actually emitted to the activity stream
    /// before failing (diagnostic; proves the partial prose reached the runtime).
    pub streamed_prose_deltas: usize,
    /// The turn was observed live and parked on the first scheduler-gated provider
    /// event before any event was released (proves it is a live turn).
    pub turn_was_live_parked: bool,
    /// The turn ended in a terminal failure (returned an error or a non-success
    /// outcome) rather than finishing successfully.
    pub terminalized_failure: bool,
    /// The COMMITTED turn result carried a non-empty assistant message (a leak).
    pub committed_assistant_message_nonempty: bool,
    /// The COMMITTED turn result carried a Final Value (a leak).
    pub committed_final_values: usize,
    /// The committed session transcript contains the offered partial prose (a
    /// leak even if it is not the turn's outcome).
    pub committed_prose_in_transcript: bool,
}

/// A live runtime turn that receives a non-retryable provider failure AFTER valid
/// partial prose MUST terminalize with a terminal failure and commit NONE of that
/// prose — not as the turn outcome, not as a Final Value, and not in the session
/// transcript. Failing-capable AND non-vacuous: it requires that real prose was
/// offered before the fault (`offered_prose_deltas > 0`), so a runtime that leaks
/// the partial prose, or that finishes successfully, fails the oracle.
pub fn live_provider_failure_terminalizes(facts: &LiveProviderFailureFacts) -> OracleVerdict {
    let offered_prose = facts.offered_prose_deltas > 0;
    let no_committed_output = !facts.committed_assistant_message_nonempty
        && facts.committed_final_values == 0
        && !facts.committed_prose_in_transcript;
    if facts.turn_was_live_parked
        && offered_prose
        && facts.terminalized_failure
        && no_committed_output
    {
        OracleVerdict::passed(
            LIVE_PROVIDER_FAILURE_ORACLE,
            format!(
                "a live `{}` turn parked on a scheduler-gated provider event, streamed {} of {} offered valid prose delta(s), then a non-retryable `{}` fault terminalized it with NO committed output (no assistant message, no final value, no leaked prose in the transcript)",
                facts.provider_kind,
                facts.streamed_prose_deltas,
                facts.offered_prose_deltas,
                facts.fault_kind
            ),
        )
    } else {
        OracleVerdict::failed(
            LIVE_PROVIDER_FAILURE_ORACLE,
            format!(
                "live `{}` provider failure turn did not terminalize cleanly for fault `{}`: live_parked={} offered_prose_deltas={} streamed_prose_deltas={} terminalized_failure={} committed_assistant_message_nonempty={} committed_final_values={} committed_prose_in_transcript={}",
                facts.provider_kind,
                facts.fault_kind,
                facts.turn_was_live_parked,
                facts.offered_prose_deltas,
                facts.streamed_prose_deltas,
                facts.terminalized_failure,
                facts.committed_assistant_message_nonempty,
                facts.committed_final_values,
                facts.committed_prose_in_transcript
            ),
        )
    }
}

pub const LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE: &str =
    "sim.oracle.live-provider-failure-coverage.v1";

/// Aggregate the per-combo live-failure facts for a seed: every combo must pass
/// the per-turn oracle, and the set must exercise more than one provider kind and
/// more than one fault position, so the oracle cannot pass vacuously on a single
/// degenerate case.
pub fn live_provider_failure_coverage(facts: &[LiveProviderFailureFacts]) -> OracleVerdict {
    if let Some(failed) = facts
        .iter()
        .map(live_provider_failure_terminalizes)
        .find(|verdict| !verdict.is_passed())
    {
        return OracleVerdict::failed(
            LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE,
            format!("a live provider failure combo failed: {}", failed.message),
        );
    }
    let kinds = facts
        .iter()
        .map(|fact| fact.provider_kind.as_str())
        .collect::<BTreeSet<_>>();
    let positions = facts
        .iter()
        .map(|fact| fact.offered_prose_deltas)
        .collect::<BTreeSet<_>>();
    if kinds.len() < 2 || positions.len() < 2 {
        return OracleVerdict::failed(
            LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE,
            format!(
                "live provider failure coverage was not exercised broadly enough: {} provider kind(s) {:?}, {} fault position(s) {:?} (need >= 2 of each)",
                kinds.len(),
                kinds,
                positions.len(),
                positions
            ),
        );
    }
    OracleVerdict::passed(
        LIVE_PROVIDER_FAILURE_COVERAGE_ORACLE,
        format!(
            "{} live provider failure turns terminalized with no committed output across {} provider kinds {:?} and {} fault positions {:?}",
            facts.len(),
            kinds.len(),
            kinds,
            positions.len(),
            positions
        ),
    )
}

pub fn combine_oracles(oracles: &[OracleVerdict]) -> OracleVerdict {
    if let Some(failure) = oracles.iter().find(|oracle| !oracle.is_passed()) {
        return failure.clone();
    }
    OracleVerdict::passed(
        "sim.oracle.generated-workload.v1",
        format!("{} generated workload oracles passed", oracles.len()),
    )
}
