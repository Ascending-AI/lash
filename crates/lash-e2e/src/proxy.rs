//! A TCP proxy between one node and its database: the partition a case
//! puts a node behind. Partitioned, it forwards nothing in either direction
//! and holds what it read, so a statement in flight neither fails nor
//! arrives; healed, it delivers what it held and forwards again.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// The running proxy.
pub struct Proxy {
    addr: SocketAddr,
    partitioned: Arc<watch::Sender<bool>>,
    task: tokio::task::JoinHandle<()>,
}

impl Proxy {
    /// Forward a loopback port to `target`.
    ///
    /// # Errors
    ///
    /// The listener does not bind.
    pub async fn start(target: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let partitioned = Arc::new(watch::Sender::new(false));
        let state = partitioned.clone();
        let task = tokio::spawn(async move {
            while let Ok((inbound, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    let _ = gate(&state).await;
                    let Ok(outbound) = TcpStream::connect(target).await else {
                        return;
                    };
                    let (inbound_read, inbound_write) = inbound.into_split();
                    let (outbound_read, outbound_write) = outbound.into_split();
                    let up = tokio::spawn(pump(inbound_read, outbound_write, state.clone()));
                    let down = tokio::spawn(pump(outbound_read, inbound_write, state));
                    let _ = tokio::join!(up, down);
                });
            }
        });
        Ok(Self {
            addr,
            partitioned,
            task,
        })
    }

    /// The address the node connects to.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop forwarding.
    pub fn partition(&self) {
        self.partitioned.send_replace(true);
    }

    /// Forward again, delivering what was held.
    pub fn heal(&self) {
        self.partitioned.send_replace(false);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn gate(state: &watch::Sender<bool>) -> Result<()> {
    let mut partitioned = state.subscribe();
    partitioned.wait_for(|partitioned| !partitioned).await?;
    Ok(())
}

async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    state: Arc<watch::Sender<bool>>,
) -> Result<()> {
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        let read = from.read(&mut buffer).await?;
        if read == 0 {
            to.shutdown().await?;
            return Ok(());
        }
        gate(&state).await?;
        to.write_all(&buffer[..read]).await?;
    }
}
