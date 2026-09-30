//! Law (FIG-4262, ADR 0115): a request retried across a rolling upgrade
//! keeps its identity.
//!
//! A request first attempted by N and retried by N+1 after finalize carries
//! protocol turn options the fleet stamped at N's version on the first
//! attempt and at N+1's on the retry. Identity preimages exclude
//! format-version stamps, so the retry reproduces the request identity and
//! the intent hash, and the receipt decision replays it instead of refusing
//! it as a different request.

use super::*;
use crate::SessionId;
use crate::protocol_turn_options::PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION;
use crate::testing::guarded_surfaces::pinned;

/// The record-config request one generation writes: built by a runtime,
/// then planned by a store whose `F` is `encoded_under`, which stamps the
/// turn options the request carries with the version that `F` assigns.
fn record_config_request(encoded_under: FleetFormat) -> RuntimeCommit {
    let mut state = crate::RuntimeSessionState {
        session_id: SessionId::from("rolled"),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.protocol_turn_options = crate::ProtocolTurnOptions::from_payload(serde_json::json!({
        "mode": "rolled",
        "render": {"print": {"max_chars": 8000}}
    }));
    let mut commit = RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        OperationId::new(
            crate::ExecutionScope::runtime_operation(
                "session:rolled:boundary:protocol-materialization",
            ),
            "record-config",
        ),
    );
    commit
        .stamp_semantic_boundary()
        .expect("stamp the record-config request");
    RuntimeCommitPlanner::prepare(commit, encoded_under)
        .expect("plan the record-config request")
        .commit()
        .clone()
}

/// The turn-options stamps a request carries: its config's and its
/// checkpoint's.
fn stamps(commit: &RuntimeCommit) -> [u32; 2] {
    [
        commit
            .config
            .protocol_turn_options
            .as_ref()
            .expect("the config records turn options")
            .schema_version(),
        commit
            .checkpoint
            .turn_state
            .protocol_turn_options
            .schema_version(),
    ]
}

/// N's generation before finalize and N+1's after it, as the active tier
/// defines them, then a pair whose stamps differ in every tier: N's epoch
/// against the same epoch pinned to a version no writer of this build emits.
fn rolls() -> [(FleetFormat, FleetFormat); 2] {
    [
        (
            FleetFormat::seed(FleetFormat::writable()),
            FleetFormat::current(),
        ),
        (
            FleetFormat::seed(FleetFormat::writable()),
            pinned(
                "PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION",
                PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION + 1,
            ),
        ),
    ]
}

#[test]
fn a_request_retried_across_the_roll_keeps_its_identity() {
    for (first, retry) in rolls() {
        let stored = record_config_request(first);
        let attempted = record_config_request(retry);
        let surface = crate::surface_format!(PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION);
        assert_eq!(stamps(&stored), [first.writer_version(surface); 2]);
        assert_eq!(stamps(&attempted), [retry.writer_version(surface); 2]);

        let stored_hash = stored.turn_commit_hash().expect("first attempt intent");
        let attempted_hash = attempted.turn_commit_hash().expect("retry intent");
        assert_eq!(
            stored_hash,
            attempted_hash,
            "the intent hash does not depend on the turn-options stamp: {:?} then {:?}",
            stamps(&stored),
            stamps(&attempted)
        );
        assert_eq!(
            stored.turn_commit.append_request_identity,
            attempted.turn_commit.append_request_identity,
            "the request identity does not depend on the turn-options stamp"
        );
        assert_eq!(
            decide_runtime_commit_receipt(
                &stored_hash,
                &attempted_hash,
                &stored.turn_commit.append_request_identity,
                &attempted.turn_commit.append_request_identity,
            ),
            RuntimeCommitReceiptDecision::Replay,
            "the retry replays the first attempt's receipt"
        );
        commit_identity::validate_receipt_identity(&attempted)
            .expect("the retry's identity matches the content it rides with");
    }
    let [(_, _), (first, retry)] = rolls();
    assert_ne!(
        stamps(&record_config_request(first)),
        stamps(&record_config_request(retry)),
        "the discriminating roll stamps its two attempts differently"
    );
}
