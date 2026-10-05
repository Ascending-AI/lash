mod settlement_order_journal_tests {
    use lash_core_execution::RuntimeEffectOutcome;

    /// A journaled tool-batch outcome is refused (FIG-3397): the batch command
    /// is gone, a batch is one round of its Run, and an entry recorded before
    /// that cutover must not decode into any current outcome.
    #[test]
    fn a_retired_tool_batch_outcome_does_not_decode() {
        let legacy = serde_json::json!({
            "type": "tool_batch",
            "launches": [],
            "settlement_order": [],
        });
        let error = serde_json::from_value::<RuntimeEffectOutcome>(legacy)
            .expect_err("a tool-batch outcome must not decode");
        assert!(
            error.to_string().contains("unknown variant `tool_batch`"),
            "the refusal must name the retired tag: {error}"
        );
    }

    /// FIG-2362: a journal entry written before the exec-code failure was typed
    /// journaled only the erased message string; it still decodes, under the
    /// honest `erased` reason.
    #[test]
    fn a_legacy_erased_exec_code_failure_still_decodes() {
        let legacy = serde_json::json!({
            "type": "exec_code",
            "result": { "Err": "code execution is not available in this session" },
        });
        let decoded = serde_json::from_value::<RuntimeEffectOutcome>(legacy)
            .expect("legacy erased exec-code failure decodes");
        let RuntimeEffectOutcome::ExecCode { result } = decoded else {
            panic!("decoded the wrong outcome kind");
        };
        let failure = result.expect_err("the journaled failure survives");
        assert_eq!(
            failure.reason,
            lash_core_execution::ExecCodeFailureReason::Erased
        );
        assert_eq!(
            failure.message,
            "code execution is not available in this session"
        );
    }
}

mod envelope_hash_tests {
    use lash_core_execution::runtime::effect::*;
    use lash_core_execution::{RuntimeEffectKind, SessionId, TurnId};

    fn invocation(kind: RuntimeEffectKind) -> RuntimeEffectInvocation {
        let _ = kind;
        RuntimeEffectInvocation::new(
            lash_core_execution::EffectAddress::new(
                lash_core_execution::ExecutionScope::turn("session", "turn"),
                "replay",
            )
            .expect("valid envelope address"),
            lash_core_execution::RuntimeAttribution::for_session("session"),
            "effect",
        )
    }

    fn await_event_key() -> lash_core_execution::AwaitEventKey {
        lash_core_execution::AwaitEventKey {
            scope: lash_core_execution::ExecutionScope::Turn {
                session_id: SessionId::from("session"),
                turn_id: TurnId::from("turn"),
            },
            wait: lash_core_execution::AwaitEventWaitIdentity::ToolCompletion {
                tool_call_id: lash_core_execution::ToolCallId::fixture("call"),
            },
            key_id: "key".to_string(),
            signature: "signature".to_string(),
        }
    }

    fn ungrouped_corpus() -> Vec<(&'static str, RuntimeEffectEnvelope)> {
        let prepared = lash_core_execution::PreparedToolCall {
            call_id: lash_core_execution::ToolCallId::fixture("call"),
            provider_call_id: None,
            tool_id: "tool:test".into(),
            tool_name: "test".into(),
            args: serde_json::json!({}),
            replay: None,
            prepared_payload: serde_json::Value::Null,
        };
        let commands: Vec<(&'static str, RuntimeEffectKind, RuntimeEffectCommand)> = vec![
            (
                "sleep",
                RuntimeEffectKind::Sleep,
                RuntimeEffectCommand::Sleep {
                    spec: lash_core_execution::SleepSpec::For { duration_ms: 1_000 },
                },
            ),
            (
                "exec_code",
                RuntimeEffectKind::ExecCode,
                RuntimeEffectCommand::ExecCode {
                    code: "1 + 1".to_string(),
                },
            ),
            (
                "sync_execution_environment",
                RuntimeEffectKind::SyncExecutionEnvironment,
                RuntimeEffectCommand::SyncExecutionEnvironment,
            ),
            (
                "language_runtime_value",
                RuntimeEffectKind::LanguageRuntimeValue,
                RuntimeEffectCommand::LanguageRuntimeValue {
                    operation: "read".to_string(),
                },
            ),
            (
                "tool_attempt",
                RuntimeEffectKind::ToolAttempt,
                RuntimeEffectCommand::ToolAttempt {
                    call: Box::new(prepared),
                    execution_grant: None,
                    attempt: 1,
                    max_attempts: 1,
                },
            ),
            (
                "checkpoint",
                RuntimeEffectKind::Checkpoint,
                RuntimeEffectCommand::Checkpoint {
                    checkpoint: lash_core_execution::CheckpointKind::AfterWork,
                },
            ),
            (
                "await_event",
                RuntimeEffectKind::AwaitEvent,
                RuntimeEffectCommand::AwaitEvent {
                    key: await_event_key(),
                },
            ),
            (
                "peek_await_event",
                RuntimeEffectKind::PeekAwaitEvent,
                RuntimeEffectCommand::PeekAwaitEvent {
                    key: await_event_key(),
                },
            ),
        ];
        commands
            .into_iter()
            .map(|(name, kind, command)| {
                (name, RuntimeEffectEnvelope::new(invocation(kind), command))
            })
            .collect()
    }

    #[test]
    fn ungrouped_envelope_v3_hash_golden_corpus() {
        let golden = [
            (
                "sleep",
                "95a5b578cf5737d9386728f0149c85973f8bc6e87deac069b7c82d1e2d1793ee",
            ),
            // Moved by 1ca3e8f42f (FIG-4020): `ExecCode` lost its `language`
            // field, as a module's identity is its IR rather than its front
            // end's dialect. Stored shapes change in place under the 1.0
            // version freeze, so the golden is re-pinned rather than the
            // hash domain bumped.
            (
                "exec_code",
                "fbf856d6a5e06ffba26aa333f7e84770920756b72c2a89900a0d234004cd86f7",
            ),
            // Moved by FIG-3587: the command lost `update_machine_config`, as
            // every sync now carries the environment. A journal holding the
            // old spelling conflicts at its first sync, and that conflict
            // parks the turn (the clean cutover), rather than replaying a
            // host-only protocol-start sync.
            (
                "sync_execution_environment",
                "3843a915e7f7d65518f41e770c343c686f106ccc2479027e0918e50928613cd5",
            ),
            (
                "language_runtime_value",
                "0c076b8310466a2a5a24fc49e1afef00612825435004387132d5dc2b85705469",
            ),
            (
                "tool_attempt",
                "467efd0bda5bdbf7700c4b581501c7f296ceb0184035eb69bab979c6606f450c",
            ),
            (
                "checkpoint",
                "d5d9bde834af9f145e121cd2af8fd6d6e630602ccc448c847b0f68d36f9c9768",
            ),
            (
                "await_event",
                "52750206854642766773ddcb1838e3582e5e6f086c7dcd289f5584bc56c9f11a",
            ),
            (
                "peek_await_event",
                "3f59b829efacf2f2a6d14f0410100630d691e752e92faa641cc09883c852dec2",
            ),
        ];
        let corpus = ungrouped_corpus();
        assert_eq!(
            corpus.len(),
            golden.len(),
            "every corpus entry needs a golden hash"
        );
        for ((name, envelope), (golden_name, golden_hash)) in corpus.iter().zip(golden) {
            assert_eq!(
                *name, golden_name,
                "corpus and golden list must stay aligned"
            );
            let hash = envelope.stable_hash().expect("envelope hashes");
            assert_eq!(
                hash, golden_hash,
                "the canonical encoding of an ungrouped `{name}` effect moved; \
                 every recorded envelope_hash on a live Postgres journal just \
                 became a ReplayMismatch"
            );
        }
    }
}
