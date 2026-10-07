use crate::SessionId;
use crate::runtime_error::{RuntimeError, RuntimeErrorClass, RuntimeErrorCode};

/// The retired replacement-abort codes are not aliased (clean cutover): a
/// stored error carrying one decodes as a foreign code, never as the
/// engine-neutral divergence.
#[test]
fn retired_replacement_abort_wire_codes_are_not_aliased() {
    for retired in ["worker_replacement_abort", "restate_effect_hash_mismatch"] {
        let code = RuntimeErrorCode::from_wire_code(retired);
        assert!(
            matches!(code, RuntimeErrorCode::ForeignCode(_)),
            "{retired}: {code:?}"
        );
        assert!(!code.is_replay_mismatch(), "{retired}");
        assert!(!code.parks_turn(), "{retired}");
    }
    let code = RuntimeErrorCode::from_wire_code("effect_replay_divergence");
    assert_eq!(code, RuntimeErrorCode::EffectReplayDivergence);
    assert_eq!(
        serde_json::to_value(&code).expect("serialize divergence code"),
        serde_json::json!("effect_replay_divergence")
    );
}

#[test]
fn runtime_error_code_classification_is_exhaustive_and_disjoint() {
    // The code declaration generates both classification and law iteration.
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

/// A `lash:` spelling belongs to exactly one failure vocabulary.
#[test]
fn runtime_and_turn_failure_spellings_do_not_overlap() {
    use lash_sansio::session_model::TurnFailureCode;

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

/// FIG-3575: one answer per code. A code is terminal exactly when a failed
/// turn settles it as an outcome, with no exceptions, for every first-party
/// code and for a foreign code of either class. A retryable code is a live
/// fault by construction. A parked code (FIG-3586, FIG-3587) is its own
/// third class: neither a recorded failed turn nor a live fault a retry may
/// spend budget on, so it is neither terminal nor retryable.
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
        match cause {
            TurnFailureCause::Outcome => {
                assert!(code.is_terminal() && !code.parks_turn(), "{code}");
            }
            TurnFailureCause::LiveFault => {
                assert!(!code.is_terminal() && !code.parks_turn(), "{code}");
            }
            TurnFailureCause::Parked => {
                assert!(
                    !code.is_terminal() && !code.is_retryable() && code.parks_turn(),
                    "{code}: a parked code is neither an outcome nor a live fault"
                );
            }
        }
    }
    // The parked class, named: the replay refusals, which a redrive of the
    // same build refuses again with zero dispatch.
    let parked = RuntimeErrorCode::ALL_FIRST_PARTY
        .iter()
        .filter(|code| code.turn_failure_cause() == TurnFailureCause::Parked)
        .map(RuntimeErrorCode::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        parked,
        [
            RuntimeErrorCode::LashlangCellReplayDivergence,
            RuntimeErrorCode::RetiredGeneration,
            RuntimeErrorCode::PluginRevisionUnavailable,
            RuntimeErrorCode::LashlangCellBindingDrift,
            RuntimeErrorCode::EffectReplayDivergence,
            RuntimeErrorCode::VmWorkerUnavailable,
        ]
        .iter()
        .map(RuntimeErrorCode::as_str)
        .collect::<std::collections::BTreeSet<_>>(),
        "replay and worker deployment refusals park"
    );
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
        RuntimeErrorCode::EngineProcessJournalIdentityDrift,
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
        RuntimeErrorCode::RuntimeEffectTaskJoin,
        RuntimeErrorCode::RuntimeEffectLocalTaskClosed,
        RuntimeErrorCode::RuntimeEffectProcessTaskJoin,
        RuntimeErrorCode::RuntimeEffectAttachmentStore,
        RuntimeErrorCode::EngineEffectController,
        RuntimeErrorCode::EngineProcessAwait,
        RuntimeErrorCode::RuntimeEffectSleepCancelled,
        RuntimeErrorCode::RuntimeToolRunAwaitCancelled,
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

    let live = RuntimeEffectControllerError::new(RuntimeErrorCode::RuntimeStore, "io");
    assert_eq!(live.turn_failure_cause(), TurnFailureCause::LiveFault);
    assert_eq!(
        live.into_journaled().turn_failure_cause(),
        TurnFailureCause::Outcome
    );
}

/// FIG-3586: a lashlang replay refusal parks its turn. It is neither an
/// outcome — nothing about the turn failed, and a redeploy of the build that
/// wrote the journal serves it — nor a live fault a queued run may spend its
/// retry budget on, since every redrive by this build refuses again. FIG-3587
/// widens it to any recorded effect's replay hash conflict on the SQL hosts,
/// and an engine-journal divergence parks the same way.
#[test]
fn replay_refusals_park_the_turn() {
    use crate::runtime_error::TurnFailureCause;

    for code in [
        RuntimeErrorCode::LashlangCellReplayDivergence,
        RuntimeErrorCode::RetiredGeneration,
        RuntimeErrorCode::LashlangCellBindingDrift,
        RuntimeErrorCode::EffectReplayDivergence,
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
                    | RuntimeErrorCode::RetiredGeneration
                    | RuntimeErrorCode::LashlangCellBindingDrift
                    | RuntimeErrorCode::PluginRevisionUnavailable
                    | RuntimeErrorCode::EffectReplayDivergence
                    | RuntimeErrorCode::VmWorkerUnavailable
            ),
            "{code}: only deployment and replay refusals park"
        );
    }
}

/// Generation refusals keep their fields across journaling (FIG-4605): the
/// stored refusal carries both generations in its typed cause, and this
/// build decodes them back. Stored shapes change in place under the version
/// freeze, so a build from before the cause does not decode this record:
/// FIG-3619's older-decoder guarantee is retired until the 1.0 reset.
#[test]
fn a_stored_session_state_refusal_round_trips_with_its_generations() {
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
            serde_json::json!({
                "code": code.as_str(),
                "message": refused.message,
                "cause": {
                    "kind": "store_refusal",
                    "refusal": { "type": code.as_str(), "found": found, "current": current },
                },
            }),
            "a stored refusal keeps both generations typed"
        );
        assert!(
            refused.message.contains(&found.to_string())
                && refused.message.contains(&current.to_string()),
            "the stored message names both generations: {}",
            refused.message
        );

        let decoded: RuntimeError =
            serde_json::from_value(stored).expect("this build decodes the stored refusal");
        assert_eq!(decoded.code, code);
        assert_eq!(
            decoded.session_state_version_refusal(),
            Some(crate::runtime_error::SessionStateVersionRefusal { found, current })
        );
    }
}

fn terminal_store_causes() -> Vec<crate::RuntimeErrorCause> {
    use crate::compat::{CompatRefusal, VersionRange};
    use crate::store::StoreRefusal;
    let refusals = [
        StoreRefusal::WriterFenced {
            recorded: 2,
            writable: VersionRange::exactly(1),
        },
        StoreRefusal::Incompatible {
            refusal: CompatRefusal::Unstamped {
                component: "sqlite-core".into(),
                writing_release: None,
            },
        },
        StoreRefusal::StoreSessionMismatch {
            loaded: SessionId::from("other"),
            requested: SessionId::from("admission"),
        },
        StoreRefusal::SessionStateVersionUnsupported {
            found: 0,
            current: 1,
        },
        StoreRefusal::SessionStateVersionNewerThanRuntime {
            found: 2,
            current: 1,
        },
    ];
    refusals
        .into_iter()
        .map(|refusal| crate::RuntimeErrorCause::StoreRefusal {
            refusal: Box::new(refusal),
        })
        .chain([crate::RuntimeErrorCause::SessionDeleted {
            session_id: SessionId::from("admission"),
        }])
        .collect()
}

fn assert_terminal_derivation(fault: &crate::runtime_error::RuntimeEffectControllerError) {
    use crate::RuntimeEffectKind as Kind;
    use crate::runtime_error::EffectErrorJournalPolicy;
    assert!(fault.is_terminal());
    for kind in [
        Kind::BeforeLlmCall,
        Kind::AssistantResponseHooks,
        Kind::SyncExecutionEnvironment,
        Kind::LoadExecutionEnv,
        Kind::PresentToolResult,
        Kind::LanguageRuntimeValue,
        Kind::AdmitShift,
        Kind::RecoverFollowOn,
        Kind::ResolveTurnConfig,
        Kind::ResolveConfigTransaction,
        Kind::CloseRunScope,
        Kind::BeginSessionClose,
        Kind::IngestTriggerOccurrence,
        Kind::AdmitTriggerDelivery,
        Kind::Process,
        Kind::LlmCall,
        Kind::Direct,
        Kind::Sleep,
    ] {
        assert_eq!(
            fault.journal_disposition(kind),
            EffectErrorJournalPolicy::Terminal,
            "{} records its terminal cause: {fault:?}",
            kind.as_str()
        );
    }
    assert!(
        !fault.is_attempt_fault(),
        "a terminal cause is the recorded answer: {fault:?}"
    );
}

#[test]
fn terminal_causes_override_retry_authority_granted_before_the_cause() {
    for cause in terminal_store_causes() {
        for code in [
            RuntimeErrorCode::StoreCommitFailed,
            RuntimeErrorCode::TransientCancelWatch,
            RuntimeErrorCode::LlmProfileUnavailable,
        ] {
            let mut fault = crate::runtime_error::RuntimeEffectControllerError::new(
                code,
                "a refused derivation",
            )
            .retryable_uncommitted_derivation();
            fault.cause = Some(cause.clone());
            assert_terminal_derivation(&fault);
        }
    }
}

/// FIG-4404: a recorded model this worker cannot bind is the attempt's fault
/// on the two effects whose body binds it, and nowhere else; no other
/// failure of a model call gains the retry authority.
#[test]
fn an_unbound_llm_profile_is_the_attempts_fault_on_model_calls_alone() {
    use crate::runtime_error::{EffectErrorJournalPolicy, RuntimeEffectControllerError};
    let key = crate::LlmProfileKey::new("kimi-k3@tensorx");
    let unavailable = crate::provider::LlmProfileUnavailable::new(
        key.clone(),
        crate::provider::LlmProfileUnavailableReason::UnknownKey,
    );
    let fault = RuntimeEffectControllerError::llm_profile_unavailable(
        &key,
        format!("the recorded model cannot be bound on this worker: {unavailable}"),
    );
    assert_eq!(fault.code, RuntimeErrorCode::LlmProfileUnavailable);
    assert_eq!(fault.profile_key(), Some(&key));
    assert!(fault.is_attempt_fault());
    assert!(!fault.is_terminal(), "the fault is retried, never settled");
    for kind in [
        crate::RuntimeEffectKind::LlmCall,
        crate::RuntimeEffectKind::Direct,
    ] {
        assert_eq!(
            fault.journal_disposition(kind),
            EffectErrorJournalPolicy::RetryUncommittedResponseDerivation,
            "a pre-call bind fault is never the recorded result of {}",
            kind.as_str()
        );
        // Any other failure of the call is its recorded result, marked or not.
        let other = RuntimeEffectControllerError::new(
            RuntimeErrorCode::RuntimeEffectWrongOutcome,
            "a model call returned the wrong effect outcome",
        )
        .retryable_uncommitted_derivation();
        assert_eq!(
            other.journal_disposition(kind),
            EffectErrorJournalPolicy::Terminal
        );
    }
    assert_eq!(
        fault.journal_disposition(crate::RuntimeEffectKind::ToolAttempt),
        EffectErrorJournalPolicy::Terminal,
        "the code alone grants no retry authority outside the model calls"
    );

    // The typed key survives both conversions and the wire.
    let runtime = fault.clone().into_runtime_error();
    assert_eq!(runtime.profile_key(), Some(&key));
    assert_eq!(
        runtime.attempt_failure_text(),
        fault.attempt_failure_text(),
        "the fault's record rides the attempt's text from either error type"
    );
    assert!(runtime.is_retryable());
    assert!(!runtime.is_terminal());
    let json = serde_json::to_value(&runtime).expect("serialize runtime error");
    assert_eq!(
        json["cause"],
        serde_json::json!({"kind": "llm_profile_unavailable", "profile_key": "kimi-k3@tensorx"})
    );
    let decoded: RuntimeError = serde_json::from_value(json).expect("decode runtime error");
    assert_eq!(decoded.profile_key(), Some(&key));
    assert_eq!(
        RuntimeEffectControllerError::from(decoded).profile_key(),
        Some(&key)
    );
    // An engine keeps only a failed attempt's text. The fault's typed record
    // rides it, and the park of the exhausted retries decodes the key from
    // the record, whatever the engine wrote before it.
    let failure = fault.attempt_failure_text();
    assert!(failure.starts_with(&fault.to_string()));
    let park = crate::store::ParkReason::engine_retry_exhausted(
        8,
        Some("500".to_string()),
        format!("[500] Handler failed with retryable error: {failure}"),
    );
    assert_eq!(park.profile_key(), Some(&key));
    let stored = serde_json::to_value(&park).expect("serialize the park reason");
    assert_eq!(stored["profile_key"], "kimi-k3@tensorx");
    assert_eq!(
        serde_json::from_value::<crate::store::ParkReason>(stored).expect("decode the park"),
        park
    );
    // A failure that names the key only in prose carries no typed key.
    let prose = crate::store::ParkReason::engine_retry_exhausted(
        8,
        None,
        "model `kimi-k3@tensorx` is unavailable".to_string(),
    );
    assert_eq!(prose.profile_key(), None);
    assert!(
        serde_json::to_value(&prose)
            .expect("serialize the park reason")
            .get("profile_key")
            .is_none()
    );
    let unmarked = RuntimeEffectControllerError::new(RuntimeErrorCode::RuntimeStore, "store");
    assert_eq!(unmarked.attempt_failure_text(), unmarked.to_string());

    let plain = serde_json::to_value(RuntimeError::new(
        RuntimeErrorCode::LlmProfileUnavailable,
        "no key",
    ))
    .expect("serialize runtime error");
    assert!(plain.get("cause").is_none());
}

#[test]
fn terminal_codes_never_take_derivation_retry_authority() {
    for code in RuntimeErrorCode::ALL_FIRST_PARTY
        .iter()
        .filter(|code| code.is_terminal())
    {
        let fault = crate::runtime_error::RuntimeEffectControllerError::new(
            code.clone(),
            "a terminal refusal",
        )
        .retryable_uncommitted_derivation();
        assert_terminal_derivation(&fault);
    }
}

#[test]
fn stored_corruption_is_a_typed_terminal_admission_refusal() {
    let corrupt = || crate::StoreError::StoredDataCorrupt {
        record_kind: "TurnCancellationBinding",
        message: "invalid scope".into(),
    };
    let expected = serde_json::json!({
        "kind": "stored_data_corrupt", "record_kind": "TurnCancellationBinding", "message": "invalid scope",
    });
    for runtime in [
        crate::runtime_error::runtime_error_from_store_commit(corrupt()),
        crate::runtime_error::runtime_error_from_turn_input_admission(corrupt()),
        crate::runtime_error::RuntimeEffectControllerError::from(corrupt()).into_runtime_error(),
    ] {
        assert_eq!(runtime.code, RuntimeErrorCode::RuntimeStoreCorrupt);
        assert_eq!(serde_json::to_value(&runtime.cause).unwrap(), expected);
        let runtime: RuntimeError =
            serde_json::from_slice(&serde_json::to_vec(&runtime).unwrap()).unwrap();
        assert_terminal_derivation(
            &crate::runtime_error::RuntimeEffectControllerError::from(runtime)
                .retryable_uncommitted_derivation(),
        );
    }
}

/// FIG-4652: a refused run shape and a refused creation config carry their
/// cause typed, through the journal's encoding and the controller-to-runtime
/// hop, and every refusal is terminal.
mod typed_refusal_causes {
    use crate::config_transaction::{
        ConfigRefusal, ConfigRefusalReason, ConfigValueRole, RefusalSite,
    };
    use crate::run_spec::{DefinitionRef, RenderRefusal, RunDefinitionRefusal, RunShapeRefusal};
    use crate::runtime_error::{
        RuntimeEffectControllerError, RuntimeError, RuntimeErrorCause, RuntimeErrorCode,
    };

    /// A refusal as some raiser's own type.
    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum Raised {
        TooWide { width: u32 },
    }

    impl std::fmt::Display for Raised {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let Self::TooWide { width } = self;
            write!(formatter, "{width} is too wide")
        }
    }

    fn owner_refusal() -> ConfigRefusal {
        ConfigRefusal {
            owner: "protocol".to_string(),
            at: RefusalSite::Candidate,
            reason: ConfigRefusalReason::Unreadable {
                role: ConfigValueRole::RunOptions,
                message: "unknown field `prompt`".to_string(),
            },
        }
    }

    #[test]
    fn every_run_shape_refusal_is_a_typed_terminal_cause() {
        let reasoning = lash_core_llm::llm_profile::ReasoningRefused {
            key: crate::LlmProfileKey::new("model"),
            reasoning: crate::ReasoningSelection::Effort("deep".to_string()),
            category:
                lash_sansio::llm::capability::LlmProfileEffortValidationCategory::UnsupportedEffort,
            message: "deep is not declared".to_string(),
        };
        for (refusal, code) in [
            (
                RunShapeRefusal::Definition {
                    refusal: RunDefinitionRefusal::new(
                        DefinitionRef::new("review", 2),
                        &Raised::TooWide { width: 9 },
                    ),
                },
                RuntimeErrorCode::RunShapeRefused,
            ),
            (
                RunShapeRefusal::Owner {
                    refusal: owner_refusal(),
                },
                RuntimeErrorCode::RunShapeRefused,
            ),
            (
                RunShapeRefusal::Reasoning { refusal: reasoning },
                RuntimeErrorCode::ReasoningRefused,
            ),
            (
                RunShapeRefusal::ReasoningWithoutLlmProfile,
                RuntimeErrorCode::RunShapeRefused,
            ),
            (
                RunShapeRefusal::ProtocolOptionsWithoutProtocol,
                RuntimeErrorCode::RunShapeRefused,
            ),
            (
                RunShapeRefusal::Render {
                    refusal: RenderRefusal::new(&Raised::TooWide { width: 101 }),
                },
                RuntimeErrorCode::RunShapeRefused,
            ),
        ] {
            let error = RuntimeEffectControllerError::run_shape_refused(refusal.clone());
            assert_eq!(error.code, code, "{refusal:?}");
            assert!(error.is_terminal(), "{refusal:?}");
            assert_eq!(error.message, refusal.to_string());
            let journaled: RuntimeEffectControllerError =
                serde_json::from_value(serde_json::to_value(&error).expect("encodes"))
                    .expect("decodes");
            assert_eq!(journaled.run_shape_refusal(), Some(&refusal));
            let runtime = journaled.into_runtime_error();
            assert_eq!(runtime.code, code);
            assert!(runtime.is_terminal() && !runtime.is_retryable());
            assert_eq!(runtime.run_shape_refusal(), Some(&refusal));
            assert_eq!(runtime.config_refusal(), None);
        }
    }

    #[test]
    fn a_refused_creation_config_is_a_typed_terminal_cause() {
        let refusal = ConfigRefusal {
            at: RefusalSite::Creation,
            ..owner_refusal()
        };
        let session_id = crate::SessionId::from("child");
        let error = RuntimeError::session_config_refused(&session_id, refusal.clone());
        assert_eq!(error.code, RuntimeErrorCode::SessionConfigRefused);
        assert!(error.is_terminal() && !error.is_retryable());
        let decoded: RuntimeError =
            serde_json::from_value(serde_json::to_value(&error).expect("encodes"))
                .expect("decodes");
        assert_eq!(
            decoded.cause,
            Some(RuntimeErrorCause::ConfigRefused {
                refusal: Box::new(refusal.clone()),
            })
        );
        assert_eq!(decoded.config_refusal(), Some(&refusal));
        assert_eq!(decoded.run_shape_refusal(), None);
    }
}
