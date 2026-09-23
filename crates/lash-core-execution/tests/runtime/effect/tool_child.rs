mod tests {
    use lash_core_execution::runtime::effect::*;
    use lash_core_execution::tool_dispatch::ToolAttemptEffectIdentity;
    use lash_core_execution::{
        FrameNodeId, PreparedToolCall, ProcessExecutionEnvRef, ProcessRef, SessionId,
        ToolExecutionGrant, ToolManifest, ToolRetryPolicy,
    };
    use lash_core_execution::{ToolDefinition, ToolId};

    fn definition(id: &str) -> ToolDefinition {
        ToolDefinition::raw(
            id,
            id,
            "search the web",
            serde_json::json!({ "type": "object" }),
            serde_json::json!({ "type": "object" }),
        )
    }

    fn manifest(id: &str) -> ToolManifest {
        definition(id).manifest()
    }

    fn frame() -> FrameNodeId {
        FrameNodeId::new("frame-1").expect("a valid frame id")
    }

    fn call(tool_id: &str) -> PreparedToolCall {
        PreparedToolCall::from_parts(
            "call-1",
            ToolId::from(tool_id),
            "search",
            serde_json::json!({ "q": "lash" }),
            None,
            serde_json::Value::Null,
        )
    }

    fn scope() -> ToolChildScope {
        ToolChildScope {
            opener: EffectOpener::turn("session", "turn"),
            admitted_scope: AdmittedScope::turn("session", "turn"),
            session_id: SessionId::from("session"),
            agent_frame_id: frame(),
        }
    }

    fn process_ref(name: &str, incarnation: u64) -> ProcessRef {
        ProcessRef::new(
            name,
            lash_core_execution::ProcessIncarnation::from_registration_sequence(incarnation),
        )
    }

    fn env() -> ProcessExecutionEnvRef {
        ProcessExecutionEnvRef::new("env-ref")
    }

    fn request() -> ToolChildRequest {
        let tool = manifest("search");
        ToolChildRequest::new(
            call(tool.id.as_str()),
            ToolChildAdmission::Catalog {
                manifest: Box::new(tool),
            },
            ToolAttemptEffectIdentity::Scalar { parent: None },
            scope(),
            env(),
            ToolChildCompletionRouting::Durable,
        )
    }

    /// Every field survives the durable round trip. A request that lost a field
    /// in serialization is a child recovered under partial authority, which is
    /// the exact failure §3 retains input to prevent.
    #[test]
    fn a_request_round_trips_every_field_through_its_durable_bytes() {
        let mut original = request().with_cancellation_authority(
            TurnControlBindingId::new("binding-7").expect("a valid binding id"),
        );
        // A consistent process-opener request: the enclosing incarnation is
        // the opener's own, which is the only pair `validate` admits.
        original.scope.opener = EffectOpener::process(process_ref("process-9", 3));
        original = original.with_enclosing_process(process_ref("process-9", 3));
        let json = serde_json::to_string(&original).expect("a request serializes");
        let decoded: ToolChildRequest = serde_json::from_str(&json).expect("a request decodes");
        assert_eq!(decoded, original);
        assert_eq!(decoded.version, TOOL_CHILD_REQUEST_VERSION);
        assert_eq!(
            decoded
                .cancellation_authority
                .as_ref()
                .map(TurnControlBindingId::as_str),
            Some("binding-7")
        );
        assert_eq!(decoded.execution_env.as_str(), "env-ref");
        assert_eq!(
            decoded.enclosing_process,
            Some(process_ref("process-9", 3)),
            "the enclosing process must survive as an incarnation, not a bare name"
        );
        assert_eq!(decoded.scope.agent_frame_id.as_str(), "frame-1");
    }

    /// A field this build does not know is refused, not dropped. A retired field
    /// that vanished into a default would silently narrow the authority a
    /// recovered child runs under (prelude, FIG-2886 review).
    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        let mut value = serde_json::to_value(request()).expect("a request serializes");
        value
            .as_object_mut()
            .expect("a request is a JSON object")
            .insert("fabricated".to_string(), serde_json::Value::Bool(true));
        let error = serde_json::from_value::<ToolChildRequest>(value)
            .expect_err("an unknown field must be refused");
        assert!(
            error.to_string().contains("fabricated"),
            "the refusal must name the field it refused, got {error}"
        );
    }

    /// A version no build wrote is refused rather than read under this build's
    /// field meanings.
    #[test]
    fn a_foreign_format_version_is_refused_rather_than_defaulted() {
        let mut foreign = request();
        foreign.version = TOOL_CHILD_REQUEST_VERSION + 1;
        let error = foreign
            .validate()
            .expect_err("a foreign version is refused");
        assert_eq!(
            error.code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectToolChildRequestVersion
        );
    }

    /// A request journaled by an older build is refused, typed and before any
    /// effect. Version 6 moved a parked child's §4 commit ahead of its
    /// presentation (FIG-3609), so a child whose journal an older build wrote
    /// would replay against a different step order. Version 7 keyed a
    /// lashlang command's attempts by issue ordinal (FIG-3586), so version 6
    /// is refused too.
    #[test]
    fn a_request_journaled_by_an_older_build_is_refused() {
        for version in 1..TOOL_CHILD_REQUEST_VERSION {
            let mut older = request();
            older.version = version;
            let error = older
                .validate()
                .expect_err("an older-build request is refused");
            assert_eq!(
                error.code,
                lash_core_execution::RuntimeErrorCode::RuntimeEffectToolChildRequestVersion,
                "version {version} is refused with the typed version code"
            );
        }
    }

    #[test]
    fn an_empty_call_id_is_refused() {
        let mut blank = request();
        blank.call.call_id = "   ".to_string();
        assert_eq!(
            blank
                .validate()
                .expect_err("a blank call id is refused")
                .code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectToolChildRequestCallId
        );
    }

    /// The admitted authority and the call it authorizes are one fact. A request
    /// pinning tool A's manifest against a call to tool B would let a recovered
    /// child run tool B under tool A's retry policy and argument projection.
    #[test]
    fn an_admission_for_another_tool_is_refused() {
        let mut crossed = request();
        crossed.call.tool_id = ToolId::from("other-tool");
        assert_eq!(
            crossed
                .validate()
                .expect_err("a crossed admission is refused")
                .code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectToolChildRequestAdmission
        );
    }

    /// The pinned manifest is the whole catalog dependency: the retry policy a
    /// reopen uses comes from the record, so a catalog edited after admission
    /// cannot change how a recovered child retries (ADR 0099 §3).
    #[test]
    fn the_retry_policy_is_read_from_the_pinned_admission_not_a_live_catalog() {
        let mut tool = manifest("search");
        tool.retry_policy = ToolRetryPolicy::safe(4, 10, 100);
        let pinned = ToolChildRequest::new(
            call(tool.id.as_str()),
            ToolChildAdmission::Catalog {
                manifest: Box::new(tool),
            },
            ToolAttemptEffectIdentity::Scalar { parent: None },
            scope(),
            env(),
            ToolChildCompletionRouting::Inline,
        );
        let decoded: ToolChildRequest =
            serde_json::from_str(&serde_json::to_string(&pinned).expect("serializes"))
                .expect("decodes");
        assert_eq!(decoded.retry_policy(), ToolRetryPolicy::safe(4, 10, 100));
    }

    /// A granted call carries its own manifest and contract, so it needs no
    /// pinned catalog entry beside it — the two arms are alternatives, and the
    /// grant arm answers `manifest()` from the grant.
    #[test]
    fn a_granted_admission_answers_from_its_own_grant() {
        let admission = ToolChildAdmission::Granted {
            grant: Box::new(ToolExecutionGrant::from_definition(definition("search"))),
        };
        assert_eq!(admission.manifest().id, ToolId::from("search"));
        assert!(admission.grant().is_some());
        assert!(
            ToolChildAdmission::Catalog {
                manifest: Box::new(manifest("search"))
            }
            .grant()
            .is_none()
        );
    }

    /// The opener and the admitted scope are two facts, not one. A process
    /// opener's child is claimed under its own scope, and collapsing them would
    /// make a recovered child validate its opener against the wrong identity.
    #[test]
    fn the_opener_and_the_admitted_scope_are_retained_separately() {
        let mut request = request();
        request.scope.opener = EffectOpener::process(process_ref("process-1", 4));
        request.scope.admitted_scope = AdmittedScope::runtime_operation("op-1");
        request.enclosing_process = Some(process_ref("process-1", 4));
        let decoded: ToolChildRequest =
            serde_json::from_str(&serde_json::to_string(&request).expect("serializes"))
                .expect("decodes");
        assert_eq!(
            decoded.scope.opener,
            EffectOpener::process(process_ref("process-1", 4))
        );
        assert_eq!(
            decoded.scope.admitted_scope,
            AdmittedScope::runtime_operation("op-1")
        );
        decoded
            .validate()
            .expect("a process opener needs no session match");
    }

    /// The defect ADR 0099 §1 names, at the shape level: a process re-registered
    /// under the same name is a different opener, and a retained request must
    /// not compare equal to one admitted under its predecessor. An
    /// `ExecutionScope::Process` could not express this at all.
    #[test]
    fn a_reregistered_process_opener_is_not_the_opener_that_was_admitted() {
        let mut admitted = request();
        admitted.scope.opener = EffectOpener::process(process_ref("indexer", 1));
        let mut successor = request();
        successor.scope.opener = EffectOpener::process(process_ref("indexer", 2));
        assert_ne!(admitted.scope.opener, successor.scope.opener);

        let decoded: ToolChildRequest =
            serde_json::from_str(&serde_json::to_string(&admitted).expect("serializes"))
                .expect("decodes");
        assert_eq!(
            decoded
                .scope
                .opener
                .process_ref()
                .map(|r| r.incarnation.registration_sequence()),
            Some(1),
            "the admitted incarnation is what recovery validates against"
        );
    }

    /// A turn opener names its session, so a request attributing its work to a
    /// different one is representable and invalid. Refused at the boundary.
    #[test]
    fn a_turn_opener_disagreeing_with_its_session_is_refused() {
        let mut crossed = request();
        crossed.scope.opener = EffectOpener::turn("other-session", "turn");
        assert_eq!(
            crossed
                .validate()
                .expect_err("a crossed opener session is refused")
                .code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener
        );
    }

    /// The claim pair is checked at decode, not trusted: a journal row that
    /// pairs a process scope with no incarnation — or with another process's —
    /// does not decode, because `AdmittedScope::new` is the only construction
    /// and it refuses the half-admitted shape.
    #[test]
    fn a_half_admitted_claim_pair_does_not_decode() {
        let wire = |process: serde_json::Value| {
            let mut value = serde_json::to_value(request()).expect("a request serializes");
            *value
                .pointer_mut("/scope/admitted_scope")
                .expect("the wire pair is a nested object") = serde_json::json!({
                "scope": { "type": "process", "process_id": "worker" },
                "process": process,
            });
            value
        };
        assert!(
            serde_json::from_value::<ToolChildRequest>(wire(serde_json::Value::Null)).is_err(),
            "a process claim with no incarnation is the half-admitted shape the pair exists to refuse"
        );
        let mismatched =
            serde_json::to_value(process_ref("other-worker", 2)).expect("a process ref serializes");
        assert!(
            serde_json::from_value::<ToolChildRequest>(wire(mismatched)).is_err(),
            "a pin naming another process is refused at decode, not trusted"
        );
    }

    /// A process opener's enclosing incarnation is the opener's own — the one
    /// fact stated twice. A request that pairs `process(P)#7` with enclosing
    /// `process(P)#9`, or with no enclosing at all, is refused at the boundary
    /// rather than run under a successor's context.
    #[test]
    fn a_process_opener_must_enclose_its_own_incarnation() {
        let mut request = request();
        request.scope.opener = EffectOpener::process(process_ref("worker", 7));
        request.scope.admitted_scope = AdmittedScope::process(process_ref("worker", 7));
        request.enclosing_process = Some(process_ref("worker", 9));
        assert_eq!(
            request
                .validate()
                .expect_err("enclosing a different incarnation is refused")
                .code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener
        );
        request.enclosing_process = None;
        assert_eq!(
            request
                .validate()
                .expect_err("a process opener with no enclosing process is refused")
                .code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener
        );
        request.enclosing_process = Some(process_ref("worker", 7));
        request
            .validate()
            .expect("the opener's own incarnation is the one legal enclosing");
    }

    /// Symmetrically: a non-process opener encloses no process, so a retained
    /// `enclosing_process` on a turn or drain opener is a refused
    /// inconsistency rather than a stray field.
    #[test]
    fn a_non_process_opener_records_no_enclosing_process() {
        let mut request = request();
        request.enclosing_process = Some(process_ref("worker", 1));
        assert_eq!(
            request
                .validate()
                .expect_err("a turn opener with an enclosing process is refused")
                .code,
            lash_core_execution::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener
        );
    }

    /// The cancellation authority is a validated identity, not free text: an
    /// authority that cannot exist is refused when the request is decoded, not
    /// when a recovered child tries to honour a cancellation.
    #[test]
    fn a_blank_cancellation_authority_is_refused_at_decode() {
        let mut value = serde_json::to_value(request()).expect("serializes");
        value
            .as_object_mut()
            .expect("a request is a JSON object")
            .insert(
                "cancellation_authority".to_string(),
                serde_json::Value::String(String::new()),
            );
        assert!(
            serde_json::from_value::<ToolChildRequest>(value).is_err(),
            "an empty binding id names an authority that cannot exist"
        );
    }

    /// The environment reference is required, so "no environment" is not a
    /// state the shape can hold. `captured_process_execution_env_ref` returns a
    /// reference or an error, never an absence, so a missing field is a
    /// truncated record rather than a legal one.
    #[test]
    fn a_request_without_an_execution_env_does_not_decode() {
        let mut value = serde_json::to_value(request()).expect("serializes");
        value
            .as_object_mut()
            .expect("a request is a JSON object")
            .remove("execution_env");
        let error = serde_json::from_value::<ToolChildRequest>(value)
            .expect_err("a missing environment reference must be refused");
        assert!(
            error.to_string().contains("execution_env"),
            "the refusal must name the missing field, got {error}"
        );
    }

    /// A process-lifetime key is a different fact from a durable one, and a reopen that
    /// guessed would derive a key nothing resolves (ADR 0099 §14).
    #[test]
    fn completion_routing_round_trips_every_mode() {
        for mode in [
            ToolChildCompletionRouting::Inline,
            ToolChildCompletionRouting::Durable,
            ToolChildCompletionRouting::ProcessLifetime {
                issuer: TurnControlBindingId::new("registry-identity-1")
                    .expect("a valid binding id"),
            },
        ] {
            let mut request = request();
            request.completion_routing = mode.clone();
            let decoded: ToolChildRequest =
                serde_json::from_str(&serde_json::to_string(&request).expect("serializes"))
                    .expect("decodes");
            assert_eq!(decoded.completion_routing, mode);
        }
    }

    /// Lineage is the attempt identity's parent, carried once. Losing it would
    /// reparent every attempt a recovered child makes.
    #[test]
    fn lineage_rides_the_attempt_identity_and_survives_the_round_trip() {
        let parent = lash_core_execution::RuntimeInvocation::effect(
            lash_core_execution::EffectAddress::new(
                lash_core_execution::ExecutionScope::turn("session", "turn"),
                "parent-effect",
            )
            .expect("a valid address"),
            lash_core_execution::RuntimeAttribution::for_session("session"),
            "parent-effect",
        );
        let mut request = request();
        request.attempt_identity = ToolAttemptEffectIdentity::Batch {
            parent: parent.clone(),
            replay_suffix: "leaf-2".to_string(),
        };
        let decoded: ToolChildRequest =
            serde_json::from_str(&serde_json::to_string(&request).expect("serializes"))
                .expect("decodes");
        match decoded.attempt_identity {
            ToolAttemptEffectIdentity::Batch {
                parent: decoded_parent,
                replay_suffix,
            } => {
                assert_eq!(decoded_parent, parent);
                assert_eq!(replay_suffix, "leaf-2");
            }
            other => panic!("the batch identity must survive, got {other:?}"),
        }
    }
}
