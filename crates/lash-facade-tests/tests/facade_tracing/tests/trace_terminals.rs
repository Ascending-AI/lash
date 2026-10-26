//! L15 (FIG-6299): every turn terminal closes its turn and its open tools.
//! The owner whose commit wrote the terminal exports `invoke_agent` and
//! every admitted tool span the terminal ended: a member it settled
//! `cancelled`, a member the session's close left with no outcome
//! `discarded`. Every exported span's parent is exported. No crash is
//! involved: one owner runs the turn and its end.

use super::trace_emission::{Observed, attribute};
use super::tracing::text_call;
use super::*;

use lash::TurnInput;
use lash::tracing::otel::trace::SpanId;
use lash_core::testing::runtime_helpers::{MockCall, mock_provider};
use opentelemetry_sdk::trace::SpanData;

const ECHO: &str = "echo_now";

const ECHO_OPERATION: &str = "tools.echo_now";
const HOLD: &str = "hold_open";
const HOLD_OPERATION: &str = "tools.hold_open";

fn definition(name: &str) -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{name}"),
        format!("tools.{name}"),
        "A terminal-closure probe.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// [`ECHO`] answers at once; [`HOLD`] opens `entered` and holds until its
/// token fires, or, `deaf`, until `release` opens.
#[derive(Clone)]
struct Tools {
    entered: Arc<tokio::sync::Notify>,
    deaf: bool,
    release: Arc<tokio::sync::Semaphore>,
}

impl Tools {
    fn new(deaf: bool) -> Self {
        Self {
            entered: Arc::default(),
            deaf,
            release: Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }
}

#[async_trait]
impl ToolProvider for Tools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        Ok(vec![
            definition(ECHO).manifest(),
            definition(HOLD).manifest(),
        ])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        let Some(("tools", name)) = name.split_once('.') else {
            return None;
        };
        [ECHO, HOLD]
            .contains(&name)
            .then(|| Arc::new(definition(name).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == HOLD_OPERATION {
            self.entered.notify_one();
            if self.deaf {
                let _permit = self.release.acquire().await;
                return lash_core::ToolOutcome::ok(serde_json::json!({ "late": true })).into();
            }
            call.context
                .cancellation_token()
                .cloned()
                .expect("an attempt carries the cooperative token")
                .cancelled()
                .await;
            return lash_core::ToolOutcome::cancelled("stopped").into();
        }
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

/// The model's first call asks for both tools in one round.
fn round_call() -> MockCall {
    MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: [("call-echo", ECHO), ("call-hold", HOLD)]
                .into_iter()
                .map(|(call_id, tool_name)| LlmOutputPart::ToolCall {
                    call_id: call_id.to_owned(),
                    tool_name: format!("tools__{tool_name}"),
                    input_json: "{}".to_owned(),
                    replay: None,
                })
                .collect(),
            ..LlmResponse::default()
        }),
    }
}

/// An observed core whose session `id` runs one turn into the round of
/// `tools`, with
/// [`ECHO`]'s span exported and [`HOLD`] running.
async fn held_round(id: &str, tools: &Tools) -> Result<(Observed, LashCore, lash::SendHandle)> {
    let builder = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        mock_provider(vec![round_call(), text_call("done")]).into_handle(),
        mock_llm_profile_spec(),
    )
    .tools(lash::tool_provider_key!("tools"), Arc::new(tools.clone()));
    let (observed, builder) = Observed::install(builder);
    let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
    let session = core
        .session(lash_kernel_value::identity::SessionId::parse(id).expect("nonblank host identity"))
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let handle = session.send(TurnInput::text("use both tools")).await?;
    tools.entered.notified().await;
    observed
        .until("the settled member's span is exported", |observed| {
            observed
                .spans(&format!("execute_tool {ECHO_OPERATION}"))
                .len()
                == 1
        })
        .await;
    Ok((observed, core, handle))
}

/// The terminal's closes: one `invoke_agent`, one span per admitted tool
/// with `outcome`s, and no exported span whose parent was not exported.
fn assert_closed(observed: &Observed, hold_outcome: &[&str]) {
    let all = observed.exported();
    let turns = observed.spans("invoke_agent");
    assert_eq!(turns.len(), 1, "one exported turn: {all:#?}");
    let echo = observed.spans(&format!("execute_tool {ECHO_OPERATION}"));
    assert_eq!(echo.len(), 1, "the settled member closed once: {all:#?}");
    assert_eq!(
        attribute(&echo[0], "lash.outcome").as_deref(),
        Some("completed")
    );
    let hold = observed.spans(&format!("execute_tool {HOLD_OPERATION}"));
    assert_eq!(hold.len(), 1, "the open member closed once: {all:#?}");
    let outcome = attribute(&hold[0], "lash.outcome");
    assert!(
        outcome
            .as_deref()
            .is_some_and(|outcome| hold_outcome.contains(&outcome)),
        "the open member closed as one of {hold_outcome:?}: {outcome:?}"
    );
    for tool in echo.iter().chain(&hold) {
        assert_eq!(
            tool.parent_span_id,
            turns[0].span_context.span_id(),
            "a tool is its turn's child"
        );
    }
    assert_parents_exported(&all);
}

/// Every exported span's parent is exported, or the span is a root.
fn assert_parents_exported(spans: &[SpanData]) {
    let ids: std::collections::HashSet<_> = spans
        .iter()
        .map(|span| span.span_context.span_id())
        .collect();
    for span in spans {
        assert!(
            span.parent_span_id == SpanId::INVALID || ids.contains(&span.parent_span_id),
            "{} names a parent that was not exported: {spans:#?}",
            span.name
        );
    }
}

/// (b) A session closed while its round has one member done and one
/// running, whether or not the running member answers its fired token:
/// the turn and each admitted tool are exported once, the running member
/// `cancelled` or `discarded`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_close_exports_its_turn_and_ends_its_running_member() -> Result<()> {
    for deaf in [false, true] {
        let id = format!("l15-close-{deaf}");
        let tools = Tools::new(deaf);
        let (observed, core, _handle) = held_round(&id, &tools).await?;
        delete_session_and_await(&core, &id).await?;
        observed
            .until("the close exports the turn", |observed| {
                observed.spans("invoke_agent").len() == 1
            })
            .await;
        tools.release.add_permits(1);
        assert_closed(&observed, &["cancelled", "discarded"]);
        core.shutdown().await?;
    }
    Ok(())
}

/// A turn cancelled while its round has one member done and one running:
/// the cancel's owner exports the turn and the running member,
/// `cancelled`, the terminal's closer a refusal shares.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_cancel_exports_its_turn_and_cancels_its_running_member() -> Result<()> {
    let (observed, core, handle) = held_round("l15-cancel", &Tools::new(false)).await?;
    handle
        .cancel()
        .request_id("l15")
        .mode(lash::TurnCancelMode::Immediate)
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(60), handle.output())
        .await
        .expect("the cancelled turn ends")?;
    observed
        .until("the cancel exports the turn", |observed| {
            observed.spans("invoke_agent").len() == 1
        })
        .await;
    assert_closed(&observed, &["cancelled", "discarded"]);
    core.shutdown().await?;
    Ok(())
}
