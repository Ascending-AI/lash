//! The doorbell of one database file's wake listeners on this machine
//! (FIG-7044). A commit that wrote wake rows rings, once it committed, the
//! listeners its rows name: this process's through the database's [`Ring`],
//! and each other process's through a Unix datagram sent to the endpoint its
//! listener bound: an abstract socket named after the database and the node,
//! so no path length limits it, beside a `wake-<node digest>.node` marker in
//! the liveness lock directory, which names the endpoints a ring to every
//! node reaches. A ring carries nothing but "read your wake rows": the rows
//! stay the only hint and the claim and mail polls the only authority. A
//! listener without an endpoint (no abstract sockets on this platform, a
//! live boot of its node holds it), a full socket buffer or a stale marker
//! loses nothing: the listener finds the rows at its next data-version poll.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::Ring;

/// The listeners one committed flush addressed.
#[derive(Debug, Default)]
pub(crate) struct Rung {
    /// The nodes its rows named.
    nodes: BTreeSet<String>,
    /// Whether a row named every node.
    everyone: bool,
}

impl Rung {
    /// Add the listener a row for `node` addresses: every one, with none.
    pub(crate) fn add(&mut self, node: Option<&str>) {
        match node {
            Some(node) => {
                self.nodes.insert(node.to_owned());
            }
            None => self.everyone = true,
        }
    }

    fn is_empty(&self) -> bool {
        self.nodes.is_empty() && !self.everyone
    }
}

/// Rings one database file's listeners.
#[derive(Clone)]
pub(crate) struct Doorbell {
    /// Where the endpoints' markers live.
    dir: PathBuf,
    /// The database's digest, which every endpoint's name carries.
    database: u64,
    ring: Arc<Ring>,
    #[cfg(target_os = "linux")]
    sender: Option<Arc<std::os::unix::net::UnixDatagram>>,
}

impl std::fmt::Debug for Doorbell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Doorbell")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// The FNV-1a digest of a name: stable across builds, so every process
/// names one endpoint per database and node.
fn digest(id: &str) -> u64 {
    id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

const PREFIX: &str = "wake-";
const SUFFIX: &str = ".node";

impl Doorbell {
    /// The doorbell of database `database`'s listeners, whose markers live
    /// in `dir`, and of this process's, which `ring` wakes.
    pub(crate) fn new(dir: PathBuf, database: &str, ring: Arc<Ring>) -> Self {
        Self {
            dir,
            database: digest(database),
            ring,
            #[cfg(target_os = "linux")]
            sender: std::os::unix::net::UnixDatagram::unbound()
                .and_then(|sender| sender.set_nonblocking(true).map(|()| sender))
                .ok()
                .map(Arc::new),
        }
    }

    fn marker(&self, node: u64) -> PathBuf {
        self.dir.join(format!("{PREFIX}{node:016x}{SUFFIX}"))
    }

    /// The abstract socket address of node digest `node`'s endpoint.
    #[cfg(target_os = "linux")]
    fn address(&self, node: u64) -> std::io::Result<std::os::unix::net::SocketAddr> {
        use std::os::linux::net::SocketAddrExt as _;
        std::os::unix::net::SocketAddr::from_abstract_name(
            format!("lash-wake-{:016x}-{node:016x}", self.database).as_bytes(),
        )
    }

    /// Ring the listeners `rung` addresses, once each: this process's at
    /// once, every other process's through its endpoint.
    pub(crate) fn ring(&self, rung: &Rung) {
        if rung.is_empty() {
            return;
        }
        self.ring.ring();
        #[cfg(target_os = "linux")]
        if let Some(sender) = &self.sender {
            let send = |node: u64| {
                // Missing, stale or full: that listener polls.
                if let Ok(address) = self.address(node) {
                    let _ = sender.send_to_addr(&[1], &address);
                }
            };
            if rung.everyone {
                for node in self.endpoints() {
                    send(node);
                }
            } else {
                for node in &rung.nodes {
                    send(digest(node));
                }
            }
        }
    }

    /// The node digests of every endpoint marked beside the database.
    #[expect(
        clippy::disallowed_methods,
        reason = "wake endpoint markers are files beside the host-supplied database file"
    )]
    fn endpoints(&self) -> Vec<u64> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let digest = name.to_str()?.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
                u64::from_str_radix(digest, 16).ok()
            })
            .collect()
    }

    /// Bind `node`'s endpoint and mark it, and ring this process's
    /// listeners whenever it is rung, on a thread of its own, until the
    /// answer drops. `None` when no endpoint can be bound: the node's
    /// listener then only polls.
    #[cfg(target_os = "linux")]
    #[expect(
        clippy::disallowed_methods,
        reason = "wake endpoint markers are files beside the host-supplied database file"
    )]
    pub(crate) fn answer(&self, node: &str) -> Option<Answering> {
        let node = digest(node);
        let socket = std::os::unix::net::UnixDatagram::bind_addr(&self.address(node).ok()?).ok()?;
        let marker = self.marker(node);
        std::fs::create_dir_all(&self.dir).ok()?;
        std::fs::write(&marker, b"").ok()?;
        // The thread looks at its stop flag at least this often, should the
        // shutdown that ends its wait not reach it.
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        let waker = socket.try_clone().ok()?;
        let ring = Arc::clone(&self.ring);
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("lash-sqlite-doorbell".into())
            .spawn(move || {
                let mut buffer = [0_u8; 1];
                while !thread_stop.load(Ordering::Acquire) {
                    match socket.recv(&mut buffer) {
                        Ok(0) => break,
                        Ok(_) => ring.ring(),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::Interrupted
                            ) => {}
                        Err(_) => break,
                    }
                }
                // The endpoint goes with its socket; the marker of a node
                // whose next boot marks it again costs one failed send.
                let _ = std::fs::remove_file(&marker);
            })
            .ok()?;
        Some(Answering { stop, waker })
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn answer(&self, _node: &str) -> Option<Answering> {
        None
    }
}

/// A listener's bound endpoint: dropping it ends the thread that answers it
/// and removes the endpoint.
pub(crate) struct Answering {
    stop: Arc<AtomicBool>,
    #[cfg(target_os = "linux")]
    waker: std::os::unix::net::UnixDatagram,
}

impl Drop for Answering {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Ends the thread's wait at once.
        #[cfg(target_os = "linux")]
        let _ = self.waker.shutdown(std::net::Shutdown::Read);
    }
}
