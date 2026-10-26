//! Executable-owned, opt-in Tokio Console server and bounded receipt client.
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use console_api::instrument::{InstrumentRequest, instrument_client::InstrumentClient};
use serde_json::json;
use tracing_subscriber::prelude::*;

pub struct Capture {
    stop: std::sync::mpsc::Sender<()>,
    reader: std::thread::JoinHandle<Result<()>>,
    socket: PathBuf,
}
impl Capture {
    pub fn start() -> Result<Option<Self>> {
        let Some(socket) = std::env::var_os("LASH_CONSOLE_SOCKET") else {
            return Ok(None);
        };
        let socket = PathBuf::from(socket);
        let out = PathBuf::from(
            std::env::var_os("LASH_CONSOLE_OUT").context("LASH_CONSOLE_OUT is required")?,
        );
        let parent = socket
            .parent()
            .context("console socket needs a private parent directory")?;
        ensure!(
            std::fs::metadata(parent)?.permissions().mode() & 0o077 == 0,
            "console socket directory must have mode 0700"
        );
        ensure!(!socket.exists(), "console socket already exists");
        let layer = console_subscriber::ConsoleLayer::builder()
            .server_addr(socket.clone())
            .publish_interval(Duration::from_secs(1))
            .retention(Duration::from_secs(60))
            .spawn();
        tracing_subscriber::registry()
            .with(layer)
            .try_init()
            .map_err(|e| anyhow::anyhow!(e))?;
        let (stop, stopped) = std::sync::mpsc::channel();
        let (ready, connected) = std::sync::mpsc::sync_channel(1);
        let address = socket.clone();
        let reader = std::thread::spawn(move || {
            // The diagnostic client must not observe its own transport tasks.
            let _guard = tracing_subscriber::registry()
                .with(tracing_subscriber::filter::LevelFilter::OFF)
                .set_default();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                let channel = loop {
                    let path = address.clone();
                    let result = tonic::transport::Endpoint::from_static("http://[::]:6669")
                        .connect_with_connector(tower::service_fn(move |_| {
                            let path = path.clone();
                            async move { tokio::net::UnixStream::connect(path).await.map(hyper_util::rt::TokioIo::new) }
                        })).await;
                    match result {
                        Ok(channel) => break channel,
                        Err(error) if tokio::time::Instant::now() >= deadline => return Err(error.into()),
                        Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                    }
                };
                let mut client = InstrumentClient::new(channel);
                let mut updates = client.watch_updates(InstrumentRequest {}).await?.into_inner();
                ready.send(()).map_err(|e| anyhow::anyhow!(e))?;
                let mut tasks = BTreeMap::new();
                let mut stats = BTreeMap::new();
                let mut dropped = [0_u64; 3];
                let mut end = None;
                let mut now = 0;
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(50)), if end.is_none() => {
                            if stopped.try_recv().is_ok() { end = Some(tokio::time::Instant::now() + Duration::from_secs(2)); }
                        },
                        _ = tokio::time::sleep_until(end.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600))), if end.is_some() => break,
                        update = updates.message() => {
                            let update = update?.context("console stream ended before final publication")?;
                            if let Some(t) = update.now { now = ns(t.seconds,t.nanos); }
                            if let Some(update) = update.task_update {
                                dropped[0] += update.dropped_events;
                                for task in update.new_tasks {
                                    if let Some(id) = &task.id { tasks.insert(id.id, task); }
                                }
                                stats.extend(update.stats_update);
                            }
                            if let Some(update) = update.resource_update { dropped[1] += update.dropped_events; }
                            if let Some(update) = update.async_op_update { dropped[2] += update.dropped_events; }
                        }
                    }
                }
                let rows = tasks.into_iter().map(|(id,task)| {
                    let stat = stats.get(&id);
                    let busy = stat.and_then(|s| s.poll_stats.as_ref()).and_then(|s| s.busy_time.as_ref()).map(|v| ns(v.seconds,v.nanos)).unwrap_or(0);
                    let scheduled = stat.and_then(|s| s.scheduled_time.as_ref()).map(|v| ns(v.seconds,v.nanos)).unwrap_or(0);
                    let lifetime = stat.and_then(|s| s.created_at.as_ref()).map(|start| {
                        stat.and_then(|s| s.dropped_at.as_ref()).map(|v| ns(v.seconds,v.nanos)).unwrap_or(now) - ns(start.seconds,start.nanos)
                    });
                    let site = task.location.map(|s| format!("{}:{}:{}",s.file.unwrap_or_default(),s.line.unwrap_or_default(),s.column.unwrap_or_default()));
                    json!({"task_id":id, "spawn_site":site, "busy_ns":busy,
                        "scheduled_ns":scheduled,"idle_ns":lifetime.map(|v| (v-busy-scheduled).max(0)),
                        "completed":stat.is_some_and(|s| s.dropped_at.is_some()),
                        "stats_present":stat.is_some()})
                }).collect::<Vec<_>>();
                ensure!(!rows.is_empty(), "no Tokio task metadata; build with console feature and -c kiln.console=true");
                std::fs::write(out, format!("{}\n", serde_json::to_string_pretty(&json!({
                    "kind":"lash.tokio-console", "publish_seconds":1, "completed_retention_seconds":60,
                    "dropped_task_events":dropped[0], "dropped_resource_events":dropped[1],
                    "dropped_async_op_events":dropped[2], "tasks":rows,
                    "limits":"poll elapsed is not CPU; live in-progress polls/scheduled intervals are incomplete; host process only"
                }))?))?;
                Ok(())
            })
        });
        connected
            .recv_timeout(Duration::from_secs(12))
            .context("console client did not attach")?;
        Ok(Some(Self {
            stop,
            reader,
            socket,
        }))
    }
    pub fn finish(self) -> Result<()> {
        let _ = self.stop.send(());
        let result = self
            .reader
            .join()
            .map_err(|_| anyhow::anyhow!("console reader panicked"))?;
        std::fs::remove_file(self.socket)?;
        result
    }
}
fn ns(seconds: i64, nanos: i32) -> i128 {
    i128::from(seconds) * 1_000_000_000 + i128::from(nanos)
}
