//! A fixture body reports only the identity it actually has. The controller
//! binds the public logical run to the admitted invocation before releasing it.
use super::{Barrier, BarrierKind, FileBarriers, WorkIdentity};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio::task::{JoinHandle, JoinSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDelivery {
    pub label: String,
    pub call_id: String,
    pub ordinal: u32,
    pub logical_run: String,
    pub completion: serde_json::Value,
}
#[derive(Default)]
struct Bindings {
    runs: Mutex<BTreeMap<String, WorkIdentity>>,
    changed: Notify,
}
pub struct BodyCallbacks {
    /// POST ToolDelivery here. The response is sent after controller release.
    pub endpoint: String,
    bindings: Arc<Bindings>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
}
impl BodyCallbacks {
    pub async fn start(
        listener: std::net::TcpListener,
        directory: PathBuf,
        deadline: Instant,
    ) -> Result<Self> {
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let endpoint = format!("http://{}/BodyEntered", listener.local_addr()?);
        let bindings = Arc::new(Bindings::default());
        let registry = bindings.clone();
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut children = JoinSet::new();
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    Some(result) = children.join_next(), if !children.is_empty() => { result??; }
                    accepted = listener.accept() => {
                        let (mut stream,_) = accepted?;
                        let directory=directory.clone();
                        let bindings=registry.clone();
                        let mut stopped=stopped.clone();
                        children.spawn(async move {
                            tokio::select! {
                                result = delivery(&mut stream,bindings,directory,deadline) => result,
                                _ = stopped.changed() => Ok(()),
                            }
                        });
                    }
                }
            }
            drop(listener);
            while let Some(result) = children.join_next().await {
                result.context("body callback panicked")??;
            }
            Ok(())
        });
        Ok(Self {
            endpoint,
            bindings,
            stop,
            task: Some(task),
        })
    }
    /// Bind from accepted public ingress plus sys_invocation, before a body
    /// receipt becomes controller evidence. A body cannot supply these fields.
    pub fn bind(&self, logical_run: String, work: WorkIdentity) -> Result<()> {
        ensure!(
            !logical_run.is_empty()
                && !work.ingress.is_empty()
                && !work.run.is_empty()
                && !work.segment.is_empty(),
            "callback binding lacks admitted identity"
        );
        ensure!(
            work.call.is_none() && work.ordinal.is_none(),
            "bind a run before its individual calls"
        );
        let mut runs = self
            .bindings
            .runs
            .lock()
            .map_err(|_| anyhow::anyhow!("body binding registry poisoned"))?;
        if let Some(previous) = runs.get(&logical_run) {
            ensure!(
                previous == &work,
                "logical run already has another segment binding"
            );
        } else {
            runs.insert(logical_run, work);
        }
        self.bindings.changed.notify_waiters();
        Ok(())
    }
    pub async fn finish(&mut self) -> Result<()> {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.await.context("callback listener panicked")??;
        }
        let address = self
            .endpoint
            .strip_prefix("http://")
            .context("invalid callback endpoint")?
            .split('/')
            .next()
            .context("callback address missing")?;
        ensure!(
            TcpStream::connect(address).await.is_err(),
            "callback listener leaked"
        );
        Ok(())
    }
}
impl Drop for BodyCallbacks {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
async fn delivery(
    stream: &mut TcpStream,
    bindings: Arc<Bindings>,
    directory: PathBuf,
    deadline: Instant,
) -> Result<()> {
    let result = tokio::time::timeout_at(deadline.into(), async {
        let mut bytes = Vec::new();
        let header_end = loop {
            ensure!(bytes.len() < 65536, "callback request header is oversized");
            let mut chunk = [0; 1024];
            let read = stream.read(&mut chunk).await?;
            ensure!(read > 0, "callback request closed before headers");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(position) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let header = std::str::from_utf8(&bytes[..header_end])?;
        let request = header
            .lines()
            .next()
            .context("callback has no request line")?;
        let parts: Vec<_> = request.split_whitespace().collect();
        ensure!(
            parts.len() == 3 && parts[0] == "POST" && parts[2] == "HTTP/1.1",
            "callback requires a POST JSON request"
        );
        let kind: BarrierKind = serde_json::from_value(serde_json::Value::String(
            parts[1].trim_start_matches('/').to_owned(),
        ))?;
        ensure!(
            !kind.durable(),
            "tool callback cannot certify journal durability"
        );
        ensure!(
            !header
                .lines()
                .any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding:")),
            "chunked fixture callback is unsupported"
        );
        let lengths: Vec<_> = header
            .lines()
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .collect();
        ensure!(lengths.len() == 1, "callback requires one Content-Length");
        let length: usize = lengths[0].1.trim().parse()?;
        ensure!(length <= 65536, "callback body is oversized");
        while bytes.len() < header_end + length {
            let mut chunk = [0; 1024];
            let read = stream.read(&mut chunk).await?;
            ensure!(read > 0, "callback body is incomplete");
            bytes.extend_from_slice(&chunk[..read]);
        }
        let delivery: ToolDelivery =
            serde_json::from_slice(&bytes[header_end..header_end + length])?;
        ensure!(
            !delivery.call_id.is_empty() && delivery.ordinal > 0 && !delivery.label.is_empty(),
            "body lacks actual call identity"
        );
        let mut work = loop {
            let mut changed = Box::pin(bindings.changed.notified());
            changed.as_mut().enable();
            let found = bindings
                .runs
                .lock()
                .map_err(|_| anyhow::anyhow!("body registry poisoned"))?
                .get(&delivery.logical_run)
                .cloned();
            if let Some(work) = found {
                break work;
            }
            changed.await;
        };
        work.call = Some(delivery.call_id.clone());
        work.ordinal = Some(delivery.ordinal);
        let barrier = Barrier { work, kind };
        let receipt = serde_json::json!({"delivery":delivery,"barrier":barrier});
        let data = serde_json::to_vec(&receipt)?;
        let path = directory.join(format!(
            "body-{}.json",
            lash_core::stable_hash::sha256_hex(&data)
        ));
        crate::node::write_atomically(&path, &data)?;
        FileBarriers::new(directory, deadline)?
            .enter(&barrier, path.display().to_string())
            .await
    })
    .await
    .context("body callback missed its binding or release deadline")?;
    let (status, body) = match &result {
        Ok(()) => ("200 OK", String::from("released")),
        Err(error) => ("400 Bad Request", error.to_string()),
    };
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    stream.shutdown().await?;
    result
}
