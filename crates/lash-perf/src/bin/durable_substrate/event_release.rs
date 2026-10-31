//! A release window over actual retained process history, seeded before counting.

use anyhow::{Result, bail, ensure};
use lash_execution_contracts::process::{
    LifetimeDecision, ProcessEventAppendRequest, ProcessProvenance, ProcessRegistrar as _,
    ProcessRegistration, ProcessRegistryTestSupport as _, ProcessRetention as _, WaitKind,
    WaitState,
};
use lash_perf::perf_support::time::instruction_control;
use lash_protocol_types::ToolCallId;
use lash_vocabulary::tool_contract::ToolId;
use serde_json::json;

use crate::Run;
use crate::deploy::Database;

pub async fn run(run: &Run<'_>) -> Result<()> {
    const EVENTS: usize = 2048;
    const PAYLOAD_BYTES: usize = 4096;
    ensure!(
        run.samples == 1,
        "the release witness uses one owner history"
    );
    let Database::Sqlite(path) = &run.database else {
        bail!("the event-release witness uses SQLite");
    };
    let stores =
        lash_sqlite_store::SqliteStoreSet::open(path, lash_sqlite_store::SqliteSynchronous::Full)
            .await?;
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    let registry = stores.process_registry();
    let process = registry
        .register_process(ProcessRegistration::new(
            lash_execution_contracts::testing::held_engine_input(json!({"release": EVENTS})),
            ProcessProvenance::host(),
            LifetimeDecision::Detached,
        ))
        .await?
        .id;
    let requests = (0..EVENTS)
        .map(|index| {
            let wait = WaitState {
                since_ms: 1,
                kind: WaitKind::Call {
                    resume_at_ms: None,
                    call_id: ToolCallId::fixture(&format!(
                        "release-{index}-{}",
                        "x".repeat(PAYLOAD_BYTES)
                    )),
                    tool_id: ToolId::new("event-release"),
                },
                site: None,
            };
            ProcessEventAppendRequest::wait_entered(&process, &wait)
                .with_replay_key(format!("event-release-{index}"))
        })
        .collect();
    let receipts = registry
        .append_events_for_testing(&process, requests)
        .await?;
    ensure!(receipts.len() == EVENTS, "the owner history was not seeded");
    let through = receipts.last().map_or(0, |receipt| receipt.event.sequence);
    drop(receipts);
    let controlled = std::env::var_os("LASH_PERF_CONTROL").is_some();
    instruction_control("enable")?;
    let released = registry.release_process_events(&process, through).await;
    instruction_control("disable")?;
    let released = released?;
    ensure!(
        released.released_events == EVENTS as u64 && released.released_through == through,
        "the complete event prefix was not released"
    );
    run.report.write(
        "event-release",
        &json!({
            "events": EVENTS,
            "payload_bytes_per_call_id": PAYLOAD_BYTES,
            "released_events": released.released_events,
            "released_through": released.released_through,
            "control_acknowledgements": if controlled {
                vec!["enable", "disable"]
            } else {
                Vec::<&str>::new()
            },
        }),
    )?;
    Ok(())
}
