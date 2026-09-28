use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::facade_support::{
    ModelToolReturn, ModelToolReturnPart, ToolPresentationArtifacts, ToolPresentationInput,
    ToolResultProjectionContext,
};
use lash_core::runtime::effect::{RecordedChildStream, ToolSettlement};
use lash_core::{
    AttachmentId, AttachmentRef, MediaType, Message, MessageRole, Part, PluginError,
    RecordedRender, RuntimeCommit, RuntimeSessionState, SessionBinding, SessionCommitStore,
    SessionPolicy, ToolCallOutput, ToolId,
};
use lash_protocol_standard::render::{
    ResolvedStandardRenderConfig, ToolOutputRendererSlot, ToolRenderParams,
};
use lash_protocol_standard::{BuiltinToolOutputRenderer, ToolOutputRenderer};
use lash_sansio::session_model::message::render_prompt;
use lash_sqlite_store::SqliteStoreSet;

struct CountingRenderer {
    id: &'static str,
    calls: Arc<AtomicUsize>,
}

impl ToolOutputRenderer for CountingRenderer {
    fn id(&self) -> &str {
        self.id
    }

    fn tool_output(
        &self,
        output: &ToolCallOutput,
        tool: &ToolId,
        params: &ToolRenderParams,
    ) -> lash_render::Rendered<Vec<ModelToolReturnPart>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        BuiltinToolOutputRenderer.tool_output(output, tool, params)
    }
}

#[derive(Default)]
struct Retention {
    writes: AtomicUsize,
}

impl ToolPresentationArtifacts for Retention {
    #[expect(clippy::expect_used, reason = "fixed attachment fixture must parse")]
    fn retain_text<'a>(
        &'a self,
        _label: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<AttachmentRef, PluginError>> + Send + 'a>> {
        Box::pin(async move {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(AttachmentRef::new(
                AttachmentId::parse("retained-output").expect("id"),
                MediaType::parse("text/plain").expect("type"),
                text.len() as u64,
                None,
                None,
            ))
        })
    }
}

#[expect(
    clippy::expect_used,
    reason = "the fixed renderer fixture must present"
)]
async fn presented(
    call_id: &str,
    value: &str,
    params: ToolRenderParams,
    renderer: &ToolOutputRendererSlot,
    retained: &Arc<Retention>,
) -> ModelToolReturn {
    let output = ToolCallOutput::success(value);
    let previous = ModelToolReturn::from_output(call_id.into(), "fixture".into(), &output);
    let settlement = ToolSettlement {
        version: lash_core::TOOL_SETTLEMENT_VERSION,
        intent_outcomes: Vec::new(),
        possession: Vec::new(),
        triggers: Vec::new(),
        checkpoint_messages: Vec::new(),
        usage: Vec::new(),
        stream: RecordedChildStream::default(),
        model_return: previous.clone(),
    };
    lash_protocol_standard::render::present(
        ToolPresentationInput {
            previous,
            settlement: Arc::new(settlement),
            context: ToolResultProjectionContext {
                session_id: "render-cache-law".into(),
                call_id: call_id.into(),
                tool_id: ToolId::new("fixture:id"),
                tool_name: "fixture".into(),
                render: Some(RecordedRender {
                    renderer_id: renderer.0.id().into(),
                    params: serde_json::to_value(ResolvedStandardRenderConfig {
                        defaults: params,
                        per_tool: BTreeMap::new(),
                    })
                    .expect("params"),
                }),
                args: serde_json::Value::Null,
                output,
                duration_ms: 0,
                artifacts: retained.clone(),
            },
        },
        renderer,
    )
    .await
    .expect("presented")
}

fn result_message(id: &str, presented: ModelToolReturn) -> Message {
    Message {
        id: id.into(),
        role: MessageRole::User,
        parts: vec![Part::tool_result(
            format!("{id}.p0"),
            presented.parts,
            presented.call_id,
            presented.tool_name,
        )]
        .into(),
        origin: None,
    }
}

fn visible_text(presented: &ModelToolReturn) -> String {
    presented
        .parts
        .iter()
        .filter_map(|part| match part {
            ModelToolReturnPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn call_message(id: &str) -> Message {
    Message {
        id: format!("{id}.call"),
        role: MessageRole::Assistant,
        parts: vec![Part::tool_call(
            format!("{id}.call.p0"),
            "{}".into(),
            id.into(),
            "fixture".into(),
            None,
        )]
        .into(),
        origin: None,
    }
}

#[tokio::test]
async fn standard_rendered_history_is_stable_across_changes_and_reopen() {
    let stores = SqliteStoreSet::memory().await.expect("SQLite");
    let store = stores.open_store().await.expect("store");
    let mut state = RuntimeSessionState {
        session_id: "render-cache-law".into(),
        ..RuntimeSessionState::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    store
        .admit_and_bind_session(&SessionBinding::root(state.session_id.clone()))
        .await
        .expect("bind");

    let first_calls = Arc::new(AtomicUsize::new(0));
    let first = ToolOutputRendererSlot(Arc::new(CountingRenderer {
        id: "fixture.first",
        calls: first_calls.clone(),
    }));
    let retained = Arc::new(Retention::default());
    let mut first_params = ToolRenderParams::default();
    first_params.value.max_chars = 160;
    let first_cut = presented(
        "cut",
        &"a".repeat(800),
        first_params.clone(),
        &first,
        &retained,
    )
    .await;
    let first_cut_text = visible_text(&first_cut);
    assert!(first_cut_text.contains("[output cut:"));
    assert!(first_cut_text.chars().count() <= 160);
    let first_fit = presented("fit", "short", first_params, &first, &retained).await;
    assert_eq!(visible_text(&first_fit), "short");
    state.append_active_read_delta(&[
        call_message("cut"),
        result_message("cut", first_cut),
        call_message("fit"),
        result_message("fit", first_fit),
    ]);
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit first history");
    assert_eq!(first_calls.load(Ordering::SeqCst), 2);
    assert_eq!(retained.writes.load(Ordering::SeqCst), 1);

    let first_history = state.read_view().expect("view").messages().to_vec();
    assert!(lash_sansio::messages_are_prompt_resume_safe(&first_history));
    let prefix = serde_json::to_vec(&render_prompt(&first_history).messages).expect("first prompt");
    let rebuilt =
        serde_json::to_vec(&render_prompt(&first_history).messages).expect("rebuilt prompt");
    assert_eq!(prefix, rebuilt);

    state = lash_core::store::load_persisted_session_state(&store)
        .await
        .expect("reload after first commit")
        .expect("session");

    let second_calls = Arc::new(AtomicUsize::new(0));
    let second = ToolOutputRendererSlot(Arc::new(CountingRenderer {
        id: "fixture.second",
        calls: second_calls.clone(),
    }));
    let mut second_params = ToolRenderParams::default();
    second_params.value.max_chars = 250;
    let second_cut = presented("new", &"b".repeat(800), second_params, &second, &retained).await;
    let second_cut_text = visible_text(&second_cut);
    assert!(second_cut_text.contains("[output cut:"));
    assert!(second_cut_text.chars().count() <= 250);
    assert!(
        second_cut_text.matches('b').count() > first_cut_text.matches('a').count(),
        "the changed parameters must affect only the fresh output"
    );
    state.append_active_read_delta(&[call_message("new"), result_message("new", second_cut)]);
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit second history");
    let all_history = state.read_view().expect("view").messages().to_vec();
    assert_eq!(
        serde_json::to_value(&all_history[..first_history.len()]).expect("old messages"),
        serde_json::to_value(&first_history).expect("first messages"),
    );
    assert_eq!(
        serde_json::to_vec(&render_prompt(&all_history[..first_history.len()]).messages)
            .expect("old prefix"),
        prefix,
    );
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    assert_eq!(retained.writes.load(Ordering::SeqCst), 2);

    let reopened = stores.open_store().await.expect("reopened store");
    let cold = lash_core::store::load_persisted_session_state(&reopened)
        .await
        .expect("read")
        .expect("session");
    let reopened_history = cold.read_view().expect("cold view").messages().to_vec();
    assert_eq!(
        serde_json::to_value(&reopened_history).expect("cold messages"),
        serde_json::to_value(&all_history).expect("live messages"),
    );
    assert_eq!(
        serde_json::to_vec(&render_prompt(&reopened_history[..first_history.len()]).messages)
            .expect("cold prefix"),
        prefix,
    );
    assert_eq!(first_calls.load(Ordering::SeqCst), 2);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    assert_eq!(retained.writes.load(Ordering::SeqCst), 2);
}
