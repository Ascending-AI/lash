//! Linux socket ownership identifies the initiating real process without
//! changing the server protocol or relying on source-IP guessing.
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

#[derive(Clone, Debug, Default)]
pub struct LinkState {
    pub blocked: BTreeSet<(u32, u32)>,
    pub active: BTreeMap<u64, (u32, u32)>,
    pub opened: BTreeMap<(u32, u32), usize>,
    pub dropped: BTreeMap<(u32, u32), usize>,
}
/// All tasks belong to the cluster and are joined before cleanup passes.
pub struct PeerLinks {
    state: Arc<Mutex<LinkState>>,
    processes: Arc<Mutex<BTreeMap<u32, u32>>>,
    change: watch::Sender<u64>,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<Result<()>>>,
}
impl Default for PeerLinks {
    fn default() -> Self {
        let (change, _) = watch::channel(0);
        let (stop, _) = watch::channel(false);
        Self {
            state: Default::default(),
            processes: Default::default(),
            change,
            stop,
            tasks: Vec::new(),
        }
    }
}
impl PeerLinks {
    pub fn register(&self, node: u32, pid: u32) -> Result<()> {
        self.processes
            .lock()
            .map_err(|_| anyhow::anyhow!("process registry poisoned"))?
            .insert(node, pid);
        Ok(())
    }
    pub fn snapshot(&self) -> Result<LinkState> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("link registry poisoned"))?
            .clone())
    }
    pub async fn listen(
        &mut self,
        listener: std::net::TcpListener,
        target: u32,
        upstream: SocketAddr,
    ) -> Result<()> {
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let proxy = listener.local_addr()?;
        let processes = self.processes.clone();
        let state = self.state.clone();
        let mut stop = self.stop.subscribe();
        let change = self.change.clone();
        self.tasks.push(tokio::spawn(async move {
            let mut connections = JoinSet::new();
            let mut next = 0;
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    Some(result) = connections.join_next(), if !connections.is_empty() => { result??; }
                    accepted = listener.accept() => {
                        let (client, peer) = accepted?;
                        next += 1;
                        let connection_id = u64::from(target) << 32 | next;
                        let state = state.clone();
                        let processes = processes.clone();
                        let change = change.subscribe();
                        let stop = stop.clone();
                        connections.spawn(async move {
                            let from = identify(peer, proxy, &processes).await?;
                            forward(client, upstream, from, target, connection_id, state, change, stop).await
                        });
                    }
                }
            }
            drop(listener);
            while let Some(result) = connections.join_next().await { result??; }
            Ok(())
        }));
        Ok(())
    }
    /// Close existing selected streams before reporting the cut complete.
    pub async fn partition(
        &self,
        from: u32,
        to: u32,
        deadline: std::time::Instant,
    ) -> Result<usize> {
        let before = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("link registry poisoned"))?;
            state.blocked.insert((from, to));
            state
                .active
                .values()
                .filter(|pair| **pair == (from, to))
                .count()
        };
        self.change.send_modify(|epoch| *epoch += 1);
        loop {
            let active = self
                .snapshot()?
                .active
                .values()
                .any(|pair| *pair == (from, to));
            if !active {
                return Ok(before);
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "selected established peer streams did not close"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    pub fn heal(&self, from: u32, to: u32) -> Result<()> {
        ensure!(
            self.state
                .lock()
                .map_err(|_| anyhow::anyhow!("link registry poisoned"))?
                .blocked
                .remove(&(from, to)),
            "link was not partitioned"
        );
        self.change.send_modify(|epoch| *epoch += 1);
        Ok(())
    }
    pub async fn finish(&mut self) -> Result<()> {
        self.stop.send_replace(true);
        for task in self.tasks.drain(..) {
            task.await.context("peer proxy task panicked")??;
        }
        ensure!(self.snapshot()?.active.is_empty(), "peer streams leaked");
        Ok(())
    }
}
impl Drop for PeerLinks {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}
async fn identify(
    peer: SocketAddr,
    proxy: SocketAddr,
    processes: &Arc<Mutex<BTreeMap<u32, u32>>>,
) -> Result<u32> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let table = std::fs::read_to_string("/proc/net/tcp")?;
        // All fabric listeners and outgoing connections are IPv4 loopback.
        let local = format!(
            "{:08X}:{:04X}",
            u32::from_le_bytes([127, 0, 0, 1]),
            peer.port()
        );
        let remote = format!(
            "{:08X}:{:04X}",
            u32::from_le_bytes([127, 0, 0, 1]),
            proxy.port()
        );
        let inodes: BTreeSet<String> = table
            .lines()
            .skip(1)
            .filter_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                (fields.len() > 9 && fields[1] == local && fields[2] == remote)
                    .then(|| format!("socket:[{}]", fields[9]))
            })
            .collect();
        let registered = processes
            .lock()
            .map_err(|_| anyhow::anyhow!("process registry poisoned"))?
            .clone();
        for (node, pid) in registered {
            let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
                continue;
            };
            for entry in entries {
                let path = entry?.path();
                if let Ok(link) = std::fs::read_link(path)
                    && inodes.contains(&link.to_string_lossy().to_string())
                {
                    return Ok(node);
                }
            }
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "cannot attribute peer {peer} → {proxy} to an owned Restate process"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
#[allow(clippy::too_many_arguments)]
async fn forward(
    mut client: TcpStream,
    upstream: SocketAddr,
    from: u32,
    to: u32,
    id: u64,
    state: Arc<Mutex<LinkState>>,
    mut change: watch::Receiver<u64>,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    {
        let mut state = state
            .lock()
            .map_err(|_| anyhow::anyhow!("link registry poisoned"))?;
        if state.blocked.contains(&(from, to)) {
            *state.dropped.entry((from, to)).or_default() += 1;
            return Ok(());
        }
        state.active.insert(id, (from, to));
        *state.opened.entry((from, to)).or_default() += 1;
    }
    let result = async {
        let mut server = match TcpStream::connect(upstream).await {
            Ok(server) => server,
            Err(_) => return Ok(()), // killed node: peer must see a real closed connection
        };
        let relay = tokio::io::copy_bidirectional(&mut client, &mut server);
        tokio::pin!(relay);
        loop {
            tokio::select! {
                _ = stop.changed() => return Ok(()),
                result = &mut relay => { let _ = result; return Ok(()); }
                _ = change.changed() => {
                    if state.lock().map_err(|_| anyhow::anyhow!("link registry poisoned"))?.blocked.contains(&(from,to)) {
                        *state.lock().map_err(|_| anyhow::anyhow!("link registry poisoned"))?.dropped.entry((from,to)).or_default() += 1;
                        return Ok(());
                    }
                }
            }
        }
    }.await;
    state
        .lock()
        .map_err(|_| anyhow::anyhow!("link registry poisoned"))?
        .active
        .remove(&id);
    result
}
