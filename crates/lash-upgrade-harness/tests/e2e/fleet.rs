//! Real H4 fleet selections over the reusable owned scenario runner.
use anyhow::Result;
use lash_upgrade_harness::e2e::case::{Leg, Permutation, StoreKind};
use lash_upgrade_harness::node::fleet::scenario::{Scenario, run};

macro_rules! fleet_case {
    ($name:ident, $scenario:ident, $leg:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() -> Result<()> {
            run(
                Scenario::$scenario,
                Permutation::provisioned(StoreKind::PostgreSql, Leg::$leg)?,
            )
            .await
        }
    };
}

fleet_case!(
    s14_leader_loss_reuses_durable_x_and_original_run,
    LeaderLoss,
    Live
);
fleet_case!(
    s15_minority_tool_cancel_race_has_one_winner_after_heal,
    MinorityPartition,
    Live
);
fleet_case!(
    s16_in_flight_terminal_blocks_drain_and_disconnect_keeps_fence,
    TerminalPublication,
    Live
);
fleet_case!(
    s16_in_flight_terminal_blocks_drain_and_disconnect_keeps_fence_replay,
    TerminalPublication,
    Replay
);
fleet_case!(
    s16_sigkill_redrives_same_invocation_without_a_second_seal,
    TerminalRedrive,
    Live
);
fleet_case!(
    s16_sigkill_redrives_same_invocation_without_a_second_seal_replay,
    TerminalRedrive,
    Replay
);
