mod tests {
    use lash_core_execution::runtime::effect::*;
    use lash_core_execution::tool_dispatch::ToolAttemptLineage;
    use lash_core_execution::{
        FrameNodeId, PreparedToolCall, ProcessExecutionEnvRef, ProcessId, SessionId,
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
        .expect("valid declared tool schemas")
    }

    fn manifest(id: &str) -> ToolManifest {
        definition(id).manifest()
    }

    fn frame() -> FrameNodeId {
        FrameNodeId::new("frame-1").expect("a valid frame id")
    }

    fn call(tool_id: &str) -> PreparedToolCall {
        PreparedToolCall {
            call_id: lash_core_execution::ToolCallId::fixture("call-1"),
            provider_call_id: None,
            tool_id: ToolId::from(tool_id),
            tool_name: "search".into(),
            args: serde_json::json!({ "q": "lash" }),
            replay: None,
            prepared_payload: serde_json::Value::Null,
        }
    }

    fn scope() -> ToolChildScope {
        ToolChildScope {
            opener: EffectOpener::turn("session", "turn"),
            owner: lash_core_execution::ExecutionOwner::SessionFrame {
                session_id: SessionId::from("session"),
                agent_frame_id: frame(),
            },
        }
    }

    fn process_id(name: &str) -> ProcessId {
        ProcessId::fixture(name)
    }

    fn authority() -> TurnControlBindingId {
        TurnControlBindingId::new("binding-7").expect("a valid binding id")
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
            ToolAttemptLineage::default(),
            scope(),
            authority(),
            env(),
            ToolChildCompletionRouting::Durable,
            lash_core_execution::runtime::effect::ToolChildSessionFacts::default(),
        )
    }

    /// Every field survives the durable round trip. A request that lost a field
    /// in serialization is a child recovered under partial authority, which is
    /// the exact failure §3 retains input to prevent.
    #[test]
    fn a_request_round_trips_every_field_through_its_durable_bytes() {
        let mut original = request();
        original.scope.opener = EffectOpener::process(process_id("process-9"));
        let json = serde_json::to_string(&original).expect("a request serializes");
        let decoded: ToolChildRequest = serde_json::from_str(&json).expect("a request decodes");
        assert_eq!(decoded, original);
        assert_eq!(decoded.version, TOOL_CHILD_REQUEST_VERSION);
        assert_eq!(decoded.cancellation_authority.as_str(), "binding-7");
        assert_eq!(decoded.execution_env.as_str(), "env-ref");
        assert_eq!(
            decoded.enclosing_process(),
            Some(&process_id("process-9")),
            "the enclosing process is the recorded opener's own"
        );
        assert_eq!(
            decoded
                .scope
                .owner
                .agent_frame_id()
                .map(|frame| frame.as_str()),
            Some("frame-1")
        );
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
            ToolAttemptLineage::default(),
            scope(),
            authority(),
            env(),
            ToolChildCompletionRouting::Inline,
            lash_core_execution::runtime::effect::ToolChildSessionFacts::default(),
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

    /// The opener is recorded once (FIG-4665): the claim scope and the
    /// enclosing process are derived from it, so no request can pair a
    /// process opener with another claim scope, and a retained copy of either
    /// is an unknown field, refused at decode.
    #[test]
    fn the_claim_scope_and_enclosing_process_are_the_openers_own() {
        let mut request = request();
        assert_eq!(
            request.scope.claim_scope(),
            AdmittedScope::turn("session", "turn")
        );
        assert_eq!(request.enclosing_process(), None);
        request.scope.opener = EffectOpener::process(process_id("process-1"));
        request.validate().expect("a process opener's request");
        assert_eq!(
            request.scope.claim_scope(),
            AdmittedScope::process(process_id("process-1"))
        );
        assert_eq!(request.enclosing_process(), Some(&process_id("process-1")));
        let wire = serde_json::to_value(&request).expect("serializes");
        assert!(wire.get("enclosing_process").is_none());
        assert!(wire["scope"].get("admitted_scope").is_none());
        for (parent, field) in [
            (None, "enclosing_process"),
            (Some("scope"), "admitted_scope"),
        ] {
            let mut stale = wire.clone();
            let object = match parent {
                Some(parent) => &mut stale[parent],
                None => &mut stale,
            };
            object[field] = serde_json::json!("process-2");
            assert!(
                serde_json::from_value::<ToolChildRequest>(stale).is_err(),
                "a second copy of the opener's {field} is refused"
            );
        }
    }

    /// The envelope checks its address against the request's one opener: a
    /// child addressed outside its claim scope is refused at construction
    /// and at decode, typed, so it can never be claimed under one scope and
    /// run under another.
    #[test]
    fn an_envelope_addressed_outside_its_openers_scope_is_refused() {
        let envelope = |scope: lash_core_execution::ExecutionScope| {
            RuntimeEffectEnvelope::try_new(
                RuntimeEffectInvocation::new(
                    lash_core_execution::EffectAddress::new(scope, "child")
                        .expect("a valid effect address"),
                    lash_core_execution::RuntimeAttribution::none(),
                    "child",
                ),
                RuntimeEffectCommand::ToolInvocation {
                    request: Box::new(request()),
                },
            )
        };
        let admitted = envelope(lash_core_execution::ExecutionScope::turn("session", "turn"))
            .expect("the opener's own scope is the child's address");
        for foreign in [
            lash_core_execution::ExecutionScope::turn("session", "another-turn"),
            lash_core_execution::ExecutionScope::process(process_id("process-1")),
        ] {
            assert_eq!(
                envelope(foreign.clone())
                    .expect_err("an address outside the opener's scope is refused")
                    .code,
                lash_core_execution::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener
            );
            let mut wire = serde_json::to_value(&admitted).expect("serializes");
            wire["invocation"]["address"]["execution_scope"] =
                serde_json::to_value(&foreign).expect("a scope serializes");
            let refusal = serde_json::from_value::<RuntimeEffectEnvelope>(wire)
                .expect_err("a decoded envelope is checked as a constructed one is");
            assert!(
                refusal
                    .to_string()
                    .contains("claimed under its opener's own scope"),
                "{refusal}"
            );
        }
        serde_json::from_value::<RuntimeEffectEnvelope>(
            serde_json::to_value(&admitted).expect("serializes"),
        )
        .expect("the admitted envelope round-trips");
    }

    /// The defect ADR 0099 §1 names, at the shape level: a process re-registered
    /// under the same name is a different opener, and a retained request must
    /// not compare equal to one admitted under its predecessor. An
    /// `ExecutionScope::Process` could not express this at all.
    #[test]
    fn another_process_opener_is_not_the_opener_that_was_admitted() {
        let mut admitted = request();
        admitted.scope.opener = EffectOpener::process(process_id("indexer-a"));
        let mut successor = request();
        successor.scope.opener = EffectOpener::process(process_id("indexer-b"));
        assert_ne!(admitted.scope.opener, successor.scope.opener);

        let decoded: ToolChildRequest =
            serde_json::from_str(&serde_json::to_string(&admitted).expect("serializes"))
                .expect("decodes");
        assert_eq!(
            decoded.scope.opener.process_id(),
            Some(&process_id("indexer-a")),
            "the admitted process is what recovery validates against"
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

    /// Every child records the durable authority its opener's cooperative
    /// signal is fenced on, so a request without one is a truncated record
    /// rather than a legal one.
    #[test]
    fn a_request_without_a_cancellation_authority_does_not_decode() {
        let mut value = serde_json::to_value(request()).expect("serializes");
        value
            .as_object_mut()
            .expect("a request is a JSON object")
            .remove("cancellation_authority");
        let error = serde_json::from_value::<ToolChildRequest>(value)
            .expect_err("a missing cancellation authority must be refused");
        assert!(
            error.to_string().contains("cancellation_authority"),
            "the refusal must name the missing field, got {error}"
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

    /// An inline child is a different fact from a deferring one, and a reopen
    /// that guessed would derive a key nothing resolves (ADR 0099 §14).
    #[test]
    fn completion_routing_round_trips_every_mode() {
        for mode in [
            ToolChildCompletionRouting::Inline,
            ToolChildCompletionRouting::Durable,
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
        request.lineage = ToolAttemptLineage::under(parent.clone());
        let decoded: ToolChildRequest =
            serde_json::from_str(&serde_json::to_string(&request).expect("serializes"))
                .expect("decodes");
        assert_eq!(decoded.lineage.parent_invocation(), Some(&parent));
    }
}
