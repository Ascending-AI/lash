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
        "lashlang_cell_replay_divergence",
        "lashlang_cell_replay_key_format_cutover",
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
    // A replaced worker's journal disagreeing is live: a fresh drive succeeds
    // (FIG-3575).
    assert!(!code.is_terminal());
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
        197,
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

    let decoded = RuntimeErrorCode::from_wire_code("plugin_defined_abort");
    assert_eq!(decoded.classification(), RuntimeErrorClass::Terminal);
}

/// A failed assistant-response hook is an incomplete derivation over a
/// completion the journal already holds, so the only correct recovery is to
/// redrive phase 2 (FIG-1276). That is a claim about `is_retryable`, not
/// merely about staying out of `is_terminal`.
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

/// FIG-3575: a lost journal lease is a live fault. The identical call is not
/// safe to repeat, so it is not retryable, but a redrive under a fresh lease
/// succeeds, so it is not terminal either. A durable timeout is terminal.
#[test]
fn journal_lease_loss_is_redrivable_and_a_durable_timeout_is_terminal() {
    for code in [
        RuntimeErrorCode::PostgresEffectReplayLeaseLost,
        RuntimeErrorCode::SqliteEffectReplayLeaseLost,
    ] {
        assert!(!code.is_retryable(), "{code} must not be retried unchanged");
        assert!(!code.is_terminal(), "{code} must stay redrivable");
    }
    assert!(RuntimeErrorCode::ProcessSignalWaitTimeout.is_terminal());
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

#[test]
fn queued_run_refusals_require_explicit_host_disposition() {
    let error = crate::runtime_error::runtime_error_from_store_commit(
        crate::store::StoreError::QueuedRunConfigurationChanged {
            session_id: "changed".into(),
        },
    );
    assert_eq!(error.code, RuntimeErrorCode::QueuedRunConfigurationChanged);
    assert!(error.is_terminal());
    assert!(!error.is_retryable());
    assert!(RuntimeErrorCode::QueuedRunFailed.is_terminal());
}

/// FIG-3435: a `lash:` spelling must mean exactly one thing. A collision
/// across the two first-party vocabularies would let a durable
/// `RuntimeErrorCode` decode as a turn-failure arm (or the reverse) at any
/// bare-spelling compatibility boundary.
#[test]
fn runtime_and_turn_failure_spellings_never_collide() {
    use lash_sansio::session_model::TurnFailureCode;

    for code in RuntimeErrorCode::ALL_FIRST_PARTY {
        assert!(
            !TurnFailureCode::ALL_NAMED
                .iter()
                .any(|named| named.as_str() == code.as_str()),
            "runtime spelling `{}` collides with a turn-failure arm",
            code.as_str()
        );
    }
    for code in TurnFailureCode::ALL_NAMED {
        assert!(
            matches!(
                RuntimeErrorCode::from_wire_code(code.as_str()),
                RuntimeErrorCode::ForeignCode(_)
            ),
            "turn-failure spelling `{}` collides with a runtime error arm",
            code.as_str()
        );
    }
}

/// FIG-3435: converting a `RuntimeErrorCode` into a `FailureCode` keeps
/// ownership honest. Built-in spellings are workspace vocabulary and land in
/// `lash`; a `ForeignCode` decodes through foreign ingress — a genuine
/// foreign pair keeps its namespace, while a foreign value claiming a
/// reserved namespace or carrying none lands in `foreign`, never `lash`.
#[test]
fn runtime_error_code_conversion_never_mints_lash_for_foreign_codes() {
    use lash_sansio::session_model::{FailureCode, Namespace};

    let built_in = FailureCode::from(&RuntimeErrorCode::RuntimeStore);
    assert_eq!(built_in.namespace(), &Namespace::LASH);
    assert_eq!(built_in.spelling(), "runtime_store");

    let foreign = FailureCode::from(&RuntimeErrorCode::from_wire_code("plugin_defined_abort"));
    assert_eq!(foreign.namespace().as_str(), "foreign");
    assert_eq!(foreign.spelling(), "plugin_defined_abort");
    assert_eq!(foreign.turn_code(), None);

    // A foreign spelling that collides with a Lash arm must not launder into
    // `lash` — the foreign-ingress decode demotes the claim, it never grants
    // it.
    let colliding = FailureCode::from(&RuntimeErrorCode::from_wire_code("timeout"));
    assert_eq!(colliding.namespace().as_str(), "foreign");
    assert_eq!(colliding.turn_code(), None);

    let claiming = FailureCode::from(&RuntimeErrorCode::from_wire_code("lash:timeout"));
    assert_eq!(claiming.namespace().as_str(), "foreign");
    assert_eq!(claiming.spelling(), "lash:timeout");
    assert_eq!(claiming.turn_code(), None);

    // A genuine foreign pair keeps both halves verbatim.
    let pair = FailureCode::from(&RuntimeErrorCode::from_wire_code(
        "agent_workbench:spend_cap",
    ));
    assert_eq!(pair.namespace().as_str(), "agent_workbench");
    assert_eq!(pair.spelling(), "spend_cap");
}

#[test]
fn turn_input_source_key_conflict_is_a_typed_identity_conflict() {
    let conflict = || crate::store::StoreError::PendingTurnInputSourceKeyConflict {
        session_id: SessionId::from("session"),
        source_key: "host:retry".to_string(),
        existing_input_id: crate::InputId::new("ti:existing"),
    };
    for error in [
        crate::runtime_error::runtime_error_from_turn_input_admission(conflict()),
        crate::runtime_error::runtime_error_from_store_commit(conflict()),
    ] {
        assert_eq!(error.code, RuntimeErrorCode::DurableIdentityConflict);
        assert!(error.is_terminal() && !error.is_retryable());
    }
    assert_eq!(
        crate::runtime_error::runtime_error_from_turn_input_admission(
            crate::store::StoreError::Backend("disk".to_string())
        )
        .code,
        RuntimeErrorCode::StoreCommitFailed
    );
}

/// FIG-3575: one answer per code. A code is terminal exactly when a failed
/// turn settles it as an outcome, with no exceptions, for every first-party
/// code and for a foreign code of either class. A retryable code is a live
/// fault by construction.
#[test]
fn a_code_is_terminal_exactly_when_it_is_an_outcome() {
    use crate::runtime_error::TurnFailureCause;

    let decoded = [RuntimeErrorCode::from_wire_code("recorded_host_failure")];
    for code in RuntimeErrorCode::ALL_FIRST_PARTY.iter().chain(&decoded) {
        let cause = code.turn_failure_cause();
        assert_eq!(
            code.is_terminal(),
            cause == TurnFailureCause::Outcome,
            "{code}: terminal and outcome must agree"
        );
        if code.is_retryable() {
            assert_eq!(cause, TurnFailureCause::LiveFault, "{code}");
        }
    }
    // A foreign code carries the class its minting host chose on the error.
    for (cause, terminal) in [
        (TurnFailureCause::Outcome, true),
        (TurnFailureCause::LiveFault, false),
    ] {
        let minted = RuntimeError::foreign("host_code", cause, "minted");
        assert_eq!(minted.turn_failure_cause(), cause);
        assert_eq!(minted.is_terminal(), terminal);
        let controller = crate::runtime_error::RuntimeEffectControllerError::foreign(
            "host_code",
            cause,
            "minted",
        );
        assert_eq!(controller.turn_failure_cause(), cause);
        assert_eq!(controller.into_runtime_error().turn_failure_cause(), cause);
    }
    for outcome in [
        RuntimeErrorCode::ProtocolBeforeLlmCall,
        RuntimeErrorCode::SqliteEffectReplayHashConflict,
        RuntimeErrorCode::PostgresEffectReplayHashConflict,
        RuntimeErrorCode::RestateProcessJournalIdentityDrift,
        RuntimeErrorCode::ToolIntentReplayKeyFormatCutover,
    ] {
        assert_eq!(
            outcome.turn_failure_cause(),
            TurnFailureCause::Outcome,
            "{outcome}"
        );
    }
    for live in [
        RuntimeErrorCode::SessionExecutionLeaseLost,
        RuntimeErrorCode::StoreCommitFailed,
        RuntimeErrorCode::ExecutionStateCaptureFailed,
        RuntimeErrorCode::SqliteEffectReplayStore,
        RuntimeErrorCode::PostgresEffectReplayStore,
        RuntimeErrorCode::SqliteEffectReplayLeaseLost,
        RuntimeErrorCode::PostgresEffectReplayLeaseLost,
        RuntimeErrorCode::RuntimeEffectTaskJoin,
        RuntimeErrorCode::RuntimeEffectLocalTaskClosed,
        RuntimeErrorCode::RuntimeEffectProcessTaskJoin,
        RuntimeErrorCode::RuntimeEffectAttachmentStore,
        RuntimeErrorCode::RestateEffectController,
        RuntimeErrorCode::RestateProcessAwait,
        RuntimeErrorCode::WorkerReplacementAbort,
        RuntimeErrorCode::RuntimeEffectSleepCancelled,
        RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
    ] {
        assert_eq!(
            live.turn_failure_cause(),
            TurnFailureCause::LiveFault,
            "{live}"
        );
    }
}

/// FIG-3528 principle: a journaled controller error is the recorded outcome
/// of its effect whatever its code, so a redrive replays it instead of
/// aborting on it forever.
#[test]
fn a_journaled_controller_error_is_an_outcome_whatever_its_code() {
    use crate::runtime_error::{RuntimeEffectControllerError, TurnFailureCause};

    let live = RuntimeEffectControllerError::new(RuntimeErrorCode::SqliteEffectReplayStore, "io");
    assert_eq!(live.turn_failure_cause(), TurnFailureCause::LiveFault);
    assert_eq!(
        live.into_journaled().turn_failure_cause(),
        TurnFailureCause::Outcome
    );
}

/// FIG-3575: the acceptance an aborted direct turn returns rides the error and
/// is absent from the wire form of every other error.
#[test]
fn an_aborted_turn_error_carries_its_acceptance_receipt() {
    let plain = RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, "commit failed");
    let json = serde_json::to_value(&plain).expect("serialize runtime error");
    assert!(json.get("turn_input_acceptance").is_none());

    let receipt = crate::turn_input_vocabulary::TurnInputAcceptanceReceipt {
        input_id: crate::InputId::from("input-1"),
        session_id: SessionId::from("session-1"),
        source_key: None,
        ingress: crate::turn_input_vocabulary::TurnInputIngress::NextTurn,
    };
    let aborted = plain.with_turn_input_acceptance(receipt.clone());
    let decoded: RuntimeError =
        serde_json::from_value(serde_json::to_value(&aborted).expect("serialize"))
            .expect("decode runtime error");
    assert_eq!(decoded.turn_input_acceptance.as_deref(), Some(&receipt));
}

/// FIG-3586: a lashlang replay refusal parks its turn. It is neither an
/// outcome — nothing about the turn failed, and a redeploy of the build that
/// wrote the journal serves it — nor a live fault a queued run may spend its
/// retry budget on, since every redrive by this build refuses again.
#[test]
fn lashlang_replay_refusals_park_the_turn() {
    use crate::runtime_error::TurnFailureCause;

    for code in [
        RuntimeErrorCode::LashlangCellReplayDivergence,
        RuntimeErrorCode::LashlangCellReplayKeyFormatCutover,
    ] {
        assert_eq!(
            code.turn_failure_cause(),
            TurnFailureCause::Parked,
            "{code}"
        );
        assert!(code.parks_turn(), "{code}");
        assert!(!code.is_terminal(), "{code}: a parked turn is not failed");
        assert!(
            !code.is_retryable(),
            "{code}: a parked turn is not retried live"
        );
        assert!(TurnFailureCause::Parked.aborts_invocation());
        let controller = crate::runtime_error::RuntimeEffectControllerError::new(code.clone(), "x");
        assert_eq!(controller.turn_failure_cause(), TurnFailureCause::Parked);
        assert_eq!(
            controller.into_runtime_error().turn_failure_cause(),
            TurnFailureCause::Parked
        );
    }
    for code in RuntimeErrorCode::ALL_FIRST_PARTY {
        assert_eq!(
            code.parks_turn(),
            matches!(
                code,
                RuntimeErrorCode::LashlangCellReplayDivergence
                    | RuntimeErrorCode::LashlangCellReplayKeyFormatCutover
            ),
            "{code}: only the lashlang replay refusals park"
        );
    }
}

/// FIG-3619 (lead ruling): the two session-state codes are additive. A stored
/// refusal carries only its code and message — the typed generations travel
/// in-process on the returned error and are never serialized — so a build
/// from before these codes decodes the stored error without failing, reading
/// the code as a foreign recorded outcome and re-encoding the same bytes.
#[test]
fn a_stored_session_state_refusal_reads_as_a_foreign_code_before_the_codes_existed() {
    let added = [
        RuntimeErrorCode::SessionStateVersionUnsupported,
        RuntimeErrorCode::SessionStateVersionNewerThanRuntime,
    ];
    // The decoder before this change is this `from_wire_code` without the two
    // arms: every other spelling keeps its arm, and an unknown one falls
    // through to `other => Self::ForeignCode(other.to_string())`.
    let pre_change_decode = |spelling: &str| {
        if added.iter().any(|code| code.as_str() == spelling) {
            RuntimeErrorCode::ForeignCode(spelling.to_string())
        } else {
            RuntimeErrorCode::from_wire_code(spelling)
        }
    };
    for (error, code) in [
        (
            crate::StoreError::SessionStateVersionUnsupported {
                found: 2,
                current: 3,
            },
            RuntimeErrorCode::SessionStateVersionUnsupported,
        ),
        (
            crate::StoreError::SessionStateVersionNewerThanRuntime {
                found: 4,
                current: 3,
            },
            RuntimeErrorCode::SessionStateVersionNewerThanRuntime,
        ),
    ] {
        let (found, current) = match &error {
            crate::StoreError::SessionStateVersionUnsupported { found, current }
            | crate::StoreError::SessionStateVersionNewerThanRuntime { found, current } => {
                (*found, *current)
            }
            _ => unreachable!("the cases are session-state refusals"),
        };
        let refused = crate::runtime_error::runtime_error_from_store_commit(error);
        assert_eq!(refused.code, code);
        assert_eq!(
            refused.session_state_version_refusal(),
            Some(crate::runtime_error::SessionStateVersionRefusal { found, current }),
            "the refused call returns the generations typed"
        );

        let stored = serde_json::to_value(&refused).expect("serialize the refusal");
        assert_eq!(
            stored,
            serde_json::json!({ "code": code.as_str(), "message": refused.message }),
            "a stored refusal is its code and message; the generations are not persisted"
        );
        assert!(
            refused.message.contains(&found.to_string())
                && refused.message.contains(&current.to_string()),
            "the stored message names both generations: {}",
            refused.message
        );

        let spelling = stored["code"].as_str().expect("the code is a string");
        let before = pre_change_decode(spelling);
        assert_eq!(
            before,
            RuntimeErrorCode::ForeignCode(spelling.to_string()),
            "a build without the code reads it as a foreign code"
        );
        assert!(
            before.is_terminal(),
            "a foreign code is a recorded outcome, not a retry"
        );
        assert_eq!(
            serde_json::to_value(&before).expect("re-encode the foreign code"),
            stored["code"],
            "the foreign code re-encodes to the same bytes"
        );

        let decoded: RuntimeError =
            serde_json::from_value(stored).expect("this build decodes the stored refusal");
        assert_eq!(decoded.code, code);
        assert_eq!(decoded.session_state_version_refusal(), None);
    }
}
