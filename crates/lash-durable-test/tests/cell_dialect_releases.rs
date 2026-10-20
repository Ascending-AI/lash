//! Cells of both dialects save in one store (FIG-6186), on a served core
//! over the durable store.
//!
//! Each dialect has its own helper release line, and the two lines name
//! their releases alike (`1.0`). A cell's first save pins the release its
//! dialect writes, once per store. A core serving TypeScript cells runs a
//! cell that saves at its host call; the node is replaced by a core
//! serving Python cells over the same database, whose cell saves at its
//! own host call. The second save pins its dialect's release beside the
//! first, never in its place, and both turns answer.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::Arc;
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::{ExecutionPolicy, ToolCall, ToolOutcome};
use serde_json::json;

use served::{Tier, World};

/// The host call each cell makes: a `Repeatable` echo, so the cell saves
/// before it runs.
struct Echo;

#[async_trait::async_trait]
impl StaticToolExecute for Echo {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        ToolOutcome::ok(json!({ "echo": call.args["x"].clone() })).into()
    }
}

fn echo() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "ext_echo",
        "tools.ext_echo",
        "Answers its argument.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" } }
        }),
        json!({ "type": "object" }),
    )
    .expect("the echo tool's schemas")
    .with_execution(Duration::from_secs(120))
    .with_execution_policy(ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(1).unwrap(),
        0,
        0,
    ))
}

/// A core whose cells are written in `dialect`.
fn builder(backend: &lash::Backend, dialect: lash::rlm::CellDialect) -> lash::LashCoreBuilder {
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(10_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        dialect,
    )
    .with_worker_service(sim::untimed_workers());
    lash::LashCore::rlm_builder(backend.clone(), factory).tools(
        lash::tool_provider_key!("tools"),
        Arc::new(StaticToolProvider::new(vec![echo()], Echo)),
    )
}

/// The model's one cell, `source` in `dialect`'s tag.
fn cell(dialect: &str, source: &str) -> Vec<lash_core::llm::types::LlmResponse> {
    vec![served::response(vec![lash_core::LlmOutputPart::Text {
        text: format!("<{dialect}>\n{source}\n</{dialect}>"),
        response_meta: None,
    }])]
}

/// RELEASE-PIN-PER-DIALECT: a store whose TypeScript cell pinned its
/// release saves a Python cell, which pins its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_python_cell_saves_in_a_store_a_typescript_cell_pinned_on_sqlite_memory() {
    let mut world = World::new(Tier::SqliteMemory, |backend| {
        builder(backend, lash::rlm::CellDialect::typescript())
    })
    .await
    .expect("SQLite needs no server");
    let typescript = world
        .run(
            "typescript cell",
            served::spec(8),
            cell(
                "typescript",
                "const r = await tools.extEcho({ x: 1 });\nconsole.log(r.echo);",
            ),
        )
        .await;
    served::assert_answered("the TypeScript cell", &typescript);
    world
        .restart("python-boot", |backend| {
            builder(backend, lash::rlm::CellDialect::python())
        })
        .await;
    let python = world
        .run(
            "python cell",
            served::spec(8),
            cell(
                "python",
                "r = await tools.ext_echo({\"x\": 2})\nprint(r[\"echo\"])",
            ),
        )
        .await;
    served::assert_answered("the Python cell", &python);
    world.shutdown().await;
}
