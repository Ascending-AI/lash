//! Test-only re-exports for `lash-core`'s remaining in-crate unit tests.
//!
//! The runtime suites that used to live here are now integration test binaries
//! under `crates/lash-core/tests/runtime/tests/`. The fixtures they shared with
//! the crate's own unit tests moved to `crate::testing`; this module keeps the
//! historical `crate::runtime::tests::{helpers, trace_capture}` paths pointing
//! at them.

pub(crate) use crate::testing::trace_capture;

pub(crate) mod helpers {
    pub(crate) use crate::testing::runtime_helpers::*;

    use crate::runtime::*;
    use std::sync::Arc;

    #[test]
    pub(crate) fn stream_accumulator_merges_adjacent_display_reasoning_chunks() {
        let mut accumulator = LlmStreamAccumulator::default();
        accumulator.push_reasoning("I'll".to_string(), None, Vec::new(), None);
        accumulator.push_reasoning(" check".to_string(), None, Vec::new(), None);
        accumulator.push_reasoning(" the time.".to_string(), None, Vec::new(), None);

        assert_eq!(accumulator.parts.len(), 1);
        assert!(matches!(
            &accumulator.parts[0],
            LlmOutputPart::Reasoning { text, .. } if text == "I'll check the time."
        ));
    }

    #[test]
    pub(crate) fn stream_accumulator_enriches_reasoning_delta_with_later_roundtrip_payload() {
        let mut accumulator = LlmStreamAccumulator::default();
        accumulator.push_reasoning("I'll check the time.".to_string(), None, Vec::new(), None);
        accumulator.push_reasoning(
            "I'll check the time.".to_string(),
            Some("rs_1".to_string()),
            vec!["I'll check the time.".to_string()],
            Some("encrypted".to_string()),
        );

        assert_eq!(accumulator.parts.len(), 1);
        assert!(matches!(
            &accumulator.parts[0],
            LlmOutputPart::Reasoning {
                text,
                replay: Some(replay),
                ..
            } if text == "I'll check the time."
                && replay.item_id.as_deref() == Some("rs_1")
                && replay.encrypted_content.as_deref() == Some("encrypted")
        ));
    }

    #[test]
    pub(crate) fn stream_accumulator_preserves_reasoning_when_final_response_has_tool_call() {
        let mut accumulator = LlmStreamAccumulator::default();
        accumulator.push_reasoning("I'll check the time.".to_string(), None, Vec::new(), None);
        accumulator.push_tool_call(
            "call_1".to_string(),
            "exec_command".to_string(),
            "{\"cmd\":\"date\"}".to_string(),
            Some(lash_sansio::llm::types::ProviderReplayMeta {
                item_id: Some("item_1".to_string()),
                opaque: Some("sig".to_string()),
                ..Default::default()
            }),
        );

        let mut response = LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "call_1".to_string(),
                tool_name: "exec_command".to_string(),
                input_json: "{\"cmd\":\"date\"}".to_string(),
                replay: Some(lash_sansio::llm::types::ProviderReplayMeta {
                    item_id: Some("item_1".to_string()),
                    opaque: Some("sig".to_string()),
                    ..Default::default()
                }),
            }],
            response_metadata: Default::default(),
            ..Default::default()
        };

        accumulator.apply_to_response(&mut response);

        assert_eq!(response.parts.len(), 2);
        assert!(matches!(
            &response.parts[0],
            LlmOutputPart::Reasoning { text, .. } if text == "I'll check the time."
        ));
        assert!(matches!(
            &response.parts[1],
            LlmOutputPart::ToolCall { tool_name, .. } if tool_name == "exec_command"
        ));
    }

    #[test]
    pub(crate) fn stream_accumulator_does_not_duplicate_complete_final_response() {
        let mut accumulator = LlmStreamAccumulator::default();
        accumulator.push_reasoning("I'll answer.".to_string(), None, Vec::new(), None);
        accumulator.push_text("Done.");

        let mut response = LlmResponse {
            parts: vec![
                LlmOutputPart::Reasoning {
                    text: "I'll answer.".to_string(),
                    replay: None,
                },
                LlmOutputPart::Text {
                    text: "Done.".to_string(),
                    response_meta: None,
                },
            ],
            response_metadata: Default::default(),
            ..Default::default()
        };

        accumulator.apply_to_response(&mut response);

        assert_eq!(response.parts.len(), 2);
        assert!(matches!(
            &response.parts[0],
            LlmOutputPart::Reasoning { text, .. } if text == "I'll answer."
        ));
        assert!(matches!(
            &response.parts[1],
            LlmOutputPart::Text { text, .. } if text == "Done."
        ));
    }

    #[tokio::test]
    async fn recording_factory_root_set_keeps_committed_blob() {
        let factory = RecordingSessionStoreFactory::default();
        let request = crate::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("recording-factory-gc"),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        };
        let store = factory.create_store(&request).await.expect("create store");
        let backend = Arc::new(crate::InMemoryAttachmentStore::new());
        let attachment_backend: Arc<dyn crate::AttachmentStore> = backend.clone();
        let manifest: Arc<dyn crate::AttachmentManifest> = store.clone();
        let session = crate::SessionAttachmentStore::new(
            attachment_backend,
            manifest,
            request.session_id.clone(),
        );
        let attachment = session
            .put(
                b"recording-factory-live-blob".to_vec(),
                crate::AttachmentCreateMeta::new(
                    crate::MediaType::parse("application/octet-stream").unwrap(),
                    None,
                    None,
                ),
            )
            .await
            .expect("put attachment");
        store
            .commit_refs(&request.session_id, std::slice::from_ref(&attachment.id))
            .await
            .expect("commit attachment ref");
        assert_eq!(
            store.list_all_refs().await.unwrap(),
            vec![attachment.id.clone()]
        );

        let report = crate::reclaim_unreferenced_attachments(
            &factory,
            &*backend,
            crate::AttachmentReclamationPolicy {
                grace_period_ms: 0,
                empty_root_set: crate::EmptyRootSetPolicy::Refuse,
            },
        )
        .await
        .expect("sweep");

        assert_eq!(report.scanned_blob_count, 1);
        assert_eq!(report.reclaimed_count, 0);
        assert!(report.deleted_while_referenced.is_empty());
        crate::AttachmentStore::get(&*backend, &attachment.id)
            .await
            .expect("committed blob survives");
    }

    #[tokio::test]
    async fn test_runtime_process_registry_defaults_and_can_be_disabled() {
        let runtime = TestRuntime::new(mock_provider(Vec::new())).build().await;
        assert!(runtime.host.process_registry().is_some());

        let runtime = TestRuntime::new(mock_provider(Vec::new()))
            .without_process_registry()
            .build()
            .await;
        assert!(runtime.host.process_registry().is_none());
    }
}
