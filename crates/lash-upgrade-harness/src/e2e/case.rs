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

impl ArtifactIdentity {
    /// Verify materialized bytes before acquiring any service lease.
    pub fn verify(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.sha256.len() == 64, "{} lacks a SHA-256 pin", self.role);
        let bytes = std::fs::read(&self.path)?;
        anyhow::ensure!(
            lash_core::stable_hash::sha256_hex(&bytes) == self.sha256,
            "{} binary digest differs from its materialization receipt",
            self.role
        );
        anyhow::ensure!(
            !self.candidate_sha.is_empty(),
            "{} lacks candidate provenance",
            self.role
        );
        Ok(())
    }
}

impl CaseSpec {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.id.is_empty() && !self.rules.is_empty(),
            "case lacks identity or named rules"
        );
        anyhow::ensure!(
            self.restate_nodes == 1 || self.restate_nodes == 3,
            "Restate requires one or three nodes"
        );
        anyhow::ensure!(
            self.requires.is_empty(),
            "{} is held by {:?}",
            self.id,
            self.requires
        );
        anyhow::ensure!(
            !self.artifacts.is_empty(),
            "{} lacks prebuilt artifacts",
            self.id
        );
        for artifact in &self.artifacts {
            artifact.verify()?;
        }
        Ok(())
    }
}

impl CaseLease {
    pub fn new(id: &str, directory: PathBuf, deadline: Instant) -> anyhow::Result<Self> {
        let gate_id = std::env::var("KILN_GATE_ID")?;
        anyhow::ensure!(
            !gate_id.is_empty(),
            "a private Kiln gate identity is required"
        );
        anyhow::ensure!(
            id.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "invalid case slug"
        );
        std::fs::create_dir(&directory)?;
        let digest = lash_core::stable_hash::sha256_hex(gate_id.as_bytes());
        let namespace = format!("e2e-{id}-{}", &digest[..12]);
        lash_restate::RestateNamespace::new(&namespace)?;
        Ok(Self {
            gate_id,
            namespace: namespace.clone(),
            authority: namespace,
            directory,
            postgres_url: None,
            ports: Vec::new(),
            deadline,
            processes: Vec::new(),
            cleanup: Vec::new(),
        })
    }
}
