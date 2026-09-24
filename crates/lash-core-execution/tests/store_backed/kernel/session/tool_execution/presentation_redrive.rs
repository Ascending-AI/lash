//! A redriven tool call replays its recorded presentation on the SQLite
//! effect journal, however long the live call took.
//!
//! `complete_tool_call` journals the settled call's presentation as a
//! `PresentToolResult` effect keyed by `{call_id}:present`, and a redrive
//! re-issues it for the journal to serve. How long the call took is an
//! observation, not a recorded fact: a redriven call serves its journaled
//! attempt at once, or re-runs an orchestrating body against its recorded
//! effects, so its settled duration differs from the live one. The journal
//! compares the redriven envelope with the recorded one, so a duration in the
//! envelope would refuse a healthy redrive with a replay hash conflict — and
//! the model would be shown that conflict in place of the tool's result.

use serde_json::json;

use crate::support::prelude::*;

const CALL_ID: &str = "slow-call";

/// A context over the backend host's own controller for one turn: each call
/// builds a fresh one, as a redrive does.
fn turn_context(
    backend: &lash_sqlite_store::SqliteBackend,
) -> crate::RuntimeExecutionContext<'static> {
    let controller = backend
        .effect_host()
        .scoped_static(crate::AdmittedScope::turn(
            "presentation-session",
            "presentation-turn",
        ))
        .expect("the turn scope validates")
        .expect("the backend host lends a static controller");
    crate::testing::TestExecutionContextBuilder::for_backend(backend)
        .session_id("presentation-session")
        .borrowed_effect_controller(controller)
        .build()
        .into_runtime()
}

/// The settled call, with the duration this pass observed.
fn settled(duration_ms: u64) -> crate::tool_dispatch::ToolDispatchOutcome {
    crate::tool_dispatch::ToolDispatchOutcome {
        record: crate::ToolCallRecord {
            call_id: Some(CALL_ID.to_string()),
            tool: "slow".to_string(),
            args: json!({}),
            output: crate::ToolCallOutput::success(json!({ "slow": "result" })),
            duration_ms,
        },
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
        captures: Vec::new(),
        triggers: Vec::new(),
    }
}

#[tokio::test]
async fn a_redriven_call_replays_its_presentation_whatever_its_duration() {
    let backend = crate::support::memory_backend().await;

    // The live pass: the call took 46 ms.
    let live = turn_context(&backend)
        .complete_tool_call(CALL_ID.to_string(), None, settled(46))
        .await
        .expect("the live call presents");
    // The redrive: the journaled attempt is served at once.
    let redriven = turn_context(&backend)
        .complete_tool_call(CALL_ID.to_string(), None, settled(2))
        .await
        .expect("the redriven call is served its recorded presentation");

    assert_eq!(
        redriven.completed.model_return, live.completed.model_return,
        "the redrive is served the recorded presentation"
    );
    assert!(
        redriven.completed.output.is_success(),
        "the redriven call keeps its settled output"
    );
}
