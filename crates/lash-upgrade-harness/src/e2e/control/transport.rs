//! Transparent HTTP/2 plumbing for the real V7 service stream. The final DATA
//! fragment of a proposal can be held before Restate receives a whole result;
//! an ACK cut instead holds the actual matching V7 ACK sent by Restate.
use super::{Barrier, BarrierKind, FileBarriers};
use anyhow::{Context, Result, ensure};
use lash_restate_test::protocol::{
    FrameDecoder, MessageType,
    generated::{
        ProposeRunCompletionAckMessage, ProposeRunCompletionMessage, propose_run_completion_message,
    },
};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

pub struct V7Proxy {
    pub endpoint: String,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
}
#[derive(Clone)]
pub struct TransportCut {
    pub proposal: Barrier,
    pub before_ack: Barrier,
}
impl V7Proxy {
    pub async fn start(
        listener: std::net::TcpListener,
        upstream: SocketAddr,
        directory: PathBuf,
        deadline: Instant,
        cuts: Vec<TransportCut>,
    ) -> Result<Self> {
        for cut in &cuts {
            ensure!(
                matches!(
                    cut.proposal.kind,
                    BarrierKind::XProposed | BarrierKind::DProposed | BarrierKind::VProposed
                ),
                "invalid transport proposal cut"
            );
            ensure!(
                cut.before_ack.kind == BarrierKind::BeforeAck
                    && cut.before_ack.work == cut.proposal.work,
                "ACK cut has another work identity"
            );
        }
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut children = JoinSet::new();
            let mut connection = 0;
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    Some(result) = children.join_next(), if !children.is_empty() => { result??; }
                    accepted = listener.accept() => {
                        let (client, _) = accepted?;
                        let server = TcpStream::connect(upstream).await?;
                        connection += 1;
                        let directory = directory.clone();
                        let cuts = cuts.clone();
                        let mut stopped = stopped.clone();
                        children.spawn(async move {
                            let (client_read, client_write) = client.into_split();
                            let (server_read, server_write) = server.into_split();
                            let completions = Arc::new(Mutex::new(BTreeMap::new()));
                            let incoming = relay(client_read,server_write,true,connection,&directory,deadline,&cuts,completions.clone());
                            let outgoing = relay(server_read,client_write,false,connection,&directory,deadline,&cuts,completions);
                            tokio::select! {
                                result = async { tokio::try_join!(incoming,outgoing)?; Ok::<_,anyhow::Error>(()) } => result,
                                _ = stopped.changed() => Ok(()),
                            }
                        });
                    }
                }
            }
            drop(listener);
            while let Some(result) = children.join_next().await {
                result.context("V7 proxy connection panicked")??;
            }
            Ok(())
        });
        Ok(Self {
            endpoint,
            stop,
            task: Some(task),
        })
    }
    pub async fn finish(&mut self) -> Result<()> {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.await.context("V7 proxy panicked")??;
        }
        ensure!(
            TcpStream::connect(self.endpoint.trim_start_matches("http://"))
                .await
                .is_err(),
            "V7 proxy listener leaked"
        );
        Ok(())
    }
}
impl Drop for V7Proxy {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
#[allow(clippy::too_many_arguments)]
async fn relay(
    mut input: tokio::net::tcp::OwnedReadHalf,
    mut output: tokio::net::tcp::OwnedWriteHalf,
    to_host: bool,
    connection: u64,
    directory: &std::path::Path,
    deadline: Instant,
    cuts: &[TransportCut],
    completions: Arc<Mutex<BTreeMap<(u32, u32), Barrier>>>,
) -> Result<()> {
    if to_host {
        let mut preface = [0; 24];
        input.read_exact(&mut preface).await?;
        ensure!(
            &preface == b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
            "V7 barrier requires HTTP/2 transport"
        );
        output.write_all(&preface).await?;
    }
    let barriers = FileBarriers::new(directory.to_owned(), deadline)?;
    let mut decoders: BTreeMap<u32, FrameDecoder> = BTreeMap::new();
    let mut observation = 0;
    loop {
        let mut header = [0; 9];
        match input.read_exact(&mut header).await {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
        let length =
            (usize::from(header[0]) << 16) | (usize::from(header[1]) << 8) | usize::from(header[2]);
        ensure!(length <= 16 * 1024 * 1024, "oversized HTTP/2 frame");
        let mut payload = vec![0; length];
        input.read_exact(&mut payload).await?;
        let stream = u32::from_be_bytes(header[5..9].try_into()?) & 0x7fff_ffff;
        if header[3] == 0 && !payload.is_empty() {
            let data = if header[4] & 8 != 0 {
                let padding = usize::from(payload[0]);
                ensure!(padding < payload.len(), "invalid HTTP/2 padding");
                &payload[1..payload.len() - padding]
            } else {
                &payload[..]
            };
            let decoder = decoders.entry(stream).or_default();
            decoder.push(data);
            for frame in decoder.drain()? {
                observation += 1;
                let artifact = directory.join(format!(
                    "wire-{connection}-{stream}-{}-{observation}.json",
                    if to_host { "in" } else { "out" }
                ));
                crate::node::write_atomically(
                    &artifact,
                    &serde_json::to_vec(
                        &serde_json::json!({"type":frame.ty.code(),"requested_ack":frame.requested_ack,"payload":frame.payload.to_vec(),"stream":stream}),
                    )?,
                )?;
                if !to_host && frame.ty == MessageType::ProposeRunCompletion {
                    let message: ProposeRunCompletionMessage = frame.decode()?;
                    if let Some(propose_run_completion_message::Result::Value(value)) =
                        message.result
                    {
                        let value: serde_json::Value = serde_json::from_slice(&value)?;
                        let call = value
                            .get("call_id")
                            .and_then(serde_json::Value::as_str)
                            .or_else(|| {
                                value
                                    .pointer("/record/call_id")
                                    .and_then(serde_json::Value::as_str)
                            });
                        for cut in cuts {
                            if call == cut.proposal.work.call.as_deref() && call.is_some() {
                                ensure!(frame.requested_ack, "proposal did not request a V7 ACK");
                                completions
                                    .lock()
                                    .map_err(|_| anyhow::anyhow!("completion registry poisoned"))?
                                    .insert(
                                        (stream, message.result_completion_id),
                                        cut.before_ack.clone(),
                                    );
                                barriers
                                    .enter(&cut.proposal, artifact.display().to_string())
                                    .await?;
                            }
                        }
                    }
                }
                if to_host && frame.ty == MessageType::ProposeRunCompletionAck {
                    let message: ProposeRunCompletionAckMessage = frame.decode()?;
                    let barrier = completions
                        .lock()
                        .map_err(|_| anyhow::anyhow!("completion registry poisoned"))?
                        .get(&(stream, message.completion_id))
                        .cloned();
                    if let Some(barrier) = barrier {
                        barriers
                            .enter(&barrier, artifact.display().to_string())
                            .await?;
                    }
                }
            }
        }
        output.write_all(&header).await?;
        output.write_all(&payload).await?;
    }
}
