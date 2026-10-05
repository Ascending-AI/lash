//! Transparent HTTP/2 plumbing for the real V7 service stream. The final DATA
//! fragment of a proposal can be held before Restate receives a whole result;
//! an ACK cut instead holds the actual matching V7 ACK sent by Restate.
use super::{Barrier, BarrierKind, FileBarriers, WorkIdentity};
use anyhow::{Context, Result, ensure};
use lash_restate_test::protocol::{
    FrameDecoder, MessageType,
    generated::{
        ProposeRunCompletionAckMessage, ProposeRunCompletionMessage, RunCommandMessage,
        RunCompletionNotificationMessage, SleepCommandMessage, SleepCompletionNotificationMessage,
        StartMessage, propose_run_completion_message, run_completion_notification_message,
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

mod forward;

use forward::{Forwarder, Hold};

pub struct V7Proxy {
    pub endpoint: String,
    stop: watch::Sender<bool>,
    disconnect: watch::Sender<u64>,
    registry: Arc<Mutex<Registry>>,
    upstream: Arc<Mutex<SocketAddr>>,
    task: Option<JoinHandle<Result<()>>>,
}
#[derive(Default)]
struct Registry {
    bindings: BTreeMap<String, WorkIdentity>,
    retries: Vec<Barrier>,
    active: usize,
    publications: Vec<PublicationCut>,
    dynamic_cuts: Vec<TransportCut>,
    starts: Vec<StartHold>,
    commands: Vec<CommandHold>,
}
#[derive(Default)]
struct Connection {
    completions: BTreeMap<(u32, u32), Barrier>,
    invocations: BTreeMap<u32, String>,
    pending_sleeps: BTreeMap<u32, Barrier>,
    run_names: BTreeMap<(u32, u32), String>,
    sleep_gates: BTreeMap<(u32, u32), Barrier>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct TransportCut {
    pub proposal: Barrier,
    pub before_ack: Barrier,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PublicationCut {
    pub invocation: String,
    pub journal_name: String,
    pub proposal: Barrier,
    pub before_ack: Barrier,
}
/// Hold every V7 Start whose invocation key contains `key_fragment` until
/// the barrier is released. Only a Run's own segment invocations carry it.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct StartHold {
    pub key_fragment: String,
    pub barrier: Barrier,
}
/// Hold one invocation's outgoing command of `command`'s type until the
/// barrier is released: what follows it never reaches Restate meanwhile.
#[derive(Clone)]
pub struct CommandHold {
    pub invocation: String,
    pub command: MessageType,
    pub barrier: Barrier,
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
                    BarrierKind::XProposed
                        | BarrierKind::DProposed
                        | BarrierKind::VProposed
                        | BarrierKind::DeclarationIssued
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
        let (disconnect, _) = watch::channel(0_u64);
        let registry = Arc::new(Mutex::new(Registry::default()));
        let registered = registry.clone();
        let upstream = Arc::new(Mutex::new(upstream));
        let target = upstream.clone();
        let stream_disconnect = disconnect.clone();
        let task = tokio::spawn(async move {
            let mut children = JoinSet::new();
            let mut connection = 0;
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    Some(result) = children.join_next(), if !children.is_empty() => { result??; }
                    accepted = listener.accept() => {
                        let (client, _) = accepted?;
                        let address=*target.lock().map_err(|_|anyhow::anyhow!("upstream registry poisoned"))?;
                        let server = match TcpStream::connect(address).await {
                            Ok(server) => server,
                            // A host is intentionally unavailable between incarnations.
                            // Close this accepted connection, retain the reconnect listener.
                            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => continue,
                            Err(error) => return Err(error.into()),
                        };
                        connection += 1;
                        let directory = directory.clone();
                        let cuts = cuts.clone();
                        let mut stopped = stopped.clone();
                        // Only future cuts apply to this newly accepted connection.
                        let mut disconnected=stream_disconnect.subscribe();
                        let registry=registered.clone();
                        registry.lock().map_err(|_|anyhow::anyhow!("transport registry poisoned"))?.active+=1;
                        children.spawn(async move {
                            let session=async {
                                let mut client=client;
                                let mut server=server;
                                let mut preface=[0;24];
                                if let Err(error)=client.read_exact(&mut preface).await { return Err(error.into()); }
                                server.write_all(&preface).await?;
                                if &preface!=b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" {
                                    tokio::io::copy_bidirectional(&mut client,&mut server).await?;
                                    return Ok::<_,anyhow::Error>(());
                                }
                                let (client_read, client_write) = client.into_split();
                                let (server_read, server_write) = server.into_split();
                                let connection_state = Arc::new(Mutex::new(Connection::default()));
                                let incoming = relay(client_read,server_write,true,connection,&directory,deadline,&cuts,connection_state.clone(),registry.clone());
                                let outgoing = relay(server_read,client_write,false,connection,&directory,deadline,&cuts,connection_state,registry.clone());
                                tokio::try_join!(incoming,outgoing)?;
                                Ok(())
                            };
                            let result=tokio::select! {
                                result = session => result,
                                _ = stopped.changed() => Ok(()),
                                _ = disconnected.changed() => Ok(()),
                            };
                            let result=match result {
                                Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error|matches!(error.kind(),std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof)) => Ok(()),
                                other=>other,
                            };
                            registry.lock().map_err(|_|anyhow::anyhow!("transport registry poisoned"))?.active-=1;
                            result
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
            disconnect,
            registry,
            upstream,
            task: Some(task),
        })
    }
    pub fn arm_cut(&self, cut: TransportCut) -> Result<()> {
        ensure!(
            matches!(
                cut.proposal.kind,
                BarrierKind::XProposed
                    | BarrierKind::DProposed
                    | BarrierKind::VProposed
                    | BarrierKind::DeclarationIssued
            ),
            "unsupported completion phase"
        );
        ensure!(
            cut.before_ack.kind == BarrierKind::BeforeAck
                && cut.before_ack.work == cut.proposal.work,
            "ACK cut has another work identity"
        );
        self.registry
            .lock()
            .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
            .dynamic_cuts
            .push(cut);
        Ok(())
    }
    pub fn arm_publication(&self, cut: PublicationCut) -> Result<()> {
        ensure!(
            !cut.invocation.is_empty() && !cut.journal_name.is_empty(),
            "publication cut needs actual invocation and Run slot"
        );
        ensure!(
            cut.proposal.kind == BarrierKind::PublicationRequest
                && cut.before_ack.kind == BarrierKind::BeforeAck
                && cut.proposal.work == cut.before_ack.work,
            "publication phases or work identity disagree"
        );
        self.registry
            .lock()
            .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
            .publications
            .push(cut);
        Ok(())
    }
    pub fn arm_start_hold(&self, hold: StartHold) -> Result<()> {
        ensure!(
            !hold.key_fragment.is_empty() && hold.barrier.kind == BarrierKind::SuccessorStarting,
            "start hold needs an actual Run key and the successor-start phase"
        );
        self.registry
            .lock()
            .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
            .starts
            .push(hold);
        Ok(())
    }
    pub fn arm_command_hold(&self, hold: CommandHold) -> Result<()> {
        ensure!(
            !hold.invocation.is_empty() && !hold.barrier.kind.durable(),
            "command hold needs an actual invocation and a non-durable phase"
        );
        self.registry
            .lock()
            .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
            .commands
            .push(hold);
        Ok(())
    }
    pub fn bind_invocation(&self, invocation: String, work: WorkIdentity) -> Result<()> {
        ensure!(
            !invocation.is_empty() && !work.run.is_empty() && !work.segment.is_empty(),
            "transport binding lacks admitted invocation"
        );
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?;
        if let Some(prior) = registry.bindings.get(&invocation) {
            ensure!(prior == &work, "invocation already bound to another run");
        } else {
            registry.bindings.insert(invocation, work);
        }
        Ok(())
    }
    /// Observe the forwarded Sleep command after this call's retry schedule;
    /// a held barrier delays its matching wake notification. Confirm the actual
    /// pending timer independently through RestateView::durable_sleep.
    /// Ordinal names the failed attempt, as that schedule does.
    pub fn arm_retry(&self, barrier: Barrier) -> Result<()> {
        ensure!(
            barrier.kind == BarrierKind::RetryBackoffEntered
                && barrier.work.call.is_some()
                && barrier.work.ordinal.is_some(),
            "retry cut needs a failed attempt identity"
        );
        self.registry
            .lock()
            .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
            .retries
            .push(barrier);
        Ok(())
    }
    /// Drop owned established streams while retaining the listener for reconnect.
    pub async fn disconnect(&self, deadline: Instant) -> Result<usize> {
        let count = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
            .active;
        ensure!(count > 0, "connection cut missed every established stream");
        self.disconnect.send_modify(|epoch| *epoch += 1);
        loop {
            if self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
                .active
                == 0
            {
                return Ok(count);
            }
            ensure!(
                Instant::now() < deadline,
                "disconnected transport tasks did not close"
            );
            tokio::task::yield_now().await;
        }
    }
    pub fn replace_upstream(&self, next: SocketAddr) -> Result<SocketAddr> {
        let mut upstream = self
            .upstream
            .lock()
            .map_err(|_| anyhow::anyhow!("upstream registry poisoned"))?;
        ensure!(*upstream != next, "redeployment did not change upstream");
        Ok(std::mem::replace(&mut *upstream, next))
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
    output: tokio::net::tcp::OwnedWriteHalf,
    to_host: bool,
    connection: u64,
    directory: &std::path::Path,
    deadline: Instant,
    cuts: &[TransportCut],
    connection_state: Arc<Mutex<Connection>>,
    registry: Arc<Mutex<Registry>>,
) -> Result<()> {
    let mut forwarder = Forwarder::new(output, FileBarriers::new(directory.to_owned(), deadline)?);
    let mut decoders: BTreeMap<u32, FrameDecoder> = BTreeMap::new();
    let mut observation = 0;
    let mut non_sdk = std::collections::BTreeSet::new();
    loop {
        // Retain one read future while reaping finished workers: cancelling a
        // partially read HTTP/2 header would lose original connection bytes.
        let read = read_http2(&mut input);
        tokio::pin!(read);
        let frame = loop {
            tokio::select! {
                frame = &mut read => break frame?,
                result = forwarder.next_worker(), if forwarder.has_workers() => { result?; }
            }
        };
        let Some((header, payload, wire)) = frame else {
            return forwarder.finish().await;
        };
        let stream = u32::from_be_bytes(header[5..9].try_into()?) & 0x7fff_ffff;
        let mut holds = Vec::new();
        let mut entered_sleeps = Vec::new();
        if header[3] == 0 && !payload.is_empty() && !non_sdk.contains(&stream) {
            let data = if header[4] & 8 != 0 {
                let padding = usize::from(payload[0]);
                ensure!(padding < payload.len(), "invalid HTTP/2 padding");
                &payload[1..payload.len() - padding]
            } else {
                &payload[..]
            };
            let decoder = decoders.entry(stream).or_default();
            decoder.push(data);
            let frames = match decoder.drain() {
                Ok(frames) => frames,
                Err(error) => {
                    ensure!(
                        !connection_state
                            .lock()
                            .map_err(|_| anyhow::anyhow!("connection state poisoned"))?
                            .invocations
                            .contains_key(&stream),
                        "active SDK stream is malformed: {error}"
                    );
                    non_sdk.insert(stream);
                    Vec::new()
                }
            };
            for frame in frames {
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
                if to_host && frame.ty == MessageType::Start {
                    let message: StartMessage = frame.decode()?;
                    for hold in registry
                        .lock()
                        .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
                        .starts
                        .iter()
                        .filter(|hold| message.key.contains(&hold.key_fragment))
                    {
                        holds.push(Hold::Enter(
                            hold.barrier.clone(),
                            artifact.display().to_string(),
                        ));
                    }
                    connection_state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("connection state poisoned"))?
                        .invocations
                        .insert(stream, message.debug_id);
                }
                if !to_host {
                    let invocation = connection_state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("connection state poisoned"))?
                        .invocations
                        .get(&stream)
                        .cloned();
                    for hold in registry
                        .lock()
                        .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
                        .commands
                        .iter()
                        .filter(|hold| {
                            hold.command == frame.ty
                                && invocation.as_ref() == Some(&hold.invocation)
                        })
                    {
                        holds.push(Hold::Enter(
                            hold.barrier.clone(),
                            artifact.display().to_string(),
                        ));
                    }
                }
                if !to_host && frame.ty == MessageType::RunCommand {
                    let message: RunCommandMessage = frame.decode()?;
                    connection_state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("connection state poisoned"))?
                        .run_names
                        .insert((stream, message.result_completion_id), message.name);
                }
                if !to_host && frame.ty == MessageType::ProposeRunCompletion {
                    let message: ProposeRunCompletionMessage = frame.decode()?;
                    let selected = {
                        let state = connection_state
                            .lock()
                            .map_err(|_| anyhow::anyhow!("connection state poisoned"))?;
                        let registry = registry
                            .lock()
                            .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?;
                        registry
                            .publications
                            .iter()
                            .filter(|cut| {
                                state.invocations.get(&stream) == Some(&cut.invocation)
                                    && state.run_names.get(&(stream, message.result_completion_id))
                                        == Some(&cut.journal_name)
                            })
                            .cloned()
                            .collect::<Vec<_>>()
                    };
                    for cut in selected {
                        ensure!(frame.requested_ack, "held publication lacks requested ACK");
                        connection_state
                            .lock()
                            .map_err(|_| anyhow::anyhow!("connection state poisoned"))?
                            .completions
                            .insert((stream, message.result_completion_id), cut.before_ack);
                        holds.push(Hold::Enter(cut.proposal, artifact.display().to_string()));
                    }
                }
                let invocation = connection_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("connection state poisoned"))?
                    .invocations
                    .get(&stream)
                    .cloned();
                let work = if let Some(invocation) = invocation {
                    registry
                        .lock()
                        .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
                        .bindings
                        .get(&invocation)
                        .cloned()
                } else {
                    None
                };
                let value = if !to_host && frame.ty == MessageType::ProposeRunCompletion {
                    let message: ProposeRunCompletionMessage = frame.decode()?;
                    if let Some(propose_run_completion_message::Result::Value(bytes)) =
                        message.result
                    {
                        serde_json::from_slice::<serde_json::Value>(&bytes)
                            .ok()
                            .map(|value| (value, Some(message.result_completion_id)))
                    } else {
                        None
                    }
                } else if to_host && frame.ty == MessageType::RunCompletionNotification {
                    let message: RunCompletionNotificationMessage = frame.decode()?;
                    if let Some(run_completion_notification_message::Result::Value(value)) =
                        message.result
                    {
                        serde_json::from_slice::<serde_json::Value>(&value.content)
                            .ok()
                            .map(|value| (value, None))
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let (Some(work), Some((value, completion))) = (&work, value) {
                    let mut active_cuts = cuts.to_vec();
                    active_cuts.extend(
                        registry
                            .lock()
                            .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
                            .dynamic_cuts
                            .clone(),
                    );
                    for cut in &active_cuts {
                        if same_run(work, &cut.proposal.work)
                            && proposal_matches(&value, &cut.proposal)
                            && let Some(completion) = completion
                        {
                            ensure!(frame.requested_ack, "proposal did not request a V7 ACK");
                            connection_state
                                .lock()
                                .map_err(|_| anyhow::anyhow!("connection state poisoned"))?
                                .completions
                                .insert((stream, completion), cut.before_ack.clone());
                            holds.push(Hold::Enter(
                                cut.proposal.clone(),
                                artifact.display().to_string(),
                            ));
                        }
                    }
                    let retries = registry
                        .lock()
                        .map_err(|_| anyhow::anyhow!("transport registry poisoned"))?
                        .retries
                        .clone();
                    for barrier in retries {
                        if same_run(work, &barrier.work) && retry_matches(&value, &barrier) {
                            connection_state
                                .lock()
                                .map_err(|_| anyhow::anyhow!("connection state poisoned"))?
                                .pending_sleeps
                                .insert(stream, barrier);
                        }
                    }
                }
                if !to_host && frame.ty == MessageType::SleepCommand {
                    let message: SleepCommandMessage = frame.decode()?;
                    let mut state = connection_state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("connection state poisoned"))?;
                    if let Some(barrier) = state.pending_sleeps.remove(&stream) {
                        state
                            .sleep_gates
                            .insert((stream, message.result_completion_id), barrier.clone());
                        entered_sleeps.push((barrier, artifact.display().to_string()));
                    }
                }
                if to_host && frame.ty == MessageType::SleepCompletionNotification {
                    let message: SleepCompletionNotificationMessage = frame.decode()?;
                    let barrier = connection_state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("connection state poisoned"))?
                        .sleep_gates
                        .get(&(stream, message.completion_id))
                        .cloned();
                    if let Some(barrier) = barrier {
                        holds.push(Hold::Release(barrier));
                    }
                }
                if to_host && frame.ty == MessageType::ProposeRunCompletionAck {
                    let message: ProposeRunCompletionAckMessage = frame.decode()?;
                    let barrier = connection_state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("completion registry poisoned"))?
                        .completions
                        .get(&(stream, message.completion_id))
                        .cloned();
                    if let Some(barrier) = barrier {
                        holds.push(Hold::Enter(barrier, artifact.display().to_string()));
                    }
                }
            }
        }
        forwarder.push(
            stream,
            forward::Frame {
                wire,
                holds,
                publish: entered_sleeps
                    .into_iter()
                    .map(|(barrier, artifact)| super::BarrierProof {
                        barrier,
                        artifact,
                        journal_index: None,
                    })
                    .collect(),
                header_block: header[3] == 1,
                end_stream: matches!(header[3], 0 | 1) && header[4] & 1 != 0 || header[3] == 3,
            },
        )?;
    }
}

/// Read a whole header block so no independently forwarded DATA frame can
/// interrupt its CONTINUATION sequence. HPACK bytes are never decoded or changed.
async fn read_http2(
    input: &mut tokio::net::tcp::OwnedReadHalf,
) -> Result<Option<([u8; 9], Vec<u8>, Vec<u8>)>> {
    let mut header = [0; 9];
    match input.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    }
    let payload = read_payload(input, &header).await?;
    let mut wire = header.to_vec();
    wire.extend_from_slice(&payload);
    if header[3] == 1 && header[4] & 4 == 0 {
        loop {
            let mut next = [0; 9];
            input.read_exact(&mut next).await?;
            ensure!(
                next[3] == 9 && next[5..9] == header[5..9],
                "HTTP/2 header block has a foreign CONTINUATION"
            );
            let continuation = read_payload(input, &next).await?;
            wire.extend_from_slice(&next);
            wire.extend(continuation);
            ensure!(
                wire.len() <= 16 * 1024 * 1024,
                "oversized HTTP/2 header block"
            );
            if next[4] & 4 != 0 {
                break;
            }
        }
    }
    Ok(Some((header, payload, wire)))
}
async fn read_payload(
    input: &mut tokio::net::tcp::OwnedReadHalf,
    header: &[u8; 9],
) -> Result<Vec<u8>> {
    let length =
        (usize::from(header[0]) << 16) | (usize::from(header[1]) << 8) | usize::from(header[2]);
    ensure!(length <= 16 * 1024 * 1024, "oversized HTTP/2 frame");
    let mut payload = vec![0; length];
    input.read_exact(&mut payload).await?;
    Ok(payload)
}

fn same_run(observed: &WorkIdentity, wanted: &WorkIdentity) -> bool {
    observed.ingress == wanted.ingress
        && observed.run == wanted.run
        && observed.segment == wanted.segment
}
fn proposal_matches(value: &serde_json::Value, barrier: &Barrier) -> bool {
    if value
        .get("effect_journal_version")
        .and_then(serde_json::Value::as_u64)
        != Some(u64::from(lash_restate::EFFECT_JOURNAL_VERSION))
    {
        return false;
    }
    let call = barrier.work.call.as_deref();
    if barrier.kind == BarrierKind::XProposed
        && value.get("call_id").and_then(serde_json::Value::as_str) == call
        && call.is_some()
    {
        return value.get("attempt").and_then(serde_json::Value::as_u64)
            == barrier.work.ordinal.map(u64::from);
    }
    value
        .pointer("/record/events")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|events| {
            events.iter().any(|event| {
                let phase = event.get("event").and_then(serde_json::Value::as_str);
                event.get("call_id").and_then(serde_json::Value::as_str) == call
                    && call.is_some()
                    && matches!(
                        (&barrier.kind, phase),
                        (BarrierKind::DProposed, Some("decided"))
                            | (BarrierKind::VProposed, Some("presented"))
                            | (BarrierKind::DeclarationIssued, Some("declarations_issued"))
                    )
            })
        })
}
fn retry_matches(value: &serde_json::Value, barrier: &Barrier) -> bool {
    value
        .get("effect_journal_version")
        .and_then(serde_json::Value::as_u64)
        == Some(u64::from(lash_restate::EFFECT_JOURNAL_VERSION))
        && value
            .pointer("/record/events")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|events| {
                events.iter().any(|event| {
                    event.get("event").and_then(serde_json::Value::as_str)
                        == Some("retry_timer_registered")
                        && event.get("call_id").and_then(serde_json::Value::as_str)
                            == barrier.work.call.as_deref()
                        && event.get("failed").and_then(serde_json::Value::as_u64)
                            == barrier.work.ordinal.map(u64::from)
                })
            })
}
