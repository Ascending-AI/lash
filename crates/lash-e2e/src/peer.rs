//! A supporting process a case runs beside its nodes: an external peer the
//! hosts reach, such as the workbench's MCP fixture server. A peer is no
//! lash node; the case kills it to fault the hosts' connection to it.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

/// A running (or killed) peer process.
pub struct Peer {
    pub name: String,
    pub(crate) child: Option<tokio::process::Child>,
    pub(crate) pid: u32,
}

impl Peer {
    /// Spawn `binary args` with `env`, and wait until it accepts TCP
    /// connections on `address` when one is given.
    #[expect(
        clippy::too_many_arguments,
        reason = "a peer is its name, binary, arguments, environment, address, case directory, boot number and deadline"
    )]
    pub(crate) async fn spawn(
        name: &str,
        binary: &Path,
        args: &[&str],
        env: Vec<(String, String)>,
        address: Option<&str>,
        dir: &Path,
        boot: usize,
        deadline: Instant,
    ) -> Result<Self> {
        let mut command = tokio::process::Command::new(binary);
        command
            .args(args)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(
                dir.join(format!("{name}-{boot}.stdout")),
            )?)
            .stderr(std::fs::File::create(
                dir.join(format!("{name}-{boot}.stderr")),
            )?)
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("start peer {name} from {}", binary.display()))?;
        let pid = child.id().context("the peer exited at start")?;
        if let Some(address) = address {
            loop {
                if let Some(status) = child.try_wait()? {
                    bail!("peer {name} exited before it listened: {status}");
                }
                if tokio::net::TcpStream::connect(address).await.is_ok() {
                    break;
                }
                if Instant::now() >= deadline {
                    bail!("peer {name} did not listen on {address} by the deadline");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        Ok(Self {
            name: name.to_owned(),
            child: Some(child),
            pid,
        })
    }

    /// The process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// SIGKILL the peer and reap it.
    pub(crate) async fn kill(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            child.kill().await.context("SIGKILL the peer")?;
        }
        Ok(())
    }
}
