//! Fresh-process startup, with child clocks kept separate from launch clocks.
use super::{
    FollowerMode,
    provider::{LatencyProviderKind, ProviderTiming},
    runner::{CaseSpec, Topology},
};
use crate::runtime_perf::openai_compat::OpenAiCompatBenchServer;
use anyhow::{Context, Result, ensure};
use lash_core::perf_witness::startup::{self, Marker, Phase, Recorder};
use lash_http_transport::{HttpRequest, HttpResponse, HttpTransport, LlmTransportError};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug)]
struct FirstRequestTransport(lash_http_transport::ReqwestHttpTransport);
#[async_trait::async_trait]
impl HttpTransport for FirstRequestTransport {
    async fn send(
        &self,
        request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        startup::record(Phase::ProviderFirstRequest);
        self.0.send(request, timeout).await
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ChildReceipt {
    pid: u32,
    markers: Vec<Marker>,
}

fn validate(markers: &[Marker]) -> Result<()> {
    let at = |phase| -> Result<u64> {
        let rows: Vec<_> = markers
            .iter()
            .filter(|marker| marker.phase == phase)
            .collect();
        ensure!(
            rows.len() == 1,
            "missing or duplicate startup phase {phase:?}"
        );
        Ok(rows[0].since_entry_ns)
    };
    // CoreBuilt is the retained ready handshake, not a node registration claim.
    for chain in [
        &[
            Phase::ProcessEntry,
            Phase::StoreOpenStarted,
            Phase::StoreOpened,
            Phase::StoreSetupFinished,
            Phase::StoresReady,
            Phase::CoreBuilt,
        ][..],
        &[
            Phase::StoresReady,
            Phase::NodeRegistered,
            Phase::SessionOpened,
            Phase::VmSpawnStarted,
            Phase::VmSpawned,
            Phase::VmReady,
            Phase::ProviderFirstRequest,
            Phase::FirstResult,
        ][..],
        &[Phase::CoreBuilt, Phase::SessionOpened][..],
    ] {
        let values = chain
            .iter()
            .map(|phase| at(*phase))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            values.windows(2).all(|pair| pair[0] <= pair[1]),
            "startup phases out of order"
        );
    }
    ensure!(
        markers
            .windows(2)
            .all(|pair| pair[0].since_entry_ns <= pair[1].since_entry_ns),
        "startup epoch moved backwards"
    );
    ensure!(
        markers
            .iter()
            .all(|marker| marker.observer_pid == std::process::id()),
        "mixed startup observer processes"
    );
    Ok(())
}

async fn measure(stores_dir: &Path, recorder: &Recorder, announce: bool) -> Result<ChildReceipt> {
    std::fs::create_dir_all(stores_dir)?;
    ensure!(
        !stores_dir.join("lash.db").exists(),
        "startup requires a fresh SQLite file"
    );
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        stores_dir.join("lash.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await?;
    startup::record(Phase::StoresReady);
    let server =
        OpenAiCompatBenchServer::start(crate::runtime_perf::providers::BenchmarkStreamProfile {
            full_text: "startup result".into(),
            deltas: vec!["startup result".into()],
            parts: vec![],
        })
        .await?;
    let spec = CaseSpec {
        name: "startup",
        topology: Topology::SameProcess,
        provider: LatencyProviderKind::OpenAiCompat,
        follower: FollowerMode::Standard,
        samples: 0,
        lanes: 0,
        busy: false,
    };
    let compat_server = Some(server);
    let core = super::runner::build_core(
        super::runner::durable_backend(stores)?,
        &spec,
        Arc::new(ProviderTiming::default()),
        None,
        &compat_server,
        Some(Arc::new(FirstRequestTransport(
            lash_http_transport::ReqwestHttpTransport::from_client(
                lash_http_transport::http_client_builder()
                    .no_proxy()
                    .build()?,
            ),
        ))),
    )?;
    startup::record(Phase::CoreBuilt);
    if announce {
        super::runner::announce_ready()?;
    }
    // Registration is asynchronous. Observe it before opening the first session;
    // this wait never drives engine work.
    tokio::time::timeout(Duration::from_secs(30), async {
        while !recorder
            .snapshot()
            .iter()
            .any(|marker| marker.phase == Phase::NodeRegistered)
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .context("startup node registration timed out")?;
    let session = core
        .session(lash::SessionId::try_from("startup-session".to_owned())?)
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                super::runner::latency_llm_profile_spec()?.wire_model,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12)),
        ))
        .await?;
    startup::record(Phase::SessionOpened);
    // The VM helper is a real bounded worker. Spawn/ready are observed by this
    // parent process and retain the helper PID, not a cross-process Instant.
    let entry = lash_vm_client::WorkerEntry::helper(
        std::env::var_os("LASH_VM_WORKER").context("LASH_VM_WORKER is required")?,
    );
    let mut config = lash_vm_client::PoolConfig::standard(entry);
    config.max_workers = 1;
    let pool = lash_vm_client::WorkerPool::new(config)?;
    let handle = session.send(lash::TurnInput::text("first result")).await?;
    let output = tokio::time::timeout(Duration::from_secs(30), handle.output()).await??;
    ensure!(
        matches!(output.result.outcome, lash::TurnOutcome::Finished(_)),
        "startup first turn did not finish"
    );
    startup::record(Phase::FirstResult);
    let markers = recorder.snapshot();
    let result = validate(&markers);
    drop(pool);
    core.shutdown().await?;
    result?;
    Ok(ChildReceipt {
        pid: std::process::id(),
        markers,
    })
}

/// The startup child uses the same readiness line as the latency child.
pub async fn run_worker(stores_dir: &Path, out: &Path, recorder: &Recorder) -> Result<()> {
    let receipt = measure(stores_dir, recorder, true).await?;
    std::fs::write(out, serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

#[derive(Serialize)]
struct StartRow {
    child: ChildReceipt,
    launch_ns: u64,
    ready_observed_ns: u64,
    child_exit_observed_ns: u64,
    launch_to_exit_ns: u64,
}
fn nanos(epoch: Instant) -> u64 {
    epoch.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}
async fn child(stores_dir: &Path, epoch: Instant) -> Result<StartRow> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    std::fs::create_dir_all(stores_dir)?;
    let out = stores_dir.join("startup.json");
    let launch_ns = nanos(epoch);
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .arg("latency-worker")
        .arg("--store-dir")
        .arg(stores_dir)
        .arg("--startup-out")
        .arg(&out)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id().context("startup child pid")?;
    let mut lines = BufReader::new(child.stdout.take().context("startup child stdout")?).lines();
    let ready = tokio::time::timeout(Duration::from_secs(30), lines.next_line()).await??;
    ensure!(
        ready.as_deref() == Some(super::runner::READY_MARKER),
        "startup child did not signal ready"
    );
    let ready_observed_ns = nanos(epoch);
    let status = tokio::time::timeout(Duration::from_secs(60), child.wait()).await??;
    ensure!(status.success(), "startup child failed: {status}");
    let child_exit_observed_ns = nanos(epoch);
    let receipt: ChildReceipt = serde_json::from_slice(&std::fs::read(out)?)?;
    ensure!(
        receipt.pid == pid,
        "startup child receipt belongs to another process"
    );
    // The parent-observed endpoint includes receipt write, shutdown and exit.
    Ok(StartRow {
        child: receipt,
        launch_ns,
        ready_observed_ns,
        child_exit_observed_ns,
        launch_to_exit_ns: child_exit_observed_ns - launch_ns,
    })
}

/// Simultaneous fresh-process waves at widths 1/2/4; never a timing gate.
pub async fn run(out: &Path, stores_dir: &Path) -> Result<()> {
    let mut waves = Vec::new();
    let mut initial = 0.0;
    let mut knee = None;
    for width in [1, 2, 4] {
        let epoch = Instant::now();
        let paths: Vec<_> = (0..width)
            .map(|index| stores_dir.join(format!("width-{width}-{index}")))
            .collect();
        let rows =
            futures_util::future::try_join_all(paths.iter().map(|path| child(path, epoch))).await?;
        let window_ns = nanos(epoch);
        let summary = crate::perf_support::metrics::basic_summary(
            rows.iter()
                .map(|row| {
                    row.child
                        .markers
                        .iter()
                        .find(|m| m.phase == Phase::FirstResult)
                        .map_or(0.0, |m| m.since_entry_ns as f64 / 1e6)
                })
                .collect(),
        );
        if width == 1 {
            initial = summary.p95;
        }
        let ratio = (initial > 0.0).then(|| summary.p95 / initial);
        if width > 1 && knee.is_none() && ratio.is_some_and(|ratio| ratio >= 1.25) {
            knee = Some(width);
        }
        waves.push(serde_json::json!({"simultaneous_starts":width,"completed":rows.len(), "window_ns":window_ns,
            "completed_starts_per_second":width as f64 / (window_ns as f64 / 1e9),
            "child_entry_to_first_result_ms":summary,"p95_vs_single_ratio":ratio,"starts":rows}));
    }
    let receipt = serde_json::json!({"kind":"lash.startup-phases","mode":"functional_noncertifying",
        "cache_policy":"fresh SQLite files; OS page/executable caches uncontrolled; no cache dropping",
        "clock":"one monotonic process-entry epoch per child; one separate parent epoch per wave",
        "configured_tokio_workers_per_process":2,"configured_vm_workers_per_child":1,
        "definitions": {
            "configured_tokio_workers_per_process,configured_vm_workers_per_child":"configured thread and worker counts per process/child; not measured occupancy",
            "configured_knee_ratio":"configured dimensionless p95 growth threshold; not a measured ratio",
            "markers.*.since_entry_ns":"point sample; nanoseconds since instrumented child entry; observer_pid identifies clock; VM markers are parent-observed",
            "launch_ns,ready_observed_ns,child_exit_observed_ns":"point samples; nanoseconds since parent wave start; child_exit_observed includes child shutdown/receipt/exit",
            "launch_to_exit_ns":"one interval in nanoseconds; parent launch through observed successful child exit",
            "window_ns":"one parent wave interval in nanoseconds; wave epoch through all successful exits",
            "child_entry_to_first_result_ms":"milliseconds; each child's entry through committed first send result; min/median/mean/p50/p95/p99/max over starts in this wave; percentiles interpolate rank p*(n-1)",
            "simultaneous_starts,completed":"counts per wave; configured launches and measured successful exits respectively",
            "completed_starts_per_second":"completed count divided by parent wave seconds; finite-wave completion rate",
            "p95_vs_single_ratio":"dimensionless measured ratio; same child first-result quantity versus width 1",
            "diagnostic_knee_width":"first configured width above 1 with p95 ratio >= configured 1.25; null means none observed; shared-host diagnostic, not certified capacity",
            "pid,observer_pid,subject_pid":"process identities, not measurements"
        }, "configured_knee_ratio":1.25,"diagnostic_knee_width":knee,"waves":waves});
    std::fs::write(out, serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_first_use_phases_are_complete_and_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::install().unwrap();
        let receipt = measure(dir.path(), &recorder, false).await.unwrap();
        validate(&receipt.markers).unwrap();
        let spawned = receipt
            .markers
            .iter()
            .find(|m| m.phase == Phase::VmSpawned)
            .unwrap();
        let ready = receipt
            .markers
            .iter()
            .find(|m| m.phase == Phase::VmReady)
            .unwrap();
        assert_ne!(spawned.subject_pid, receipt.pid);
        assert_eq!(spawned.subject_pid, ready.subject_pid);
    }
}
