//! Real H4 fleet selections over the reusable owned scenario runner.
use anyhow::Result;
use lash_upgrade_harness::node::fleet::scenario::{Scenario, run};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s14_leader_loss_reuses_durable_x_and_original_run() -> Result<()> {
    run(Scenario::LeaderLoss).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s15_minority_tool_cancel_race_has_one_winner_after_heal() -> Result<()> {
    run(Scenario::MinorityPartition).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s16_in_flight_terminal_blocks_drain_and_disconnect_keeps_fence() -> Result<()> {
    run(Scenario::TerminalPublication).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s16_sigkill_redrives_same_invocation_without_a_second_seal() -> Result<()> {
    run(Scenario::TerminalRedrive).await
}
