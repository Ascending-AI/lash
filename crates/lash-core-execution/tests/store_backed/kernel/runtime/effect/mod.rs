mod executor;
mod tool_child_driver;
mod tool_child_rebuild;

mod tests {
    use std::sync::Arc;

    use crate::LlmRequest as CoreLlmRequest;
    use crate::llm::types::{
        AttachmentSource, LlmEventSender, LlmMessage, LlmProviderTraceSender, LlmToolChoice,
    };
    use crate::runtime::effect::*;
    use crate::support::memory_backend;
    use crate::support::prelude::*;
    use crate::{SessionId, TurnId};

    #[tokio::test]
    async fn runtime_effect_envelope_and_request_specs_round_trip_without_live_fields() {
        let backend = memory_backend().await;
        let attachment_store = crate::SessionAttachmentStore::ephemeral(
            crate::Backend::from(backend.clone()).attachment_store(),
        );
        let llm_request = CoreLlmRequest {
            instructions: Some(Arc::from("I")),
            model: "model".to_string(),
            messages: vec![LlmMessage::new(
                crate::llm::types::LlmRole::User,
                vec![crate::llm::types::LlmContentBlock::Attachment {
                    source: Box::new(AttachmentSource::inline(
                        crate::MediaType::parse("image/png").unwrap(),
                        vec![1, 2, 3, 4],
                    )),
                }],
            )],
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::new()),
            tool_choice: LlmToolChoice::None,
            model_variant: crate::ReasoningSelection::Effort("fast".to_string()),
            model_capability: crate::ModelCapability::default(),
            scope: crate::LlmRequestScope::new(
                "session",
                "session:frame:test",
                "session:turn:test:llm:0",
            ),
            output_spec: None,
            stream_events: Some(LlmEventSender::new(|_| {})),
            generation: crate::GenerationOptions::default(),
            provider_trace: Some(LlmProviderTraceSender::new(|_| {})),
        };
        let spec = LlmRequestSpec::from_request(&llm_request, &attachment_store)
            .await
            .expect("llm spec");
        let encoded = serde_json::to_string(&spec).expect("serialize llm spec");
        assert!(!encoded.contains("stream_events"));
        assert!(!encoded.contains("provider_trace"));
        assert!(!encoded.contains("\"data\""));
        assert!(encoded.contains(crate::attachments::content_id(&[1, 2, 3, 4]).as_str()));
        let mut legacy: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        legacy["messages"][0]["blocks"][0] =
            serde_json::json!({"Attachment": {"attachment_idx": 2}});
        legacy["attachments"] = serde_json::json!([]);
        let error = serde_json::from_value::<LlmRequestSpec>(legacy)
            .expect_err("a dangling legacy attachment must fail persisted-request decode");
        assert!(
            error.to_string().contains("missing field `source`"),
            "{error}"
        );
        let decoded: LlmRequestSpec = serde_json::from_str(&encoded).expect("decode llm spec");
        let live = decoded.into_request(None, None);
        assert_eq!(live.model, "model");
        assert_eq!(live.instructions.as_deref(), Some("I"));
        assert!(matches!(
            live.attachments()[0],
            AttachmentSource::Stored { .. }
        ));
        assert!(live.stream_events.is_none());
        assert!(live.provider_trace.is_none());

        let invocation = crate::runtime::causal::direct_effect_invocation(
            &ExecutionScope::turn("session", "turn"),
            &SessionId::from("session"),
            "test",
            "request:direct".to_string(),
            Some(&TurnId::from("turn")),
            None,
        );
        let envelope = RuntimeEffectEnvelope::new(
            invocation,
            RuntimeEffectCommand::Direct {
                request: Box::new(
                    LlmRequestSpec::from_request(&llm_request, &attachment_store)
                        .await
                        .expect("normalized spec"),
                ),
                usage_source: "test".to_string(),
            },
        );
        let hash = envelope.stable_hash().expect("stable hash");
        assert!(!hash.is_empty());
        let encoded = serde_json::to_string(&envelope).expect("serialize envelope");
        let decoded: RuntimeEffectEnvelope =
            serde_json::from_str(&encoded).expect("decode envelope");
        assert_eq!(
            decoded.invocation.replay_key(),
            envelope.invocation.replay_key()
        );
        assert_eq!(decoded.command.kind(), RuntimeEffectKind::Direct);
    }

    #[tokio::test]
    async fn resolver_defaults_refuse_turn_control_without_an_explicit_host() {
        struct UnsupportedResolver;
        impl AwaitEventResolver for UnsupportedResolver {
            /// A test double that mints keys under no durable authority.
            fn await_event_authority_binding_id(&self) -> Option<String> {
                None
            }
        }

        let resolver = UnsupportedResolver;
        let scope = ExecutionScope::turn("unsupported-session", "unsupported-turn");
        for wait in [
            AwaitEventWaitIdentity::TurnCancelGate,
            AwaitEventWaitIdentity::TurnTerminal,
            AwaitEventWaitIdentity::tool_completion("unsupported-call"),
        ] {
            let error = resolver
                .await_event_key(&scope, wait)
                .await
                .expect_err("default resolver must refuse every identity");
            assert_eq!(error.code.as_str(), "await_event_unsupported");
        }

        let backend = memory_backend().await;
        let key = backend
            .effect_host()
            .await_event_key(&scope, AwaitEventWaitIdentity::TurnCancelGate)
            .await
            .expect("explicit backend host key");
        assert_eq!(
            resolver
                .resolve_await_event(&key, Resolution::Cancelled)
                .await
                .expect("default resolution has one opaque shape"),
            ResolveOutcome::UnknownOrRevoked
        );
        for error in [
            resolver
                .peek_await_event(&key)
                .await
                .expect_err("default resolver must refuse reads"),
            resolver
                .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
                .await
                .expect_err("default resolver must refuse waits"),
            resolver
                .revoke_await_events_for_session(&SessionId::from("unsupported-session"))
                .await
                .expect_err("default resolver must refuse revocation"),
        ] {
            assert_eq!(error.code.as_str(), "await_event_unsupported");
        }
    }

    #[tokio::test]
    async fn await_event_key_is_stable_for_scope_and_wait_identity() {
        let backend = memory_backend().await;
        let host = backend.effect_host();
        let scope = ExecutionScope::turn("session", "turn");
        let wait = AwaitEventWaitIdentity::tool_completion("call");

        let first = host
            .await_event_key(&scope, wait.clone())
            .await
            .expect("first key");
        let second = host
            .await_event_key(&scope, wait)
            .await
            .expect("second key");

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn duplicate_await_event_resolution_reports_existing_terminal() {
        let backend = memory_backend().await;
        let host = backend.effect_host();
        let scope = ExecutionScope::turn("session-dupe", "turn-dupe");
        let key = host
            .await_event_key(&scope, AwaitEventWaitIdentity::tool_completion("call-dupe"))
            .await
            .expect("key");
        let resolution = Resolution::Ok(serde_json::json!({"done": true}));

        let first = host
            .resolve_await_event(&key, resolution.clone())
            .await
            .expect("first resolve");
        let second = host
            .resolve_await_event(&key, Resolution::Ok(serde_json::json!({"ignored": true})))
            .await
            .expect("duplicate resolve");

        assert_eq!(first, ResolveOutcome::Accepted);
        assert_eq!(
            second,
            ResolveOutcome::AlreadyResolved {
                terminal: resolution
            }
        );
    }
}
