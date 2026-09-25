//! FIG-3633: an RLM session on a file backend finds its Lashlang artifacts
//! after a restart.
//!
//! The RLM protocol takes its artifact store from the backend it runs on, so
//! the module a cell compiles and publishes lives in the same substrate as
//! the session and the process that runs it. A fresh core over the reopened
//! backend therefore runs that process body from the module the first boot
//! published.

use super::*;

/// A cell that starts a process parked on a timer and reports its id. The
/// timer outlives the first boot, so the process body runs to its end only
/// after the restart.
const START_PARKED_PROCESS: &str = r#"const worker = async () => {
  await sleep(1500);
  return { resumed: "after restart" };
};
const job = await processes.start({ definition: worker });
finish(job.process_id);"#;

/// A core over `backend` whose RLM factory keeps its artifacts in that same
/// backend, as every host builds one; `cells` script the provider.
fn restart_core(
    backend: Arc<lash_sqlite_store::SqliteBackend>,
    cells: Vec<String>,
    owner: &str,
) -> Result<LashCore> {
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        backend.as_ref(),
    );
    LashCore::rlm_builder(backend, crate::TurnBudget::Unbounded, factory)
        .provider(queued_text_provider(cells))
        .model(mock_model_spec())
        .commit_budget(lash_core::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash_core::QueuedWorkBatchingConfig::new(1))
        .lease_timings(short_lease())
        // ADR 0095: `processes` is catalogue presence, so the cell needs this.
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
        ))
        .without_queued_work()
        .build(lash_core::LeaseOwnerIdentity::opaque(
            owner,
            format!("{owner}:incarnation"),
        ))
}

/// A three-second lease: the restarted host reclaims what the dead boot
/// held without waiting out the default TTL.
fn short_lease() -> lash_core::facade_support::LeaseTimings {
    lash_core::facade_support::LeaseTimings::from_ttl(std::time::Duration::from_secs(3))
        .expect("a three-second lease holds three renew intervals")
}

/// The file backend at `root`, its effect leases on [`short_lease`] like the
/// runtime's, so the dead boot's effect claims lapse on the same window.
async fn file_backend(root: &std::path::Path) -> Arc<lash_sqlite_store::SqliteBackend> {
    Arc::new(
        lash_sqlite_store::SqliteBackend::open_with_options_and_clock(
            root,
            lash_sqlite_store::SqliteBackendOptions {
                effect_replay: lash_sqlite_store::SqliteEffectReplayOptions {
                    lease_timings: short_lease(),
                    ..Default::default()
                },
                ..Default::default()
            },
            Arc::new(lash_core::facade_support::SystemClock),
        )
        .await
        .expect("open the file backend"),
    )
}

/// The first boot, on a runtime of its own: the cell compiles the worker,
/// publishes its module into the backend's artifact store and starts it,
/// and the process parks on its timer. Returns the process's id.
async fn first_boot(root: std::path::PathBuf) -> ProcessId {
    let backend = file_backend(&root).await;
    let core = restart_core(
        backend,
        vec![typescript_block(START_PARKED_PROCESS)],
        "rlm-artifacts-first-boot",
    )
    .expect("build the first-boot core");
    let session = core
        .session(SESSION_ID)
        .open()
        .await
        .expect("open the session");
    let started = session
        .send(TurnInput::text("start the parked worker"))
        .output()
        .await
        .expect("run the starting turn");
    let process_id = ProcessId::from(
        started
            .final_value()
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("the cell reports its process id: {started:?}")),
    );
    wait_for_process(
        &core,
        &process_id,
        "the process starts and parks on its timer",
        |process| process.first_started.is_some() && !process.terminal(),
    )
    .await;
    process_id
}

const SESSION_ID: &str = "rlm-artifacts-restart";

#[tokio::test]
async fn an_rlm_session_on_a_file_backend_finds_its_artifacts_after_a_restart() -> Result<()> {
    let dir = tempfile::tempdir().expect("restart law tempdir");

    // The first boot runs on its own runtime, which is shut down afterwards:
    // every task of that process, its in-process worker included, is gone,
    // so only the file backend carries anything into the restart.
    let root = dir.path().to_path_buf();
    let process_id = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build the first-boot runtime");
        let process_id = runtime.block_on(first_boot(root));
        runtime.shutdown_timeout(std::time::Duration::from_secs(5));
        process_id
    })
    .join()
    .expect("the first boot completes");

    // Restart: a fresh core over the same file backend. Its RLM factory's
    // engine loads the worker's module from that backend when the process
    // resumes.
    let backend = file_backend(dir.path()).await;
    let core = restart_core(backend, Vec::new(), "rlm-artifacts-restarted")?;
    let worker =
        lash_core_worker::DurableProcessWorker::new(core.durable_process_worker_config()?)?;

    // The dead boot's worker still holds the process's lease. Once it lapses,
    // the restarted host's worker reclaims the process and re-runs its body
    // from the module the first boot published, through the timer to its end.
    drive_until(
        &core,
        &worker,
        &process_id,
        "the resumed process settles",
        |process| process.terminal(),
    )
    .await?;
    let output = core.processes().await_output(&process_id).await?;
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("the resumed process must run its published module: {output:#?}");
    };
    assert_eq!(
        value.to_json_value(),
        serde_json::json!({ "resumed": "after restart" })
    );
    wait_for_terminal(&core, &process_id, lash_core::ProcessStatus::Completed).await;
    Ok(())
}

/// Drive `worker` until the process matches `done`, failing after 20 s with
/// the process as last observed.
async fn drive_until(
    core: &LashCore,
    worker: &lash_core_worker::DurableProcessWorker,
    process_id: &ProcessId,
    label: &str,
    done: impl Fn(&lash_core::facade_support::ObservedProcess) -> bool,
) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let _drive = worker.drive_pending_processes().await?;
        let observed = core
            .processes()
            .get(process_id)
            .await?
            .expect("the process is registered");
        if done(&observed) {
            return Ok(());
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{label}: timed out, last observed {observed:#?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
