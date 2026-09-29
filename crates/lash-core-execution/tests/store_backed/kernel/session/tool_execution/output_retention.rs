//! An oversized presentation is retained inside its journaled
//! `PresentToolResult` effect, and a redrive under another retention policy
//! is served the recorded witness, reference and policy verbatim (FIG-1643).
//!
//! The call fails with a message far longer than the live policy lets into
//! history: a failure is retained exactly as a success is. The live pass
//! retains it and crashes after its presentation is journaled; the redrive
//! runs under a policy that would keep the message inline, and is still
//! served the retained block the live pass recorded.

use serde_json::json;
use std::sync::{Arc, Mutex};

const CALL_ID: &str = "oversized-call";
const SEED: u64 = 0x1643_0002;

/// The live pass's policy: a 1 KiB inline limit and a 256-byte witness.
const LIVE_POLICY: crate::OutputRetentionPolicy = crate::OutputRetentionPolicy {
    inline_limit_bytes: 1024,
    witness_bytes: 256,
};

fn message() -> String {
    "a stack frame of the failed call\n".repeat(600)
}

/// A context over the handler's lent controller whose attachment facade
/// applies `policy`: each attempt builds a fresh one, as a redrive does.
fn turn_context<'run>(
    backend: &crate::Backend,
    scoped: crate::ScopedEffectController<'run>,
    policy: crate::OutputRetentionPolicy,
) -> crate::RuntimeExecutionContext<'run> {
    crate::testing::TestExecutionContextBuilder::for_backend(backend)
        .session_id("retention-session")
        .borrowed_effect_controller(scoped)
        .attachment_store(Arc::new(
            crate::SessionAttachmentStore::ephemeral(backend.attachment_store())
                .with_output_retention(policy),
        ))
        .build()
        .into_runtime()
}

fn settled() -> crate::tool_dispatch::ToolDispatchOutcome {
    crate::tool_dispatch::ToolDispatchOutcome {
        record: crate::ToolCallRecord {
            call_id: lash_core_execution::ToolCallId::fixture(CALL_ID),
            provider_call_id: None,
            tool: "oversized".to_string(),
            args: json!({}),
            output: crate::ToolCallOutput::failure(crate::ToolFailure::io("dump", message())),
        },
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
        captures: Vec::new(),
        triggers: Vec::new(),
    }
}

async fn present(
    backend: &crate::Backend,
    scoped: crate::ScopedEffectController<'_>,
    policy: crate::OutputRetentionPolicy,
) -> crate::ModelToolReturn {
    turn_context(backend, scoped, policy)
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
        .model_return
}

#[tokio::test]
async fn an_oversized_failure_is_retained_and_replays_verbatim_under_a_changed_policy() {
    let double =
        crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let returns = Arc::new(Mutex::new(Vec::new()));
    let attempt =
        |policy: crate::OutputRetentionPolicy, crash: bool| -> lash_restate_test::HandlerAttempt {
            let backend = backend.clone();
            let returns = Arc::clone(&returns);
            Arc::new(move |scoped| {
                let backend = backend.clone();
                let returns = Arc::clone(&returns);
                Box::pin(async move {
                    let presented = present(&backend, scoped, policy).await;
                    returns.lock().expect("the returns cell").push(presented);
                    assert!(
                        !crash,
                        "the turn crashes after its presentation is journaled"
                    );
                })
            })
        };
    double
        .run_crashed_then_redriven(
            crate::AdmittedScope::turn("retention-session", "retention-turn"),
            attempt(LIVE_POLICY, true),
            attempt(crate::OutputRetentionPolicy::DEFAULT, false),
        )
        .await
        .expect("the live pass crashes and the redrive completes");
    let returns = returns.lock().expect("the returns cell").clone();
    let [live, redriven] = returns.as_slice() else {
        panic!("one crashed pass and one redrive presented: {returns:?}");
    };

    // The live pass kept a bounded witness and a reference in the failure's
    // place.
    let [crate::ModelToolReturnPart::Retained(retained)] = live.parts.as_slice() else {
        panic!(
            "the oversized failure is one retained block: {:?}",
            live.parts
        );
    };
    assert!(
        retained.witness.len() <= 256,
        "the witness is bounded by the recorded policy: {} bytes",
        retained.witness.len()
    );
    assert!(retained.witness.starts_with("[Tool execution failed]"));
    assert!(retained.witness.contains(retained.reference.id.as_str()));

    // The redrive, under a policy that keeps the message inline, is served
    // the recorded presentation.
    assert_eq!(
        redriven, live,
        "the redrive is served the recorded presentation"
    );

    // The reference resolves to the exact failure text the model would
    // otherwise have been shown.
    let stored = backend
        .attachment_store()
        .get(&retained.reference.id)
        .await
        .expect("the retained failure is stored");
    assert_eq!(
        String::from_utf8(stored.bytes).expect("retained text"),
        format!("[Tool execution failed]\n{}", message())
    );
}
