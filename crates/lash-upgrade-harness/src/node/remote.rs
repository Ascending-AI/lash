//! A remote-protocol connection between two builds (ADR 0115 §4).
//!
//! `remote-client` is one build's client: it starts the peer build's
//! `remote-host` as a child process and speaks to it over the child's
//! stdin and stdout, one JSON line per message. The host embeds lash as a
//! host does: on its first accepted request it serves and registers a
//! Restate deployment of its own build, so the root runs in the host and
//! its activity streams to the client live. The client opens with a
//! `Hello`, builds its [`Negotiated`] version from the host's answer, and
//! sends a turn request at that version; the host answers the request's
//! stream, its outcome and its errors at the request's version. The
//! transport's framing ([`Frame`]) is the host's own, as ADR 0115 §4 leaves
//! transports to hosts: lash owns the messages inside it.
//!
//! A client can offer a range of its choosing (`--offer`), to play a peer
//! no build is. When the host refuses the range, the client still sends one
//! request at the offered version, and the host refuses it again before any
//! decode or effect: the host's report counts the turns it started.

use std::io::{BufRead as _, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use lash_remote_protocol::{
    Envelope, Negotiated, Negotiation, REMOTE_PROTOCOL, RemoteProtocolError, RemoteSendOutcome,
    RemoteTurnActivity, RemoteTurnInput, RemoteTurnRequest, VersionRange, answer,
};
use serde::{Deserialize, Serialize};

use super::objects::parse_range;
use super::provider::ProviderArgs;
use super::{RestateArgs, Serving, StoreArgs};
use crate::identity::BuildLabel;

/// The field the synthetic N+1 adds at its newer remote-protocol version.
pub const ADDED_FIELD: &str = "synthetic_next_note";

#[derive(Clone, Debug, Args)]
pub struct RemoteHostArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    #[command(flatten)]
    pub restate: RestateArgs,
    #[command(flatten)]
    pub provider: ProviderArgs,
}

#[derive(Clone, Debug, Args)]
pub struct RemoteClientArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    #[command(flatten)]
    pub restate: RestateArgs,
    /// The peer build's `lash-upgrade-node`, started as the host.
    #[arg(long)]
    pub peer: PathBuf,
    /// Where the host records and holds its model calls.
    #[command(flatten)]
    pub provider: ProviderArgs,
    /// The range the client offers, `MIN..MAX`; this build's own when
    /// absent.
    #[arg(long, value_parser = parse_range)]
    pub offer: Option<VersionRange>,
    #[arg(long)]
    pub session: String,
    #[arg(long)]
    pub message: String,
}

/// One line on the connection: the transport's frame around one protocol
/// message.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "frame", content = "message", rename_all = "snake_case")]
pub enum Frame {
    /// A bootstrap message (ADR 0115 §4), including the host's refusal of a
    /// request whose version it does not speak.
    Negotiation(Negotiation),
    /// A request, at the negotiated version.
    Request(serde_json::Value),
    /// One activity of the request's root, at the request's version.
    Stream(serde_json::Value),
    /// The request's outcome, at the request's version.
    Reply(serde_json::Value),
    /// The request's error, at the request's version.
    Error(serde_json::Value),
    /// The host's account of the connection, once the client closed it.
    Report(HostReport),
}

/// A host's error answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostError {
    pub message: String,
}

/// One request as the host saw it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRequest {
    /// The version the request carried.
    pub version: u32,
    /// Whether the request's input carried [`ADDED_FIELD`] on the wire.
    pub carried_added_field: bool,
    /// The added field's value, as this host decoded it: only a host that
    /// knows the field reads one.
    pub note: Option<String>,
    /// `started`, `error` or `refused`.
    pub disposition: String,
}

/// What the host did on one connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReport {
    pub build: BuildLabel,
    /// The generation of the deployment the host served, once it served one.
    pub generation: Option<String>,
    pub local: VersionRange,
    pub answer: Option<Negotiation>,
    pub requests: Vec<HostRequest>,
    /// Turns the host sent into a session: its effects.
    pub turns_started: u32,
}

/// A refusal of the connection, typed as the client met it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unsupported {
    pub local: VersionRange,
    pub peer: VersionRange,
}

/// One message the client received, with the version it carried.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Received {
    /// `stream`, `reply`, `error` or `negotiation`.
    pub frame: String,
    /// The message's `protocol_version`; `None` for a bootstrap message.
    pub version: Option<u32>,
}

/// What `remote-client` reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientReport {
    pub build: BuildLabel,
    pub offered: VersionRange,
    pub answer: Negotiation,
    /// The version the connection selected, or the typed refusal.
    pub selected: Result<u32, Unsupported>,
    /// The version the turn request was sent at.
    pub request_version: u32,
    pub received: Vec<Received>,
    /// The outcome's status, when the host answered one.
    pub status: Option<String>,
    /// The assistant's reply, when the turn answered one.
    pub reply: Option<String>,
    /// The host's error answer to the invalid second request.
    pub error: Option<String>,
    pub host: HostReport,
}

fn write_frame(out: &mut impl Write, frame: &Frame) -> Result<()> {
    serde_json::to_writer(&mut *out, frame)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// The version a message states, read before anything else of it.
fn version_of(message: &serde_json::Value) -> Option<u32> {
    message
        .get("protocol_version")?
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
}

/// The connection's selected version for one request at `version`: the
/// selection a client that offered exactly `version` validated.
fn negotiated_at(version: u32) -> Result<Negotiated, RemoteProtocolError> {
    let peer = VersionRange::exactly(version);
    Negotiated::from_accept(
        peer,
        &answer(REMOTE_PROTOCOL, &Negotiation::Hello { supported: peer }),
    )
}

/// A writer that frames each JSON line the activity sink writes.
struct StreamFramer {
    out: Arc<Mutex<std::io::Stdout>>,
    line: Vec<u8>,
}

impl Write for StreamFramer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for &byte in bytes {
            if byte == b'\n' {
                let message: serde_json::Value = serde_json::from_slice(&self.line)
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                self.line.clear();
                let mut out = self
                    .out
                    .lock()
                    .map_err(|_| std::io::Error::other("stdout lock poisoned"))?;
                write_frame(&mut *out, &Frame::Stream(message))
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
            } else {
                self.line.push(byte);
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serve one connection on stdin and stdout until the client closes it.
pub(super) async fn host(args: RemoteHostArgs) -> Result<()> {
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let emit = |frame: Frame| -> Result<()> {
        let mut out = out.lock().map_err(|_| anyhow!("stdout lock poisoned"))?;
        write_frame(&mut *out, &frame)
    };
    let mut report = HostReport {
        build: BuildLabel::current(),
        generation: None,
        local: REMOTE_PROTOCOL,
        answer: None,
        requests: Vec::new(),
        turns_started: 0,
    };
    let mut lines = BufReader::new(std::io::stdin()).lines();
    let hello = match lines.next() {
        Some(line) => match serde_json::from_str::<Frame>(&line?)? {
            Frame::Negotiation(hello) => hello,
            other => bail!("the connection opened with {other:?}, not a Hello"),
        },
        None => bail!("the client closed the connection before its Hello"),
    };
    let accepted = answer(REMOTE_PROTOCOL, &hello);
    emit(Frame::Negotiation(accepted.clone()))?;
    report.answer = Some(accepted.clone());
    let open = matches!(accepted, Negotiation::Accept { .. });
    let mut node: Option<Serving> = None;
    for line in lines {
        let Frame::Request(message) = serde_json::from_str::<Frame>(&line?)? else {
            bail!("the client sent a frame that is not a request");
        };
        let bytes = serde_json::to_vec(&message)?;
        let version = version_of(&message).unwrap_or(0);
        let carried_added_field = message
            .get("input")
            .and_then(|input| input.get(ADDED_FIELD))
            .is_some();
        let mut record = HostRequest {
            version,
            carried_added_field,
            note: None,
            disposition: "refused".to_owned(),
        };
        // A connection that did not negotiate runs nothing, and a request
        // outside this build's range is refused before its body decodes.
        let request_envelope =
            match Envelope::<serde_json::Value>::decode_json(&bytes, REMOTE_PROTOCOL) {
                Ok(envelope) if open => envelope,
                Ok(_) => {
                    emit(Frame::Negotiation(Negotiation::Unsupported {
                        local: REMOTE_PROTOCOL,
                        peer: VersionRange::exactly(version.max(1)),
                    }))?;
                    report.requests.push(record);
                    continue;
                }
                Err(RemoteProtocolError::Unsupported { local, peer }) => {
                    emit(Frame::Negotiation(Negotiation::Unsupported { local, peer }))?;
                    report.requests.push(record);
                    continue;
                }
                Err(error) => bail!("the request is not an envelope: {error}"),
            };
        let request = match RemoteTurnRequest::decode_json(&bytes) {
            Ok(request) => request,
            Err(error) => {
                record.disposition = "error".to_owned();
                let reply = Envelope::reply_to(
                    &request_envelope,
                    HostError {
                        message: error.to_string(),
                    },
                );
                emit(Frame::Error(serde_json::to_value(&reply)?))?;
                report.requests.push(record);
                continue;
            }
        };
        record.note = note_of(&request.input);
        record.disposition = "started".to_owned();
        let negotiated = negotiated_at(version)?;
        if node.is_none() {
            let serving = Serving::start(
                &args.store,
                &args.restate,
                &args.provider,
                std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                true,
            )
            .await?;
            report.generation = Some(serving.ready.generation.clone());
            node = Some(serving);
        }
        let Some(serving) = node.as_ref() else {
            bail!("the host serves no deployment");
        };
        let core = &serving.core;
        report.turns_started += 1;
        let session_id = request.session_id.clone();
        let turn_id = request.turn_id.clone();
        let input = lash::TurnInput::try_from(request)
            .map_err(|error| anyhow!("the request's input: {error}"))?;
        let session = core
            .session(session_id.clone())
            .create()
            .await
            .map_err(|error| anyhow!("create session {session_id}: {error}"))?;
        let handle = session
            .send(input)
            .id(turn_id)
            .into_future()
            .await
            .map_err(|error| anyhow!("send to {session_id}: {error}"))?;
        let input_id = handle.input_id().clone();
        let sink = lash_remote_protocol::RemoteTurnActivitySink::new(
            StreamFramer {
                out: Arc::clone(&out),
                line: Vec::new(),
            },
            0,
            negotiated,
        );
        let outcome = handle
            .outcome_into(&sink)
            .await
            .map_err(|error| anyhow!("await the turn on {session_id}: {error}"))?;
        let errors = sink.take_errors();
        if !errors.is_empty() {
            bail!("the activity stream failed: {errors:?}");
        }
        let remote = outcome.to_remote(&session_id, &input_id);
        let encoded = remote.encode_json(&negotiated)?;
        emit(Frame::Reply(serde_json::from_slice(&encoded)?))?;
        report.requests.push(record);
    }
    emit(Frame::Report(report))?;
    if let Some(serving) = node {
        serving.stop().await;
    }
    Ok(())
}

#[cfg(feature = "synthetic-next")]
fn note_of(input: &RemoteTurnInput) -> Option<String> {
    input.synthetic_next_note.clone()
}

#[cfg(not(feature = "synthetic-next"))]
fn note_of(_input: &RemoteTurnInput) -> Option<String> {
    None
}

/// The turn input this build sends: the synthetic N+1 fills its added
/// field, which its encoder of N's version drops.
fn turn_input(message: &str) -> RemoteTurnInput {
    #[allow(unused_mut, reason = "only the synthetic N+1 fills the added field")]
    let mut input = RemoteTurnInput::text(message);
    #[cfg(feature = "synthetic-next")]
    {
        input.synthetic_next_note = Some(format!("from {}", BuildLabel::current()));
    }
    input
}

/// Connect to the peer build's host and run one turn through it.
pub(super) fn client(args: RemoteClientArgs) -> Result<ClientReport> {
    let mut host = Command::new(&args.peer)
        .arg("remote-host")
        .args(super::store_args(&args.store))
        .args(super::restate_args(&args.restate))
        .args(args.provider.to_args())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn {} remote-host", args.peer.display()))?;
    let mut to_host = host.stdin.take().context("the host's stdin")?;
    let mut from_host = BufReader::new(host.stdout.take().context("the host's stdout")?).lines();
    let mut next = || -> Result<Frame> {
        let line = from_host
            .next()
            .ok_or_else(|| anyhow!("the host closed the connection"))??;
        serde_json::from_str(&line).with_context(|| format!("decode the host's frame: {line}"))
    };

    let offered = args.offer.unwrap_or(REMOTE_PROTOCOL);
    write_frame(
        &mut to_host,
        &Frame::Negotiation(Negotiation::Hello { supported: offered }),
    )?;
    let Frame::Negotiation(accept) = next()? else {
        bail!("the host did not answer the Hello");
    };
    let negotiated = Negotiated::from_accept(offered, &accept);
    let selected = match &negotiated {
        Ok(negotiated) => Ok(negotiated.selected()),
        Err(RemoteProtocolError::Unsupported { local, peer }) => Err(Unsupported {
            local: *local,
            peer: *peer,
        }),
        Err(error) => bail!("the host's answer is invalid: {error}"),
    };

    let request = RemoteTurnRequest {
        session_id: lash::SessionId::from(args.session.clone()),
        turn_id: lash::TurnId::from(format!("{}-turn", args.session)),
        input: turn_input(&args.message),
        protocol_turn_options: None,
        tool_grants: Vec::new(),
        metadata: std::collections::HashMap::new(),
    };
    let (request_version, request_json) = match &negotiated {
        Ok(negotiated) => (
            negotiated.selected(),
            serde_json::from_slice::<serde_json::Value>(&request.encode_json(negotiated)?)?,
        ),
        // A refused connection still sends one request at the version it
        // offered: the host must refuse it before any decode or effect.
        Err(_) => {
            let mut value = serde_json::to_value(&request)?;
            if let Some(fields) = value.as_object_mut() {
                fields.insert(
                    "protocol_version".to_owned(),
                    serde_json::json!(offered.max()),
                );
            }
            (offered.max(), value)
        }
    };
    write_frame(&mut to_host, &Frame::Request(request_json))?;

    let mut received = Vec::new();
    // The request's own answer: every stream item, then one reply, error
    // or refusal.
    let (status, reply) = loop {
        match next()? {
            Frame::Stream(message) => {
                let bytes = serde_json::to_vec(&message)?;
                let envelope = Envelope::<RemoteTurnActivity>::decode_json(&bytes, offered)
                    .map_err(|error| anyhow!("a stream item: {error}"))?;
                received.push(Received {
                    frame: "stream".to_owned(),
                    version: Some(envelope.protocol_version()),
                });
            }
            Frame::Reply(message) => {
                let bytes = serde_json::to_vec(&message)?;
                let envelope = Envelope::<RemoteSendOutcome>::decode_json(&bytes, offered)
                    .map_err(|error| anyhow!("the reply: {error}"))?;
                received.push(Received {
                    frame: "reply".to_owned(),
                    version: Some(envelope.protocol_version()),
                });
                let outcome = envelope.into_body();
                outcome
                    .validate()
                    .map_err(|error| anyhow!("the reply is inconsistent: {error}"))?;
                let reply = outcome
                    .report
                    .as_ref()
                    .map(|report| report.assistant_output.safe_text.clone())
                    .filter(|text| !text.is_empty());
                break (Some(format!("{:?}", outcome.status)), reply);
            }
            Frame::Negotiation(refusal) => {
                received.push(Received {
                    frame: "negotiation".to_owned(),
                    version: None,
                });
                break (Some(format!("{refusal:?}")), None);
            }
            other => bail!("the host answered the request with {other:?}"),
        }
    };

    // An invalid request on a negotiated connection is answered with an
    // error at its version.
    let mut error = None;
    if let Ok(negotiated) = &negotiated {
        let invalid = RemoteTurnRequest {
            session_id: lash::SessionId::from(String::new()),
            ..request.clone()
        };
        let invalid: serde_json::Value =
            serde_json::from_slice(&Envelope::at(negotiated, &invalid).encode_json()?)?;
        write_frame(&mut to_host, &Frame::Request(invalid))?;
        let Frame::Error(message) = next()? else {
            bail!("the host did not answer the invalid request with an error");
        };
        let bytes = serde_json::to_vec(&message)?;
        let envelope = Envelope::<HostError>::decode_json(&bytes, offered)
            .map_err(|decode| anyhow!("the error: {decode}"))?;
        received.push(Received {
            frame: "error".to_owned(),
            version: Some(envelope.protocol_version()),
        });
        error = Some(envelope.into_body().message);
    }

    drop(to_host);
    let host_report = match next()? {
        Frame::Report(report) => report,
        other => bail!("the host sent {other:?} after the connection closed"),
    };
    let exit = host.wait().context("reap the host")?;
    if !exit.success() {
        bail!("the host exited {exit}");
    }
    Ok(ClientReport {
        build: BuildLabel::current(),
        offered,
        answer: accept,
        selected,
        request_version,
        received,
        status,
        reply,
        error,
        host: host_report,
    })
}
