//! FIG-5549: a commit to a process's log on one core ticks the process
//! change hub of every other core on the same store, through an every-node
//! `appended` node wake, so a wait on one core never depends on a trace or
//! a poll of its own to learn what another core committed.

use super::*;

use std::time::Duration;

/// FIG-5570: a host's terminal wait wakes on the process change hub, like
/// every other process reader. The core's work cadence polls once an hour;
/// the wait subscribes to the hub and returns at the commit's tick.
#[tokio::test]
async fn a_terminal_wait_wakes_on_the_process_change_hub() {
    let hour = Duration::from_secs(3600);
    let stores = sqlite_memory_store_set().await;
    let core = standard_core_builder_over(lash_conformance::backend_over(stores))
        .work_cadence(crate::WorkCadencePolicy {
            poll_initial: hour,
            poll_max: hour,
        })
        .build(lash_core::LeaseOwnerIdentity::opaque(
            lash_core::LeaseOwnerId::new("fig-5570-waiter"),
            lash_core::LeaseIncarnationId::new("fig-5570-waiter-boot"),
        ))
        .expect("standard core");
    let registry = core.process_registry.clone();
    let process_id = registry
        .register_process(lash_execution_contracts::process::ProcessRegistration::new(
            lash_execution_contracts::testing::held_engine_input(serde_json::Value::Null),
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        ))
        .await
        .expect("register the process")
        .id;

    let waiter = {
        let (core, process_id) = (core.clone(), process_id.clone());
        tokio::spawn(async move { core.processes().await_output(&process_id).await })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while core.process_changes().tracked_processes() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the terminal wait subscribes to the change hub");

    registry
        .complete_process(
            &process_id,
            lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(serde_json::Value::Null),
            ),
            lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(
                &process_id,
            ),
        )
        .await
        .expect("complete the process");
    let output = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("the wait returns at the commit's tick, not on its cadence")
        .expect("the wait task")
        .expect("the terminal is read");
    assert!(matches!(
        output,
        lash_execution_contracts::process::ProcessAwaitOutput::Settled { .. }
    ));
}
