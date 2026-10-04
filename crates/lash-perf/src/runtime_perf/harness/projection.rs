//! Benchmark `BenchmarkRuntime` state seeding for the runtime perf harness:
//! historical message seeds, the RLM session projection install, and the
//! projected RLM bindings block, moved verbatim out of `harness.rs` so the
//! budget stays at the #1512 style seam.

use lash::messages::MessageRole;
use lash::plugins::PluginMessage;
use tokio_util::sync::CancellationToken;

use crate::runtime_perf::scenarios::RuntimePerfScenario;

use super::{BenchmarkRuntime, HISTORY_EXCHANGES, validate_runtime_perf_turn};

#[expect(
    clippy::expect_used,
    reason = "the serialized benchmark messages are appended through the benchmark runtime's admin session, taken by set_up"
)]
pub(crate) async fn seed_runtime_state(
    runtime: &mut BenchmarkRuntime,
    scenario: RuntimePerfScenario,
) -> anyhow::Result<()> {
    let mut messages = Vec::with_capacity(HISTORY_EXCHANGES * 2);
    for index in 0..HISTORY_EXCHANGES {
        messages.push(PluginMessage::text(
            MessageRole::User,
            format!(
                "Historical user turn {index}: trace the performance-sensitive path through runtime/session graph/tool prep."
            ),
        ));
        messages.push(PluginMessage::text(
            MessageRole::Assistant,
            format!(
                "Historical assistant turn {index}: inspected runtime.rs, turn_runner.rs, and token ledger export surfaces."
            ),
        ));
    }

    runtime
        .session
        .as_ref()
        .expect("benchmark session")
        .admin()
        .state()
        .append_messages(messages)
        .await
        .map_err(|err| anyhow::anyhow!("seed historical messages: {err}"))?;

    if matches!(scenario, RuntimePerfScenario::RlmGlobals) {
        install_rlm_session_projection(runtime).await?;
    }

    Ok(())
}

#[expect(
    clippy::expect_used,
    reason = "the benchmark runtime's admin session is taken by set_up before the RLM session projection is applied"
)]
async fn install_rlm_session_projection(runtime: &mut BenchmarkRuntime) -> anyhow::Result<()> {
    runtime
        .session
        .as_ref()
        .expect("benchmark session")
        .admin()
        .state()
        .append_session_nodes(lash::plugins::AppendSessionNodesRequest {
            operation_id: "runtime-perf-rlm-projection".into(),
            nodes: lash_protocol_rlm::rlm_seed_initial_nodes(rlm_perf_projected_seed(
                RuntimePerfScenario::RlmGlobals,
                0,
            )?),
            requires_ancestor_node_id: None,
        })
        .await?;
    let turn_input =
        lash::TurnInput::text("Seed current working variables, then finish the benchmark marker.");
    let turn = runtime
        .run_turn(turn_input, CancellationToken::new())
        .await?;
    validate_runtime_perf_turn(RuntimePerfScenario::RlmGlobals, 0, &turn)?;
    runtime.await_background_work().await?;
    Ok(())
}

fn rlm_perf_projected_seed(
    scenario: RuntimePerfScenario,
    turn_index: usize,
) -> anyhow::Result<lash_protocol_rlm::RlmSeed> {
    let projected = |value| {
        serde_json::json!({
            "__projected__": {"kind": "materialized", "value": value},
        })
    };
    lash_protocol_rlm::RlmSeed::from_seed_value(&serde_json::json!({
        "benchmark": projected(serde_json::json!({
            "name": "runtime_perf", "scenario": scenario.name(),
        })),
        "input": projected(serde_json::json!({
            "turn": turn_index + 1,
            "goal": "measure runtime overhead across a longer same-session chat",
            "path": "crates/lash/src/runtime",
        })),
        "chat": projected(serde_json::json!({
            "turn_count": turn_index + 1, "scenario": scenario.name(), "mode": "runtime_perf",
        })),
    }))
    .map_err(anyhow::Error::msg)
}
