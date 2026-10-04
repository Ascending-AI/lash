//! Stream-local holds over original HTTP/2 bytes. Header blocks keep their
//! connection-wide compression order; each block is written without interleaving.
use super::super::{Barrier, BarrierProof, FileBarriers};
use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{Mutex, mpsc, oneshot};

pub(super) enum Hold {
    Enter(Barrier, String),
    Release(Barrier),
}
pub(super) struct Frame {
    pub wire: Vec<u8>,
    pub holds: Vec<Hold>,
    pub publish: Vec<BarrierProof>,
    pub header_block: bool,
    pub end_stream: bool,
}
struct Queued {
    frame: Frame,
    previous_header: Option<oneshot::Receiver<()>>,
    header_done: Option<oneshot::Sender<()>>,
}

pub(super) struct Forwarder {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    barriers: Arc<FileBarriers>,
    streams: BTreeMap<u32, mpsc::UnboundedSender<Queued>>,
    workers: FuturesUnordered<BoxFuture<'static, Result<()>>>,
    previous_header: Option<oneshot::Receiver<()>>,
    buffered: Arc<AtomicUsize>,
}
impl Forwarder {
    pub fn new(writer: OwnedWriteHalf, barriers: FileBarriers) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
            barriers: Arc::new(barriers),
            streams: BTreeMap::new(),
            workers: FuturesUnordered::new(),
            previous_header: None,
            buffered: Arc::new(AtomicUsize::new(0)),
        }
    }
    pub fn has_workers(&self) -> bool {
        !self.workers.is_empty()
    }
    pub async fn next_worker(&mut self) -> Result<()> {
        if let Some(result) = self.workers.next().await {
            result?;
        }
        Ok(())
    }
    pub fn push(&mut self, stream: u32, frame: Frame) -> Result<()> {
        let end_stream = frame.end_stream;
        let mut queued = Queued {
            frame,
            previous_header: None,
            header_done: None,
        };
        if queued.frame.header_block {
            let (done, ready) = oneshot::channel();
            queued.previous_header = self.previous_header.replace(ready);
            queued.header_done = Some(done);
        }
        let bytes = queued.frame.wire.len();
        let prior = self.buffered.fetch_add(bytes, Ordering::Relaxed);
        ensure!(
            prior + bytes <= 64 * 1024 * 1024,
            "held HTTP/2 streams exceeded the connection buffer limit"
        );
        let sender = self.streams.entry(stream).or_insert_with(|| {
            let (sender, mut receiver) = mpsc::unbounded_channel::<Queued>();
            let writer = self.writer.clone();
            let barriers = self.barriers.clone();
            let buffered = self.buffered.clone();
            self.workers.push(Box::pin(async move {
                while let Some(queued) = receiver.recv().await {
                    let bytes = queued.frame.wire.len();
                    forward(&writer, &barriers, queued).await?;
                    buffered.fetch_sub(bytes, Ordering::Relaxed);
                }
                Ok(())
            }));
            sender
        });
        sender
            .send(queued)
            .map_err(|_| anyhow::anyhow!("HTTP/2 stream forwarding stopped"))?;
        if end_stream {
            self.streams.remove(&stream);
        }
        Ok(())
    }
    pub async fn finish(mut self) -> Result<()> {
        self.streams.clear();
        while self.has_workers() {
            self.next_worker().await?;
        }
        Ok(())
    }
}
// These futures belong to the relay itself. Dropping the connection drops
// every queue, held frame and socket owner synchronously; no tasks survive it.
async fn forward(
    writer: &Mutex<OwnedWriteHalf>,
    barriers: &FileBarriers,
    queued: Queued,
) -> Result<()> {
    if let Some(previous) = queued.previous_header {
        previous
            .await
            .context("preceding HTTP/2 header block was not forwarded")?;
    }
    for hold in queued.frame.holds {
        match hold {
            Hold::Enter(barrier, artifact) => barriers.enter(&barrier, artifact).await?,
            Hold::Release(barrier) => barriers.await_release(&barrier).await?,
        }
    }
    writer.lock().await.write_all(&queued.frame.wire).await?;
    if let Some(done) = queued.header_done {
        let _ = done.send(());
    }
    for proof in queued.frame.publish {
        barriers.publish(&proof)?;
    }
    Ok(())
}
