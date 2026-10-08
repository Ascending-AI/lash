//! The real-host E2E harness (`docs/agents/real-host-e2e.md`).
//!
//! A case boots real lash host processes, each one lash node built through
//! the public facade over the case's store: one SQLite file or SQLite memory
//! store for a single node, or one PostgreSQL database several nodes share.
//! The case drives the hosts only through their HTTP surfaces, holds their
//! bodies and commits at named cuts, kills or partitions them, and reads
//! what they committed back from their commit ledgers and the store itself.
//! It ends by writing the [`CaseReceipt`](evidence::CaseReceipt) that
//! `scripts/e2e-gate.py` splits into the runner's artifacts.

mod case;
mod control;
mod evidence;
mod node;
mod peer;
mod proxy;

pub use case::{Case, Leg, Store};
pub use control::{Control, Delivery, ProviderReply};
pub use evidence::{CleanupReceipt, Evidence, NodeEvidence, Tripwire};
pub use node::{Host, Node, NodeOptions, free_port};
pub use peer::Peer;
pub use proxy::Proxy;

/// Read a JSON-lines file; a missing file reads as no lines.
///
/// # Errors
///
/// A line that is not JSON.
pub fn read_jsonl(path: &std::path::Path) -> anyhow::Result<Vec<serde_json::Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| Ok(serde_json::from_str(line)?))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}
