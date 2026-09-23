mod effect_group_contract_tests {
    use lash_core_execution::RuntimeEffectKind;
    use lash_core_execution::SessionId;
    use lash_core_execution::TurnId;
    use lash_core_execution::runtime::effect::RuntimeEffectCommand;
    use lash_core_execution::runtime::effect::*;

    fn invocation(kind: RuntimeEffectKind) -> RuntimeEffectInvocation {
        let _ = kind;
        RuntimeEffectInvocation::new(
            lash_core_execution::EffectAddress::new(
                lash_core_execution::ExecutionScope::turn("session", "turn"),
                "replay",
            )
            .expect("valid group contract address"),
            lash_core_execution::RuntimeAttribution::for_session("session"),
            "effect",
        )
    }

    /// A child's invocation, keyed by its position.
    ///
    /// Siblings need distinct replay keys — one replay key is one journaled
    /// child — so a group's children cannot share the flat [`invocation`] key.
    fn child_invocation(_kind: RuntimeEffectKind, position: usize) -> RuntimeEffectInvocation {
        RuntimeEffectInvocation::new(
            lash_core_execution::EffectAddress::new(
                lash_core_execution::ExecutionScope::turn("session", "turn"),
                format!("replay-{position}"),
            )
            .expect("valid child address"),
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
                tool_call_id: "call".to_string(),
            },
            key_id: "key".to_string(),
            signature: "signature".to_string(),
        }
    }

    /// One envelope per cheaply-constructible non-group command variant.
    ///
    /// The corpus deliberately spans every command *shape* the encoding could
    /// perturb — unit-ish payloads, string payloads, keyed payloads, and the
    /// nested `ToolAttempt` payload — because the field being guarded sits on
    /// the envelope rather than inside any one command.
    ///
    /// Eight command variants are covered. The omissions are named rather than
    /// implied: `LlmCall`, `Direct` and `AssistantResponseHooks` need a full
    /// `LlmRequestSpec`/`LlmResponse`, `Trigger` a `TriggerCommand`, `Process` a
    /// `ProcessCommand`, and the group and acceptance commands (`ToolInvocation`,
    /// `IncorporateGroupSettlements`, `PresentToolResult`, `AcceptTurnInput`)
    /// their own retained records — payloads
    /// whose construction cost buys nothing here, because the field under guard
    /// sits on the *envelope*, so its omission is variant-independent and one
    /// covered variant already proves the encoding. The corpus width is a
    /// belt-and-braces pin on future edits, not the argument.
    fn ungrouped_corpus() -> Vec<(&'static str, RuntimeEffectEnvelope)> {
        let prepared = lash_core_execution::PreparedToolCall::from_parts(
            "call",
            "tool:test",
            "test",
            serde_json::json!({}),
            None,
            serde_json::Value::Null,
        );
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
                    language: "typescript".to_string(),
                    code: "1 + 1".to_string(),
                },
            ),
            (
                "sync_execution_environment",
                RuntimeEffectKind::SyncExecutionEnvironment,
                RuntimeEffectCommand::SyncExecutionEnvironment {
                    update_machine_config: true,
                },
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
                    call: prepared,
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

    /// Fixed-byte authority for the v3 admitted-address envelope hashes.
    ///
    /// FIG-2828 deliberately moved every hash from v2 and paired that cutover
    /// with new SQLite and PostgreSQL store generations. FIG-2968 made the same
    /// version-and-drain decision for a narrower change: the `sleep` command's
    /// payload became the canonical `SleepSpec`, which moves only that entry's
    /// bytes, and the effect-store generations were bumped so pre-cutover
    /// journals are refused rather than replayed against moved bytes. The hash
    /// domain tag is unchanged because no non-sleep envelope moved. The
    /// companion omission test still proves that an absent group does not
    /// perturb this corpus within the v3 format.
    #[test]
    fn ungrouped_envelope_v3_hash_golden_corpus() {
        let golden = [
            (
                "sleep",
                "95a5b578cf5737d9386728f0149c85973f8bc6e87deac069b7c82d1e2d1793ee",
            ),
            (
                "exec_code",
                "7f426da760b9b4e4fbcecbad269ddab57bfecbc805c80552f6aa29e2e27219fc",
            ),
            (
                "sync_execution_environment",
                "6364c8fedd3f1379cfde023fd703349d4939eedf0241cd1b11161b8afde87b44",
            ),
            (
                "language_runtime_value",
                "0c076b8310466a2a5a24fc49e1afef00612825435004387132d5dc2b85705469",
            ),
            (
                "tool_attempt",
                "14fe59d38589fe58cd66f4328251d301a8a556f886371c8544dcafe8b4cf867d",
            ),
            (
                "checkpoint",
                "d5d9bde834af9f145e121cd2af8fd6d6e630602ccc448c847b0f68d36f9c9768",
            ),
            (
                "await_event",
                "2ed3e1075946e128ed12a118c3dbee71f8478d76e1c204b59fd78d314b97fc97",
            ),
            (
                "peek_await_event",
                "9a0613831bc619f17b187c670ef4343829bd3b6d54c7190fc8e2944b2fc35e35",
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

    /// The structural reason the golden hashes above hold: the field is omitted
    /// entirely rather than encoded as `null`.
    ///
    /// Kept beside the golden test because it is the invariant a future editor
    /// would break — dropping `skip_serializing_if` still encodes *validly*, so
    /// only this assertion names the mistake.
    #[test]
    fn an_ungrouped_envelope_omits_the_group_field_entirely() {
        for (name, envelope) in ungrouped_corpus() {
            let json = lash_core_ids::stable_hash::stable_json_string(&envelope)
                .expect("envelope encodes");
            // Structural, not a substring search: a corpus payload that merely
            // *contained* the text "group" would otherwise read as a hash
            // regression.
            let decoded = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&json)
                .expect("an envelope encodes as a JSON object");
            assert!(
                !decoded.contains_key("group"),
                "an ungrouped `{name}` envelope must not encode a top-level group key at all: {json}"
            );
        }
    }

    /// A group child's membership folds into its hash, which is what makes
    /// "replay cannot silently change the wake rule" backed rather than
    /// asserted — it is the only mechanism available on engine tiers that keep
    /// no group row.
    #[test]
    fn group_membership_and_wake_drift_change_the_child_hash() {
        let base = RuntimeEffectEnvelope::new(
            invocation(RuntimeEffectKind::Sleep),
            RuntimeEffectCommand::Sleep {
                spec: lash_core_execution::SleepSpec::For { duration_ms: 1 },
            },
        );
        let ungrouped = base.stable_hash().expect("hashes");
        let first = base
            .clone()
            .in_effect_group(
                "scope:group:batch:0",
                0,
                GroupWakePolicy::First,
                LoserPolicy::RunToCompletion,
            )
            .stable_hash()
            .expect("hashes");
        let wake_drifted = base
            .clone()
            .in_effect_group(
                "scope:group:batch:0",
                0,
                GroupWakePolicy::All,
                LoserPolicy::RunToCompletion,
            )
            .stable_hash()
            .expect("hashes");
        let position_drifted = base
            .clone()
            .in_effect_group(
                "scope:group:batch:0",
                1,
                GroupWakePolicy::First,
                LoserPolicy::RunToCompletion,
            )
            .stable_hash()
            .expect("hashes");
        let group_drifted = base
            .in_effect_group(
                "scope:group:batch:1",
                0,
                GroupWakePolicy::First,
                LoserPolicy::RunToCompletion,
            )
            .stable_hash()
            .expect("hashes");

        let distinct = std::collections::HashSet::from([
            ungrouped.clone(),
            first.clone(),
            wake_drifted.clone(),
            position_drifted.clone(),
            group_drifted.clone(),
        ]);
        assert_eq!(
            distinct.len(),
            5,
            "membership, wake rule, position, and group key must each move the hash"
        );
        assert_ne!(
            first, wake_drifted,
            "a replay whose wake rule changed must be refused by the envelope-hash fence"
        );
    }

    /// `GroupWakePolicy` carries no serde default, mirroring
    /// `settlement_order`'s fail-closed discipline: a group child recorded
    /// without a wake rule is refused rather than replayed under a guessed one.
    #[test]
    fn a_group_membership_without_a_wake_rule_fails_closed() {
        let legacy = serde_json::json!({
            "group_key": "scope:group:batch:0",
            "position": 0,
        });
        let error = serde_json::from_value::<EffectGroupMembership>(legacy)
            .expect_err("a membership without a wake rule must not decode");
        assert!(
            error.to_string().contains("wake"),
            "the refusal must name the missing wake rule: {error}"
        );
    }

    #[test]
    fn group_membership_round_trips_through_the_envelope() {
        let envelope = RuntimeEffectEnvelope::new(
            invocation(RuntimeEffectKind::Sleep),
            RuntimeEffectCommand::Sleep {
                spec: lash_core_execution::SleepSpec::For { duration_ms: 1 },
            },
        )
        .in_effect_group(
            "scope:group:batch:2",
            3,
            GroupWakePolicy::FirstSuccess,
            LoserPolicy::Cancel,
        );
        let encoded = serde_json::to_string(&envelope).expect("envelope encodes");
        let decoded =
            serde_json::from_str::<RuntimeEffectEnvelope>(&encoded).expect("envelope decodes");
        let membership = decoded.group.expect("membership survives the round trip");
        assert_eq!(membership.group_key, "scope:group:batch:2");
        assert_eq!(membership.position, 3);
        assert_eq!(membership.wake, GroupWakePolicy::FirstSuccess);
    }

    /// The handle is durable continuation state, so its consumed cursor must
    /// survive encoding: a restored frame that lost it would re-consume rank 1
    /// and observe the winner twice.
    #[test]
    fn an_effect_group_handle_round_trips_its_consumed_cursor() {
        let handle = EffectGroupHandle::restored("scope:group:batch:0", 3, 2)
            .expect("a sane cursor restores");
        let encoded = serde_json::to_string(&handle).expect("handle encodes");
        let decoded = serde_json::from_str::<EffectGroupHandle>(&encoded).expect("handle decodes");
        assert_eq!(decoded, handle);
        assert_eq!(decoded.consumed(), 2);
        assert!(!decoded.is_exhausted());
    }

    /// A corrupt continuation must fail closed rather than resume as a finished
    /// group. Both malformed shapes below satisfy `is_exhausted()`, which is the
    /// one state indistinguishable from orderly completion, so an unvalidated
    /// restore would silently drop every settlement still owed.
    ///
    /// Asserted on `Deserialize` as well as the constructor, because a durable
    /// continuation arrives by decoding: a derived impl would admit exactly the
    /// handles `restored` refuses.
    #[test]
    fn a_corrupt_effect_group_handle_is_refused_rather_than_read_as_exhausted() {
        let cases = [
            ("cursor past the child count", 3usize, 4usize),
            ("a group with no children", 0, 0),
        ];
        for (name, children, consumed) in cases {
            let error = EffectGroupHandle::restored("scope:group:batch:0", children, consumed)
                .expect_err(&format!("{name} must not restore"));
            assert_eq!(
                error.code,
                lash_core_execution::RuntimeErrorCode::RuntimeEffectGroupShape,
                "{name} must be a typed group-shape refusal"
            );

            let encoded = serde_json::json!({
                "group_key": "scope:group:batch:0",
                "children": children,
                "consumed": consumed,
            });
            let decode_error = serde_json::from_value::<EffectGroupHandle>(encoded)
                .expect_err(&format!("{name} must not decode either"));
            assert!(
                decode_error.to_string().contains("scope:group:batch:0"),
                "the decode refusal must carry the constructor's reason: {decode_error}"
            );
        }
    }

    #[test]
    fn a_handle_reports_exhaustion_once_every_child_is_consumed() {
        let mut handle = EffectGroupHandle::new(&group_of(2));
        assert_eq!(handle.consumed(), 0, "a fresh handle has consumed nothing");
        assert!(!handle.is_exhausted());
        handle.advance().expect("rank 1 of 2");
        assert!(!handle.is_exhausted());
        handle.advance().expect("rank 2 of 2");
        assert!(
            handle.is_exhausted(),
            "exhaustion is the caller's arithmetic, knowable without a round trip"
        );
    }

    /// The write side of the same fence
    /// [`a_corrupt_effect_group_handle_is_refused_rather_than_read_as_exhausted`]
    /// holds on decode. Validating only the read side left both corrupt shapes
    /// *mintable*: a host could construct them, serialize them into a
    /// continuation, and only discover the problem when that continuation failed
    /// to load — surfacing an arithmetic slip as an unresumable session, far from
    /// the slip. Refused rather than clamped, because clamping is what hides the
    /// lost settlements.
    #[test]
    fn a_handle_cannot_mint_the_corrupt_states_decode_refuses() {
        // Probe 1 — a zero-child handle. Unrepresentable rather than refused:
        // `new` derives the count from the group, and `try_new` refuses an empty
        // group, so there is no argument that produces one.
        RuntimeEffectGroup::try_new(
            invocation(RuntimeEffectKind::Sleep),
            "scope:group:batch:0",
            Vec::new(),
            GroupWakePolicy::First,
            LoserPolicy::RunToCompletion,
        )
        .expect_err("an empty group is the only source of a zero-child handle");
        let single = EffectGroupHandle::new(&group_of(1));
        assert_eq!(
            single.children(),
            1,
            "the child count comes from the group, so it cannot be zero or wrong"
        );

        // Probe 2 — a cursor past the child count, previously reachable by
        // advancing twice on a one-child group.
        let mut handle = EffectGroupHandle::new(&group_of(1));
        handle.advance().expect("rank 1 of 1");
        let error = handle
            .advance()
            .expect_err("advancing past the last child must be refused");
        assert_eq!(
            error.code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectGroupShape
        );
        assert!(
            error.message.contains("scope:group:batch:0"),
            "the refusal must name the group: {error}"
        );
        assert_eq!(
            handle.consumed(),
            1,
            "a refused advance must leave the cursor where it was, not clamp past it"
        );

        // The legitimate exhausted state — children == consumed > 0 — is still a
        // first-class shape on both sides of the seam.
        assert!(handle.is_exhausted());
        let encoded = serde_json::to_string(&handle).expect("an exhausted handle encodes");
        let decoded =
            serde_json::from_str::<EffectGroupHandle>(&encoded).expect("and decodes again");
        assert_eq!(decoded, handle);
        assert!(decoded.is_exhausted());
    }

    fn child(position: usize, group_key: &str, wake: GroupWakePolicy) -> RuntimeEffectEnvelope {
        RuntimeEffectEnvelope::new(
            child_invocation(RuntimeEffectKind::Sleep, position),
            RuntimeEffectCommand::Sleep {
                spec: lash_core_execution::SleepSpec::For {
                    duration_ms: position as u64 + 1,
                },
            },
        )
        .in_effect_group(group_key, position, wake, LoserPolicy::RunToCompletion)
    }

    fn group_of(children: usize) -> RuntimeEffectGroup {
        RuntimeEffectGroup::try_new(
            invocation(RuntimeEffectKind::Sleep),
            "scope:group:batch:0",
            (0..children).map(unstamped_child).collect(),
            GroupWakePolicy::First,
            LoserPolicy::RunToCompletion,
        )
        .expect("a non-empty group assembles")
    }

    fn unstamped_child(position: usize) -> RuntimeEffectEnvelope {
        RuntimeEffectEnvelope::new(
            child_invocation(RuntimeEffectKind::Sleep, position),
            RuntimeEffectCommand::Sleep {
                spec: lash_core_execution::SleepSpec::For {
                    duration_ms: position as u64 + 1,
                },
            },
        )
    }

    /// The group is the one place the key, wake rule, and positions are made to
    /// agree. Every durability claim in ADR 0065 reduces to that agreement, and
    /// before `try_new` existed every disagreement below was representable,
    /// silent, and diagnosable only as a `ReplayMismatch` in production.
    #[test]
    fn assembling_a_group_stamps_unstamped_children_from_their_own_index() {
        let group = RuntimeEffectGroup::try_new(
            invocation(RuntimeEffectKind::Sleep),
            "scope:group:batch:0",
            vec![unstamped_child(0), unstamped_child(1)],
            GroupWakePolicy::First,
            LoserPolicy::RunToCompletion,
        )
        .expect("a group of unstamped children assembles");
        assert_eq!(group.group_key(), "scope:group:batch:0");
        assert_eq!(group.wake(), GroupWakePolicy::First);
        for (index, child) in group.children().iter().enumerate() {
            let membership = child.group.as_deref().expect("every child is stamped");
            assert_eq!(membership.group_key, "scope:group:batch:0");
            assert_eq!(membership.position, index);
            assert_eq!(membership.wake, GroupWakePolicy::First);
        }
    }

    #[test]
    fn assembling_a_group_refuses_children_that_disagree_with_it() {
        let key = "scope:group:batch:0";
        let cases: Vec<(&str, Vec<RuntimeEffectEnvelope>)> = vec![
            ("empty", vec![]),
            (
                "foreign key",
                vec![
                    child(0, key, GroupWakePolicy::First),
                    child(1, "scope:group:batch:1", GroupWakePolicy::First),
                ],
            ),
            (
                "permuted position",
                vec![
                    child(1, key, GroupWakePolicy::First),
                    child(0, key, GroupWakePolicy::First),
                ],
            ),
            (
                "drifted wake",
                vec![
                    child(0, key, GroupWakePolicy::First),
                    child(1, key, GroupWakePolicy::All),
                ],
            ),
        ];
        for (name, children) in cases {
            let error = RuntimeEffectGroup::try_new(
                invocation(RuntimeEffectKind::Sleep),
                key,
                children,
                GroupWakePolicy::First,
                LoserPolicy::RunToCompletion,
            )
            .expect_err(&format!("a group with a {name} child must not assemble"));
            assert_eq!(
                error.code,
                lash_core_execution::RuntimeErrorCode::RuntimeEffectGroupShape,
                "a {name} disagreement must be a typed group-shape refusal"
            );
        }
    }

    #[test]
    fn assembling_a_group_refuses_an_empty_group_key() {
        let error = RuntimeEffectGroup::try_new(
            invocation(RuntimeEffectKind::Sleep),
            "   ",
            vec![unstamped_child(0)],
            GroupWakePolicy::First,
            LoserPolicy::RunToCompletion,
        )
        .expect_err("a group without an identity must not assemble");
        assert_eq!(
            error.code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectGroupShape
        );
    }

    /// Close may narrow the declared disposition but never widen it. Widening
    /// would make the losers' fate depend on whether the caller reached its close
    /// at all — a crash-drain applies the *declared* disposition — which is the
    /// divergence that declaring at open exists to remove.
    #[test]
    fn a_close_may_narrow_the_declared_loser_disposition_but_not_widen_it() {
        use LoserPolicy::{Cancel, RunToCompletion};
        assert_eq!(
            LoserPolicy::resolve_close(RunToCompletion, RunToCompletion).expect("same"),
            RunToCompletion
        );
        assert_eq!(
            LoserPolicy::resolve_close(Cancel, Cancel).expect("same"),
            Cancel
        );
        assert_eq!(
            LoserPolicy::resolve_close(RunToCompletion, Cancel)
                .expect("narrowing a declared RunToCompletion to Cancel is allowed"),
            Cancel
        );
        let error = LoserPolicy::resolve_close(Cancel, RunToCompletion)
            .expect_err("widening a declared Cancel must be refused");
        assert_eq!(
            error.code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectGroupShape
        );
    }

    /// The declared disposition is journaled with the group *and* folded into
    /// every child's hash, so a replay under a drifted disposition is refused on
    /// engine tiers that keep no group row too.
    #[test]
    fn a_drifted_loser_disposition_changes_the_child_hash_and_fails_assembly() {
        let key = "scope:group:batch:0";
        let run = RuntimeEffectEnvelope::new(
            invocation(RuntimeEffectKind::Sleep),
            RuntimeEffectCommand::Sleep {
                spec: lash_core_execution::SleepSpec::For { duration_ms: 1 },
            },
        )
        .in_effect_group(key, 0, GroupWakePolicy::First, LoserPolicy::RunToCompletion);
        let cancel = RuntimeEffectEnvelope::new(
            invocation(RuntimeEffectKind::Sleep),
            RuntimeEffectCommand::Sleep {
                spec: lash_core_execution::SleepSpec::For { duration_ms: 1 },
            },
        )
        .in_effect_group(key, 0, GroupWakePolicy::First, LoserPolicy::Cancel);
        assert_ne!(
            run.stable_hash().expect("hashes"),
            cancel.stable_hash().expect("hashes"),
            "loser disposition must move the child hash, or a drifted replay is \
             unfenced wherever no group row exists"
        );

        let error = RuntimeEffectGroup::try_new(
            invocation(RuntimeEffectKind::Sleep),
            key,
            vec![cancel],
            GroupWakePolicy::First,
            LoserPolicy::RunToCompletion,
        )
        .expect_err("a child whose disposition disagrees must not assemble");
        assert_eq!(
            error.code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectGroupShape
        );
    }

    /// A grouped child on a path with no membership slot must be refused, never
    /// silently stripped: those paths record no canonical envelope, so the wake
    /// rule loses the only fence that makes "replay cannot silently change it"
    /// backed rather than asserted.
    #[test]
    fn an_unhonored_group_membership_is_refused_rather_than_dropped() {
        assert!(
            refuse_unhonored_group_membership(None, "trigger").is_ok(),
            "an ungrouped effect passes every shape unchanged"
        );
        let membership = EffectGroupMembership {
            group_key: "scope:group:batch:0".to_string(),
            position: 1,
            wake: GroupWakePolicy::First,
            loser_disposition: LoserPolicy::RunToCompletion,
        };
        let error = refuse_unhonored_group_membership(Some(&membership), "trigger")
            .expect_err("a grouped child on a membership-less path must be refused");
        assert_eq!(
            error.code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectGroupShape
        );
        assert!(
            error.message.contains("trigger") && error.message.contains("scope:group:batch:0"),
            "the refusal must name the path and the group: {error}"
        );
    }
}

/// Retention of a closed group's record on the native tier (FIG-3548).
///
/// These laws are part of `effect_model`, which also runs in the zero-feature
/// lane (`effect_model__test__fv_ecbe9667`): the retention they pin is
/// production behavior, not a `testing` convenience.
mod native_group_retention {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use lash_core_execution::core_internal::RuntimeEffectLocalRunner;
    use lash_core_execution::runtime::effect::*;
    use lash_core_execution::runtime::{NativeEffectHost, NativeRuntimeEffectController};
    use lash_core_execution::{CancellationToken, ExecutionScope};

    /// Runs every child as a counted no-op sleep, so a law can tell a served
    /// record (the count holds) from a re-dispatch (it moves).
    struct CountingExecutors {
        runs: Arc<AtomicUsize>,
    }

    struct CountingRunner {
        runs: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl RuntimeEffectLocalRunner for CountingRunner {
        async fn execute(
            self: Box<Self>,
            _envelope: RuntimeEffectEnvelope,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(RuntimeEffectOutcome::Sleep)
        }
    }

    impl GroupExecutors for CountingExecutors {
        fn executor_for(
            &self,
            _envelope: &RuntimeEffectEnvelope,
        ) -> Option<RuntimeEffectLocalExecutor<'static>> {
            Some(RuntimeEffectLocalExecutor::owned_runner(
                Box::new(CountingRunner {
                    runs: Arc::clone(&self.runs),
                }),
                None,
            ))
        }
    }

    struct World {
        #[cfg(feature = "testing")]
        controller: Arc<NativeRuntimeEffectController>,
        host: NativeEffectHost,
        runs: Arc<AtomicUsize>,
    }

    fn world() -> World {
        let controller = Arc::new(NativeRuntimeEffectController::default());
        let runs = Arc::new(AtomicUsize::new(0));
        controller
            .register_group_executors(Arc::new(CountingExecutors {
                runs: Arc::clone(&runs),
            }))
            .expect("the counting resolver registers");
        let host = NativeEffectHost::with_native_controller(Arc::clone(&controller));
        World {
            #[cfg(feature = "testing")]
            controller,
            host,
            runs,
        }
    }

    fn group(scope: &ExecutionScope, key: &str) -> RuntimeEffectGroup {
        let address = |replay_key: String| {
            lash_core_execution::EffectAddress::new(scope.clone(), replay_key)
                .expect("valid address")
        };
        RuntimeEffectGroup::try_new(
            RuntimeEffectInvocation::new(
                address(format!("{key}:group")),
                lash_core_execution::RuntimeAttribution::none(),
                "group",
            ),
            key,
            vec![RuntimeEffectEnvelope::new(
                RuntimeEffectInvocation::new(
                    address(format!("{key}:child:0")),
                    lash_core_execution::RuntimeAttribution::none(),
                    "child",
                ),
                RuntimeEffectCommand::Sleep {
                    spec: lash_core_execution::SleepSpec::For { duration_ms: 1 },
                },
            )],
            lash_core_execution::GroupWakePolicy::All,
            lash_core_execution::LoserPolicy::RunToCompletion,
        )
        .expect("a one-child group assembles")
    }

    /// Opens `group`, takes its rank-0 settlement and closes it.
    async fn settle_and_close(
        host: &NativeEffectHost,
        admitted: &lash_core_execution::AdmittedScope,
        group: RuntimeEffectGroup,
    ) -> GroupSettlement {
        let scoped = host.scoped(admitted.clone()).expect("the scope binds");
        let mut handle = scoped
            .controller()
            .open_effect_group(group)
            .await
            .expect("the group opens");
        let settlement = scoped
            .controller()
            .await_next_settlement(&mut handle, CancellationToken::new())
            .await
            .expect("rank 0 settles");
        scoped
            .controller()
            .close_effect_group(handle, lash_core_execution::LoserPolicy::RunToCompletion)
            .await
            .expect("the group closes");
        settlement
    }

    /// Waits until the spawned finalizer has reaped `key` out of the open
    /// table — the moment a reopen used to fall through to a fresh dispatch.
    async fn until_reaped(host: &NativeEffectHost, key: &str) {
        let closing = host
            .effect_group_closing()
            .expect("the native host exposes its group lifecycle");
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while closing
                .read_group_lifecycle(key)
                .await
                .expect("the lifecycle reads")
                .is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the finalizer reaps the closed group");
    }

    /// A reopen after the finalizer has reaped the closed group serves the
    /// recorded settlement; the child does not run again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reopen_after_the_reap_serves_the_recorded_settlement() {
        let world = world();
        let admitted = lash_core_execution::AdmittedScope::turn("retention", "turn");
        let group = group(admitted.scope(), "retention-reaped");

        let first = settle_and_close(&world.host, &admitted, group.clone()).await;
        assert_eq!(world.runs.load(Ordering::SeqCst), 1, "the child ran once");
        until_reaped(&world.host, "retention-reaped").await;

        let replayed = settle_and_close(&world.host, &admitted, group).await;
        assert_eq!(replayed.sequence, first.sequence);
        assert!(matches!(replayed.outcome, Ok(RuntimeEffectOutcome::Sleep)));
        assert_eq!(
            world.runs.load(Ordering::SeqCst),
            1,
            "the reopen served the retained record; the child did not re-run"
        );
    }

    /// A rank read after the finalizer has reaped the closed group is served
    /// from the retained record, as the journaled tiers serve it from the
    /// journal: a post-close read must not depend on whether the spawned
    /// finalizer has reaped yet (FIG-3567).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rank_read_after_the_reap_serves_the_recorded_settlement() {
        let world = world();
        let admitted = lash_core_execution::AdmittedScope::turn("rank-read", "turn");
        let group = group(admitted.scope(), "retention-rank-read");

        let first = settle_and_close(&world.host, &admitted, group).await;
        until_reaped(&world.host, "retention-rank-read").await;

        let scoped = world.host.scoped(admitted).expect("the scope binds");
        let read = scoped
            .controller()
            .read_group_settlement("retention-rank-read", 1)
            .await
            .expect("a reaped group's rank read is answered")
            .expect("rank 1 is recorded");
        assert_eq!(read.sequence, first.sequence);
        assert_eq!(read.child_replay_key, "retention-rank-read:child:0");
        assert!(matches!(read.outcome, Ok(RuntimeEffectOutcome::Sleep)));
        assert!(
            scoped
                .controller()
                .read_group_settlement("retention-rank-read", 2)
                .await
                .expect("a rank past the record is answered")
                .is_none(),
            "no rank is invented past the record"
        );
    }

    /// Retiring the owning session evicts the retained record: the next
    /// open of the same key is a fresh group, and its child runs again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_retirement_evicts_the_retained_record() {
        let world = world();
        let admitted = lash_core_execution::AdmittedScope::turn("evicted", "turn");
        let group = group(admitted.scope(), "retention-evicted");

        settle_and_close(&world.host, &admitted, group.clone()).await;
        until_reaped(&world.host, "retention-evicted").await;
        world
            .host
            .retire_effect_journal(lash_core_execution::EffectJournalRetirement::session(
                "evicted",
            ))
            .await
            .expect("the session retires");

        settle_and_close(&world.host, &admitted, group).await;
        assert_eq!(
            world.runs.load(Ordering::SeqCst),
            2,
            "the evicted record is gone, so the reopen dispatched fresh"
        );
    }

    /// A scope-exact retirement evicts every record retained under that
    /// scope, whether the finalizer has reaped it yet or not — and nothing
    /// under another scope.
    #[cfg(feature = "testing")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scope_retirement_evicts_exactly_that_scopes_records() {
        let world = world();
        let retired = lash_core_execution::AdmittedScope::runtime_operation("retired-op");
        let kept = lash_core_execution::AdmittedScope::runtime_operation("kept-op");

        settle_and_close(&world.host, &retired, group(retired.scope(), "op-a")).await;
        settle_and_close(&world.host, &kept, group(kept.scope(), "op-b")).await;
        until_reaped(&world.host, "op-a").await;
        until_reaped(&world.host, "op-b").await;
        assert_eq!(world.controller.retained_group_count(), 2);

        world
            .host
            .retire_effect_journal(
                lash_core_execution::EffectJournalRetirement::runtime_operation("retired-op"),
            )
            .await
            .expect("the operation retires");
        assert_eq!(
            world.controller.retained_group_count(),
            1,
            "only the retired scope's record is evicted"
        );
    }
}
