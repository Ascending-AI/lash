//! A code cell is one `execute_tool {language}` span (the tracing lane T5,
//! FIG-6303): the turn span's child, the parent of the tool calls the cell
//! makes, timed from the cell's own first start, and closed once, by the
//! owner whose commit consumes its answer or, for a cell its turn's
//! terminal stops, by that terminal's owner as `discarded`.
//!
//! - L3's cell rows: a served cell nests its call under itself, starts at or
//!   after its turn and before its call, and a failed cell's span is its
//!   own time, not its turn's.
//! - L15(a): a turn cancelled while its cell runs, after the cell's first
//!   call was exported, exports its turn, the cell `discarded` and every
//!   call the cell admitted, each under an exported parent.
//!
//! Real RLM turns on the core's own node over SQLite memory stores, observed
//! through the OpenTelemetry adapter.

use super::trace_emission::{Observed, attribute};
use super::*;

use lash::TurnInput;
use lash::tracing::otel::trace::{SpanId, Status};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::llm::types::LlmResponse;
use opentelemetry_sdk::trace::SpanData;

const ECHO: &str = "echo_now";
const HOLD: &str = "hold_open";

fn definition(name: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A cell-span probe.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
}

/// [`ECHO`] answers at once; [`HOLD`] opens `entered` and holds until its
/// cooperative token fires.
#[derive(Clone, Default)]
struct Tools {
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ToolProvider for Tools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
        Ok(vec![
            definition(ECHO).manifest(),
            definition(HOLD).manifest(),
        ])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        [ECHO, HOLD]
            .contains(&name)
            .then(|| Arc::new(definition(name).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == HOLD {
            self.entered.notify_one();
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

/// A TypeScript cell of `code`, as the model answers it.
fn cell(code: &str) -> LlmResponse {
    lash_facade_tests::control_cell_response(
        &lash::rlm::CellDialect::typescript(),
        lash::rlm::RlmChannel::Cell,
        0,
        code.to_owned(),
    )
}

/// The model's answers, served in order; each takes `pause` first, so the
/// cells it asks for start measurably after their turns.
const PAUSE: std::time::Duration = std::time::Duration::from_millis(60);

/// An observed RLM core serving `tools`, whose model answers `responses`.
fn observed_core(
    backend: lash_core::Backend,
    tools: &Tools,
    responses: Vec<LlmResponse>,
) -> Result<(Observed, LashCore)> {
    let responses = Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::from(
        responses,
    )));
    let provider = lash::testing::TestProvider::builder()
        .kind("trace-cells")
        .complete(move |_request: LlmRequest| {
            let responses = Arc::clone(&responses);
            async move {
                tokio::time::sleep(PAUSE).await;
                Ok(responses
                    .lock()
                    .await
                    .pop_front()
                    .expect("a scripted response"))
            }
        })
        .build()
        .into_handle();
    let config = lash::rlm::RlmProtocolPluginConfig::builder()
        .channel(lash::rlm::RlmChannel::Cell)
        .instruction_limit(lash::rlm::InstructionBound::standard())
        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
        .build();
    let factory =
        lash::rlm::RlmProtocolPluginFactory::new(config, lash::rlm::CellDialect::typescript())
            .with_worker_service(untimed_fixture_workers());
    let builder = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(lash::tool_provider_key!("tools"), Arc::new(tools.clone()));
    let (observed, builder) = Observed::install(builder);
    Ok((
        observed,
        builder.build(lash_facade_tests::runtime_lease_owner())?,
    ))
}

fn parent_of<'a>(spans: &'a [SpanData], child: &SpanData) -> Option<&'a SpanData> {
    spans
        .iter()
        .find(|span| span.span_context.span_id() == child.parent_span_id)
}

/// Every exported span's parent is exported, or the span is a root.
fn assert_parents_exported(spans: &[SpanData]) {
    for span in spans {
        assert!(
            span.parent_span_id == SpanId::INVALID || parent_of(spans, span).is_some(),
            "{} names a parent that was not exported: {spans:#?}",
            span.name
        );
    }
}

/// L3's cell rows: a cell that calls a tool, then a cell that fails. Each
/// cell is one `execute_tool typescript` span, its turn's child; the call is
/// the first cell's child, starting after it; and the failed cell's span
/// starts after its turn's model call, not at its turn's start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cell_parents_its_calls_and_spans_its_own_time() -> Result<()> {
    let (observed, core) = observed_core(
        sqlite_memory_store_backend().await,
        &Tools::default(),
        vec![
            cell(&format!("await tools.{ECHO}({{}});")),
            text_response("called"),
            cell("throw new Error(\"boom\");"),
            text_response("failed"),
        ],
    )?;
    let session = created_session(&core, SessionId::fixture("trace-cell-nesting"))
        .await
        .open()
        .await?;
    for prompt in ["call the tool", "fail"] {
        let output = session.send(TurnInput::text(prompt)).output().await?;
        assert!(output.is_success(), "{prompt}: {:?}", output.result.outcome);
    }
    observed
        .until("both turns are exported", |observed| {
            observed.spans("invoke_agent").len() == 2
        })
        .await;
    let spans = observed.exported();
    let cells = observed.spans("execute_tool typescript");
    let [called, failed] = &cells[..] else {
        panic!("one span per cell: {spans:#?}");
    };
    let call = observed.spans(&format!("execute_tool {ECHO}"));
    let [call] = &call[..] else {
        panic!("one call span: {spans:#?}");
    };
    assert_eq!(
        call.parent_span_id,
        called.span_context.span_id(),
        "the cell's call is the cell's child"
    );
    for cell in [called, failed] {
        let turn = parent_of(&spans, cell).expect("the cell's parent is exported");
        assert_eq!(turn.name, "invoke_agent", "a cell is its turn's child");
        assert!(
            cell.start_time >= turn.start_time,
            "a cell starts in its turn"
        );
        assert!(cell.end_time <= turn.end_time, "a cell ends in its turn");
        assert_eq!(
            attribute(cell, "gen_ai.tool.name").as_deref(),
            Some("typescript")
        );
    }
    assert!(
        called.start_time <= call.start_time,
        "the cell starts before its call"
    );
    assert_eq!(
        attribute(called, "lash.outcome").as_deref(),
        Some("completed")
    );
    assert_eq!(
        attribute(called, "lash.cell.tool_calls").as_deref(),
        Some("1")
    );
    assert!(matches!(failed.status, Status::Error { .. }));
    let failed_turn = parent_of(&spans, failed).expect("the failed cell's turn");
    assert!(
        failed.start_time >= failed_turn.start_time + PAUSE,
        "the failed cell's span is its own time, after its turn's model call, not its turn's"
    );
    assert_parents_exported(&spans);
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// L15(a): a turn cancelled while its cell runs, once the cell's first call
/// was exported. The cancelling owner exports the turn, the cell
/// `discarded` with no error, and the cell's held call; both calls are the
/// cell's children, and every exported span's parent is exported.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_cancel_discards_its_running_cell_under_its_turn() -> Result<()> {
    let tools = Tools::default();
    let (observed, core) = observed_core(
        sqlite_memory_store_backend().await,
        &tools,
        vec![cell(&format!(
            "await tools.{ECHO}({{}});\nawait tools.{HOLD}({{}});"
        ))],
    )?;
    let session = created_session(&core, SessionId::fixture("trace-cell-cancel"))
        .await
        .open()
        .await?;
    let handle = session.send(TurnInput::text("call, then hold")).await?;
    tools.entered.notified().await;
    observed
        .until("the cell's first call is exported", |observed| {
            observed.spans(&format!("execute_tool {ECHO}")).len() == 1
        })
        .await;
    handle
        .cancel()
        .request_id("l15-cell")
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
    let spans = observed.exported();
    let turns = observed.spans("invoke_agent");
    let cells = observed.spans("execute_tool typescript");
    let [cell] = &cells[..] else {
        panic!("the cancel closes the cell once: {spans:#?}");
    };
    assert_eq!(cell.parent_span_id, turns[0].span_context.span_id());
    assert_eq!(
        attribute(cell, "lash.outcome").as_deref(),
        Some("discarded")
    );
    assert!(!matches!(cell.status, Status::Error { .. }));
    for (tool, outcomes) in [
        (ECHO, &["completed"][..]),
        (HOLD, &["cancelled", "discarded"][..]),
    ] {
        // A call the terminal closes without an outcome is named by its
        // tool id (`turn_trace::members_ended`), a settled one by its name.
        let mut calls = observed.spans(&format!("execute_tool {tool}"));
        calls.extend(observed.spans(&format!("execute_tool tool:{tool}")));
        let [call] = &calls[..] else {
            panic!("{tool} closed once: {spans:#?}");
        };
        assert_eq!(
            call.parent_span_id,
            cell.span_context.span_id(),
            "{tool} is the cell's child"
        );
        let outcome = attribute(call, "lash.outcome");
        assert!(
            outcome
                .as_deref()
                .is_some_and(|outcome| outcomes.contains(&outcome)),
            "{tool} closed as one of {outcomes:?}: {outcome:?}"
        );
    }
    assert_parents_exported(&spans);
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// FIG-6432 / S1: a predicted answer after the cell close is never the turn's
/// answer, and its discard count is visible without capturing model content.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_discarded_prediction_is_counted_on_its_cell_span() -> Result<()> {
    let tail = "Sent a joke…";
    let reply = text_response(&format!(
        "<typescript>\nawait tools.{ECHO}({{}});\n</typescript>{tail}"
    ));
    let (observed, core) = observed_core(
        sqlite_memory_store_backend().await,
        &Tools::default(),
        vec![reply, text_response("Real answer.")],
    )?;
    let session = created_session(&core, SessionId::fixture("trace-cell-discard"))
        .await
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("call the tool"))
        .output()
        .await?;
    assert!(output.is_success(), "{:?}", output.result.outcome);
    observed
        .until("the turn is exported", |observed| {
            observed.spans("invoke_agent").len() == 1
        })
        .await;
    let cells = observed.spans("execute_tool typescript");
    let [cell] = &cells[..] else {
        panic!("one cell span: {cells:#?}")
    };
    assert_eq!(
        attribute(cell, "lash.cell.trailing_discarded"),
        Some(tail.len().to_string())
    );
    assert_eq!(
        attribute(cell, "lash.cell.tool_calls").as_deref(),
        Some("1")
    );
    assert_eq!(observed.spans(&format!("execute_tool {ECHO}")).len(), 1);
    drop(session);
    core.shutdown().await?;
    Ok(())
}
