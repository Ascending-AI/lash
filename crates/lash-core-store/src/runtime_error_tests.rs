use crate::SessionId;
use crate::runtime_error::{RuntimeError, RuntimeErrorClass, RuntimeErrorCode};

#[test]
fn missing_process_execution_id_round_trips() {
    let err = RuntimeError::missing_process_execution_id();
    assert_eq!(err.code, RuntimeErrorCode::MissingProcessExecutionId);
    let json = serde_json::to_value(&err).expect("serialize runtime error");
    assert_eq!(json["code"], "missing_process_execution_id");
    let decoded: RuntimeError = serde_json::from_value(json).expect("decode runtime error");
    assert_eq!(decoded.code, RuntimeErrorCode::MissingProcessExecutionId);
}

#[test]
fn replay_mismatch_classification_covers_every_durable_controller_code() {
    for code in [
        "sqlite_effect_replay_hash_conflict",
        "postgres_effect_replay_hash_conflict",
        "worker_replacement_abort",
        "tool_intent_replay_key_format_cutover",
    ] {
        let typed = RuntimeErrorCode::from_wire_code(code);
        assert!(typed.is_replay_mismatch(), "{code}");
        assert_eq!(
            typed.as_str(),
            code,
            "classification must preserve display code"
        );
    }
}

#[test]
fn retired_restate_hash_mismatch_wire_code_decodes_to_the_current_classification() {
    let code = RuntimeErrorCode::from_wire_code("restate_effect_hash_mismatch");

    assert_eq!(code, RuntimeErrorCode::WorkerReplacementAbort);
    assert_eq!(code.as_str(), "worker_replacement_abort");
    assert!(code.is_replay_mismatch());
    assert!(code.is_terminal());
    let encoded = serde_json::to_value(&code).expect("serialize retired wire code");
    assert_eq!(encoded, serde_json::json!("worker_replacement_abort"));
}

#[test]
fn nearby_mismatch_codes_are_not_replay_divergence() {
    for code in [
        "runtime_effect_envelope_canonical_hash_invariant",
        "runtime_effect_local_executor_mismatch",
    ] {
        assert!(
            !RuntimeErrorCode::from_wire_code(code).is_replay_mismatch(),
            "{code}"
        );
    }
}

#[test]
fn session_execution_lease_lost_round_trips() {
    let err = RuntimeError::new(RuntimeErrorCode::SessionExecutionLeaseLost, "lease lost");
    let json = serde_json::to_value(&err).expect("serialize runtime error");
    assert_eq!(json["code"], "session_execution_lease_lost");
    let decoded: RuntimeError = serde_json::from_value(json).expect("decode runtime error");
    assert_eq!(decoded.code, RuntimeErrorCode::SessionExecutionLeaseLost);
}

#[test]
fn runtime_error_code_serializes_as_stable_string() {
    let err = RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, "commit failed");

    let json = serde_json::to_value(&err).expect("serialize runtime error");
    assert_eq!(json["code"], "store_commit_failed");

    let decoded: RuntimeError = serde_json::from_value(json).expect("decode runtime error");
    assert_eq!(decoded.code, RuntimeErrorCode::StoreCommitFailed);
}

#[test]
fn runtime_error_code_classification_is_exhaustive_and_disjoint() {
    // `classification` is one exhaustive match: a new variant does not
    // compile until it is classified, and cannot land in two classes. The
    // count pins ALL_FIRST_PARTY to the enum's first-party variants so the
    // iteration stays complete; `ForeignCode` is the one variant outside it.
    assert_eq!(
        RuntimeErrorCode::ALL_FIRST_PARTY.len(),
        184,
        "a new first-party variant must be added to ALL_FIRST_PARTY"
    );

    for code in RuntimeErrorCode::ALL_FIRST_PARTY {
        let class = code.classification();
        assert_eq!(
            code.is_retryable(),
            class == RuntimeErrorClass::Retryable,
            "{code}"
        );
        assert_eq!(
            code.is_terminal(),
            class == RuntimeErrorClass::Terminal,
            "{code}"
        );

        let json = serde_json::to_value(code).expect("serialize typed code");
        let decoded: RuntimeErrorCode =
            serde_json::from_value(json).expect("deserialize typed code");
        assert!(
            !matches!(&decoded, RuntimeErrorCode::ForeignCode(_)),
            "first-party code {} decoded as foreign",
            code.as_str()
        );
        assert_eq!(&decoded, code, "typed round trip for {code}");
    }

    let foreign = RuntimeErrorCode::from_wire_code("plugin_defined_abort");
    assert_eq!(foreign.classification(), RuntimeErrorClass::Unclassified);
}

/// A failed assistant-response hook is an incomplete derivation over a
/// completion the journal already holds, so the only correct recovery is to
/// redrive phase 2 (FIG-1276). That is a claim about `is_retryable`, not
/// merely about staying out of `is_terminal`: an unclassified code is
/// `Unknown`, which durable hosts are free to settle either way.
#[test]
fn assistant_response_hook_failures_are_retryable_not_terminal() {
    let code = RuntimeErrorCode::RuntimeEffectAssistantResponseHook;

    assert!(code.is_retryable(), "phase 2 must be redrivable");
    assert!(!code.is_terminal());
    assert_eq!(code.as_str(), "runtime_effect_assistant_response_hook");

    let error = RuntimeError::new(code.clone(), "assistant response hook failed");
    assert!(error.is_retryable());
    assert!(!error.is_terminal());
    assert_eq!(
        RuntimeErrorCode::from_wire_code("runtime_effect_assistant_response_hook"),
        code,
        "the wire code must decode as first-party, not foreign"
    );
}

#[test]
fn unsafe_effect_replay_and_durable_timeout_codes_are_terminal() {
    for code in [
        RuntimeErrorCode::PostgresEffectReplayLeaseLost,
        RuntimeErrorCode::SqliteEffectReplayLeaseLost,
        RuntimeErrorCode::ProcessSignalWaitTimeout,
    ] {
        assert!(!code.is_retryable(), "{code} must not be retried");
        assert!(code.is_terminal(), "{code} must settle terminally");
    }
}

#[test]
fn terminal_cause_overrides_retryable_runtime_store_code() {
    let error = RuntimeError::new(RuntimeErrorCode::RuntimeStore, "session deleted").with_cause(
        super::RuntimeErrorCause::SessionDeleted {
            session_id: SessionId::from("retired"),
        },
    );

    assert!(!error.is_retryable());
    assert!(error.is_terminal());
}

#[test]
fn foreign_runtime_error_code_round_trips() {
    let decoded: RuntimeError = serde_json::from_value(serde_json::json!({
        "code": "plugin_defined_abort",
        "message": "stopped by plugin"
    }))
    .expect("decode plugin runtime error");

    assert_eq!(
        decoded.code,
        RuntimeErrorCode::from_wire_code("plugin_defined_abort")
    );
    assert_eq!(decoded.code.as_str(), "plugin_defined_abort");
}

#[test]
fn wire_constructor_canonicalizes_built_in_codes() {
    let code = RuntimeErrorCode::from_wire_code("runtime_store");

    assert_eq!(code, RuntimeErrorCode::RuntimeStore);
    assert!(code.is_retryable());
    assert!(!code.is_terminal());
}
