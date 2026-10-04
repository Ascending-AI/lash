//! Selection and ownership shared by every scenario lane.
use super::control::{Barrier, CleanupReceipt, ProcessReceipt};
use super::host::HostKind;
use super::provider::ProviderKind;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreKind {
    SqliteMemory,
    SqliteFile,
    PostgreSql,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Channel {
    Standard,
    Rlm,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactIdentity {
    pub role: String,
    pub path: PathBuf,
    pub sha256: String,
    pub candidate_sha: String,
    pub generation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaseSpec {
    pub id: String,
    pub rules: Vec<String>,
    pub host: HostKind,
    pub store: StoreKind,
    pub channel: Channel,
    pub provider: ProviderKind,
    pub restate_nodes: usize,
    pub artifacts: Vec<ArtifactIdentity>,
    pub cuts: Vec<Barrier>,
    pub expected_terminal: String,
    /// Missing landing receipts hold selection; they never count as passed.
    pub requires: Vec<String>,
}
/// Stable ownership survives every process incarnation of a case.
pub struct CaseLease {
    pub gate_id: String,
    pub namespace: String,
    pub authority: String,
    pub directory: PathBuf,
    pub postgres_url: Option<String>,
    pub ports: Vec<u16>,
    pub deadline: Instant,
    pub processes: Vec<ProcessReceipt>,
    pub cleanup: Vec<CleanupReceipt>,
}
