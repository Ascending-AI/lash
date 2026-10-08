//! An oversized tool failure is retained exactly as an oversized success is
//! (FIG-1643): its presentation keeps a bounded witness and a reference in
//! the failure's place, and the reference resolves to the exact failure text
//! the model would otherwise have been shown.

use std::sync::Arc;

const CALL_ID: &str = "oversized-call";

/// A 1 KiB inline limit and a 256-byte witness.
const POLICY: crate::OutputRetentionPolicy = crate::OutputRetentionPolicy {
    inline_limit_bytes: 1024,
    witness_bytes: 256,
};

fn message() -> String {
    "a stack frame of the failed call\n".repeat(600)
}

fn settled() -> crate::tool_dispatch::ToolDispatchOutcome {
    crate::tool_dispatch::ToolDispatchOutcome {
        record: crate::ToolCallRecord {
            call_id: lash_core_execution::ToolCallId::fixture(CALL_ID),
            provider_call_id: None,
            tool: "oversized".to_string(),
            args: serde_json::json!({}),
            output: crate::ToolCallOutput::failure(crate::ToolFailure::io("dump", message())),
        },
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
        triggers: Vec::new(),
    }
}

#[tokio::test]
async fn an_oversized_failure_is_retained_behind_a_bounded_witness() {
    let backend = crate::support::sqlite_memory_store_backend().await;
    let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
        .session_id("retention-session")
        .attachment_store(Arc::new(
            crate::RuntimeAttachmentStore::ephemeral(backend.attachment_store())
                .with_output_retention(POLICY),
        ))
        .build()
        .into_runtime();

    let presented = context
        .complete_tool_call(
            lash_core_execution::tool_dispatch::ToolCallIds {
                call_id: lash_core_execution::ToolCallId::fixture(CALL_ID),
                provider_call_id: None,
            },
            crate::ToolId::new("oversized"),
            None,
            settled(),
            "test:call",
            1,
        )
        .await
        .expect("the call presents")
        .completed
        .model_return;

    let [crate::ModelToolReturnPart::Retained(retained)] = presented.parts.as_slice() else {
        panic!(
            "the oversized failure is one retained block: {:?}",
            presented.parts
        );
    };
    assert!(
        retained.witness.len() <= 256,
        "the witness is bounded by the policy: {} bytes",
        retained.witness.len()
    );
    assert!(retained.witness.starts_with("[Tool execution failed]"));
    assert!(retained.witness.contains(retained.reference.id.as_str()));
    let stored = backend
        .attachment_store()
        .get(
            &retained.reference.id,
            crate::AttachmentReadPolicy::DEFAULT.max_blob_bytes,
        )
        .await
        .expect("the retained failure is stored");
    assert_eq!(
        String::from_utf8(stored.bytes).expect("retained text"),
        format!("[Tool execution failed]\n{}", message())
    );
}
