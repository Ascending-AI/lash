//! The publication holder is a prebuilt, independently owned process. Its
//! socket, held DATA bytes and explicit file release survive a host SIGKILL.
use super::{
    Barrier, WorkIdentity,
    transport::{PublicationCut, TransportCut, V7Proxy},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

#[derive(Serialize, Deserialize)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
    pub upstream: SocketAddr,
    pub directory: PathBuf,
    pub control_socket: PathBuf,
    pub ready_file: PathBuf,
    pub deadline_secs: u64,
    pub cuts: Vec<TransportCut>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProxyCommand {
    Bind {
        invocation: String,
        work: WorkIdentity,
    },
    ArmCut {
        cut: TransportCut,
    },
    ArmRetry {
        barrier: Barrier,
    },
    ArmPublication {
        cut: PublicationCut,
    },
    ReplaceUpstream {
        upstream: SocketAddr,
    },
    Disconnect,
    Stop,
}
#[derive(Serialize, Deserialize)]
struct Reply {
    result: Option<serde_json::Value>,
    error: Option<String>,
}

pub async fn run(config: ProxyConfig) -> Result<()> {
    ensure!(config.deadline_secs > 0, "proxy needs a finite deadline");
    ensure!(
        !config.control_socket.exists() && !config.ready_file.exists(),
        "proxy case already has control or readiness artifacts"
    );
    std::fs::create_dir_all(&config.directory)?;
    let deadline = Instant::now() + Duration::from_secs(config.deadline_secs);
    let control = UnixListener::bind(&config.control_socket)?;
    let listener = std::net::TcpListener::bind(config.listen)?;
    let mut proxy = V7Proxy::start(
        listener,
        config.upstream,
        config.directory.clone(),
        deadline,
        config.cuts,
    )
    .await?;
    crate::node::write_atomically(
        &config.ready_file,
        &serde_json::to_vec(
            &serde_json::json!({"pid":std::process::id(),"endpoint":proxy.endpoint,"upstream":config.upstream,"control_socket":config.control_socket}),
        )?,
    )?;
    let result: Result<()> = async {
        loop {
            let (mut stream, _) = tokio::time::timeout_at(deadline.into(), control.accept())
                .await
                .context("proxy control deadline expired")??;
            let mut line = String::new();
            let count = tokio::time::timeout_at(
                deadline.into(),
                BufReader::new(&mut stream).read_line(&mut line),
            )
            .await
            .context("proxy command deadline expired")??;
            ensure!(
                count > 0 && count <= 65536,
                "proxy command is empty or oversized"
            );
            let command: ProxyCommand = serde_json::from_str(&line)?;
            let stopping = matches!(command, ProxyCommand::Stop);
            let result: Result<serde_json::Value> = async {
                Ok(match command {
                    ProxyCommand::Bind { invocation, work } => {
                        proxy.bind_invocation(invocation, work)?;
                        serde_json::json!({"bound":true})
                    }
                    ProxyCommand::ArmCut { cut } => {
                        proxy.arm_cut(cut)?;
                        serde_json::json!({"armed":true})
                    }
                    ProxyCommand::ArmRetry { barrier } => {
                        proxy.arm_retry(barrier)?;
                        serde_json::json!({"armed":true})
                    }
                    ProxyCommand::ArmPublication { cut } => {
                        proxy.arm_publication(cut)?;
                        serde_json::json!({"armed":true})
                    }
                    ProxyCommand::ReplaceUpstream { upstream } => {
                        serde_json::json!({"previous":proxy.replace_upstream(upstream)?})
                    }
                    ProxyCommand::Disconnect => {
                        serde_json::json!({"closed_streams":proxy.disconnect(deadline).await?})
                    }
                    ProxyCommand::Stop => {
                        proxy.finish().await?;
                        serde_json::json!({"closed":true})
                    }
                })
            }
            .await;
            let reply = match result {
                Ok(value) => Reply {
                    result: Some(value),
                    error: None,
                },
                Err(error) => Reply {
                    result: None,
                    error: Some(error.to_string()),
                },
            };
            stream.write_all(&serde_json::to_vec(&reply)?).await?;
            stream.write_all(b"\n").await?;
            stream.shutdown().await?;
            if stopping {
                ensure!(reply.error.is_none(), "proxy stop failed");
                return Ok(());
            }
        }
    }
    .await;
    let cleanup = proxy.finish().await;
    drop(control);
    std::fs::remove_file(&config.control_socket)?;
    result?;
    cleanup
}
pub async fn command(
    socket: &Path,
    command: &ProxyCommand,
    deadline: Instant,
) -> Result<serde_json::Value> {
    tokio::time::timeout_at(deadline.into(), async {
        let mut stream = UnixStream::connect(socket).await?;
        stream.write_all(&serde_json::to_vec(command)?).await?;
        stream.write_all(b"\n").await?;
        let mut line = String::new();
        ensure!(
            BufReader::new(stream).read_line(&mut line).await? > 0,
            "proxy closed without a receipt"
        );
        let reply: Reply = serde_json::from_str(&line)?;
        ensure!(
            reply.error.is_none(),
            "proxy refused command: {:?}",
            reply.error
        );
        reply.result.context("proxy command has no result")
    })
    .await
    .context("proxy command missed deadline")?
}
