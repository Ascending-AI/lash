mod tests {
    use lash_core_execution::runtime::effect::*;
    use lash_core_execution::tool_dispatch::ToolAttemptLineage;
    use lash_core_execution::{
        FrameNodeId, PreparedToolCall, ProcessExecutionEnvRef, ProcessId, SessionId, ToolManifest,
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
                owner: lash_core_execution::plugin::PluginRevision::new(
                    "test_protocol",
                    lash_core_execution::plugin::BehaviorRevision::ONE,
                ),
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

    /// A request journaled by another build is refused, typed and before any
    /// effect: a child whose journal another version wrote would replay
    /// against a different step order.
    #[test]
    fn a_request_journaled_by_another_build_is_refused() {
        for version in [0, TOOL_CHILD_REQUEST_VERSION + 1] {
            let mut other = request();
            other.version = version;
            let error = other
                .validate()
                .expect_err("another build's request is refused");
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
}
