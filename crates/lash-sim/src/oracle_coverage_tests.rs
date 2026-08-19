//! Red-side proofs for the workload-declared coverage floors.
//!
//! Every oracle exercised here states a universally-quantified law, so each one
//! is vacuously true over an empty observation set. That is not a hypothetical:
//! a generator, delivery-path, observer or projection break that produces zero
//! observations would make all of them report success at once, and the further
//! upstream the break the more of them go green together.
//!
//! Each test therefore feeds the oracle an empty observation set together with
//! the counts a *real* generated workload declared, and proves the verdict is
//! FAILED and names the declaration. The declaration itself is proved non-empty
//! first, so these proofs cannot themselves go vacuous.

use crate::generator::{default_max_boundaries, generate_workload};
use crate::oracles::{
    generated_final_value_semantic_channel, ingress_sessions_opened, lease_time_monotonic,
    observer_convergence, provider_transport_mutation_classified, provider_turn_interleaving_depth,
    runtime_session_graph_contract,
};
use crate::state_checker::checkpoint_state_consistency;
use crate::trace::{AbstractWorldSummary, OracleVerdict, WorkloadExpectations};

/// Every random workload profile, at the budget the runner actually uses.
const PROFILES: [&str; 3] = ["fast-random", "default-random", "full-random"];

/// What a real generated workload declares. Nothing is run: the point is that
/// the declaration exists *before* any observation does.
fn declared_for(profile: &str) -> WorkloadExpectations {
    let max_boundaries = default_max_boundaries(profile).expect("profile budget");
    generate_workload(7, profile, max_boundaries)
        .expect("workload")
        .expectations()
}

fn declared() -> WorkloadExpectations {
    declared_for("default-random")
}

fn empty_summary() -> AbstractWorldSummary {
    AbstractWorldSummary::with_digest(0, 0, Vec::new(), Vec::new(), Vec::new())
}

fn assert_absent_class(verdict: &OracleVerdict, oracle_id: &str, declared: usize) {
    assert!(
        !verdict.is_passed(),
        "`{oracle_id}` passed over an empty observation set: {}",
        verdict.message
    );
    assert_eq!(verdict.oracle_id, oracle_id);
    assert!(
        verdict.message.contains(&format!("declared {declared}")),
        "`{oracle_id}` must name the declared count it did not observe: {}",
        verdict.message
    );
}

/// The proofs below are only as strong as the declaration they rest on: a
/// profile that quietly stopped planning an observation class would declare
/// zero of it, drop that floor to nothing, and leave every red proof green.
/// So assert the declaration is non-trivial for every profile, not just the one
/// the proofs happen to use.
#[test]
fn every_workload_profile_declares_every_guarded_observation_class() {
    for profile in PROFILES {
        let declared = declared_for(profile);

        assert!(
            declared.session_count() > 1,
            "`{profile}` declared {} session(s): {declared:?}",
            declared.session_count()
        );
        assert_eq!(
            declared.sessions.len(),
            declared
                .sessions
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            "`{profile}` declared a duplicate session alias: {declared:?}"
        );
        assert!(
            declared.provider_turn_count > 0,
            "`{profile}`: {declared:?}"
        );
        assert!(
            declared.transport_mutation_count > 0,
            "`{profile}`: {declared:?}"
        );
        assert!(
            declared.lease_time_boundary_count > 0,
            "`{profile}`: {declared:?}"
        );
    }
}

#[test]
fn ingress_sessions_opened_fails_when_no_session_was_observed() {
    let declared = declared();
    let verdict = ingress_sessions_opened(&empty_summary(), &declared);
    assert_absent_class(
        &verdict,
        "sim.oracle.ingress-session-opened.v1",
        declared.session_count(),
    );
}

#[test]
fn observer_convergence_fails_when_no_session_was_observed() {
    let declared = declared();
    let verdict = observer_convergence(&empty_summary(), &declared);
    assert_absent_class(
        &verdict,
        "sim.oracle.observer-convergence.v1",
        declared.session_count(),
    );
}

#[test]
fn runtime_session_graph_contract_fails_when_no_session_was_observed() {
    let declared = declared();
    let verdict = runtime_session_graph_contract(&empty_summary(), &declared);
    assert_absent_class(
        &verdict,
        "sim.oracle.runtime-session-graph.v1",
        declared.session_count(),
    );
}

#[test]
fn provider_turn_interleaving_fails_when_no_declared_session_ran_a_turn() {
    let declared = declared();
    let verdict = provider_turn_interleaving_depth(&[], &declared);
    assert_absent_class(
        &verdict,
        "sim.oracle.provider-turn-interleaving-depth.v1",
        declared.session_count(),
    );
}

/// The one whose exemption was the actual bug: "the workload intentionally ran
/// one session" and "the run observed nothing" must not collapse into the same
/// pass. A declared single-session workload that still observed nothing fails.
#[test]
fn provider_turn_interleaving_exemption_is_not_granted_to_an_empty_run() {
    let declared = WorkloadExpectations::new(vec!["session-001".to_string()], 4, 0, 0);
    let verdict = provider_turn_interleaving_depth(&[], &declared);
    assert_absent_class(
        &verdict,
        "sim.oracle.provider-turn-interleaving-depth.v1",
        1,
    );
}

#[test]
fn provider_transport_mutation_fails_when_declared_mutations_never_landed() {
    let declared = declared();
    let verdict = provider_transport_mutation_classified(&[], &declared);
    assert_absent_class(
        &verdict,
        "sim.oracle.provider-transport-mutation-classified.v1",
        declared.transport_mutation_count,
    );
}

#[test]
fn lease_time_monotonic_fails_when_declared_lease_boundaries_never_landed() {
    let declared = declared();
    let verdict = lease_time_monotonic(&[], &declared);
    assert_absent_class(
        &verdict,
        "sim.oracle.lease-time-monotonic.v1",
        declared.lease_time_boundary_count,
    );
}

#[test]
fn generated_final_value_channel_fails_when_no_declared_turn_ran() {
    let declared = declared();
    let verdict = generated_final_value_semantic_channel(&[], &declared);
    assert_absent_class(
        &verdict,
        "sim.oracle.generated-final-value-semantic-channel.v1",
        declared.provider_turn_count,
    );
}

#[test]
fn checkpoint_state_consistency_fails_when_no_declared_session_committed() {
    let declared = declared();
    let verdict = checkpoint_state_consistency(&[], &[], &declared);
    assert_absent_class(
        &verdict,
        "sim.oracle.independent-checkpoint-state.v1",
        declared.session_count(),
    );
}

/// An undeclared trace (one recorded before the declaration existed) imposes no
/// floor — the guards key on the declaration, never on an ad hoc emptiness
/// suspicion. Generated runs always declare, so this path is only reachable for
/// promoted legacy fixtures.
#[test]
fn an_undeclared_workload_imposes_no_coverage_floor() {
    let undeclared = WorkloadExpectations::default();

    assert_eq!(undeclared.session_count(), 0);
    assert!(ingress_sessions_opened(&empty_summary(), &undeclared).is_passed());
    assert!(lease_time_monotonic(&[], &undeclared).is_passed());
    assert!(
        undeclared
            .sessions_missing_from(std::iter::empty())
            .is_empty()
    );
}
