//! Selection and ownership shared by every scenario lane.
use super::control::{Barrier, CleanupReceipt, ProcessReceipt};
use super::host::HostKind;
use super::provider::ProviderKind;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreKind {
    SqliteMemory,
    SqliteFile,
    PostgreSql,
}
/// The Restate server's invocation leg: `live` uses the server's defaults;
/// `replay` suspends at every await so every resumption replays the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Leg {
    Live,
    Replay,
}
impl Leg {
    pub fn manifest(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Replay => "replay",
        }
    }
    /// Server settings a leg applies to every Restate process it boots.
    pub fn server_env(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Live => &[],
            Self::Replay => &[("RESTATE_WORKER__INVOKER__INACTIVITY_TIMEOUT", "0s")],
        }
    }
}
impl StoreKind {
    pub fn manifest(&self) -> &'static str {
        match self {
            Self::SqliteMemory => "sqlite_memory",
            Self::SqliteFile => "sqlite_file",
            Self::PostgreSql => "postgresql",
        }
    }
}
/// One manifest permutation: the store a case's host writes through and the
/// leg its Restate server runs. The runner provisions both and publishes its
/// choice in the test environment; a case declares what it was written for
/// and refuses any other substrate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permutation {
    pub store: StoreKind,
    pub leg: Leg,
}
impl Permutation {
    pub fn provisioned(store: StoreKind, leg: Leg) -> anyhow::Result<Self> {
        let provisioned_store = std::env::var("LASH_E2E_STORE").unwrap_or_else(|_| "unset".into());
        let provisioned_leg = std::env::var("LASH_E2E_LEG").unwrap_or_else(|_| "unset".into());
        anyhow::ensure!(
            provisioned_store == store.manifest() && provisioned_leg == leg.manifest(),
            "case requires store={} leg={} but the runner provisioned store={provisioned_store} leg={provisioned_leg}",
            store.manifest(),
            leg.manifest()
        );
        Ok(Self { store, leg })
    }
    /// A PostgreSQL case gets a fresh database named for the case namespace
    /// on the server the gate provisioned, with this build's schema applied;
    /// other stores answer `None`.
    pub async fn postgres_url(&self, lease: &mut CaseLease) -> anyhow::Result<Option<String>> {
        if self.store != StoreKind::PostgreSql {
            return Ok(None);
        }
        let services = crate::harness::Services::from_env()?;
        let url = services.create_postgres_database(&lease.namespace).await?;
        {
            use sqlx::Connection as _;
            let mut connection = sqlx::PgConnection::connect(&url).await?;
            sqlx::raw_sql(lash_postgres_store::PostgresStorage::schema_ddl())
                .execute(&mut connection)
                .await?;
            connection.close().await.ok();
        }
        std::fs::write(
            lease.directory.join("postgres-lease.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "database": lease.namespace.replace('-', "_"),
                "ddl_sha256": lash_core::stable_hash::sha256_hex(
                    lash_postgres_store::PostgresStorage::schema_ddl().as_bytes()
                ),
            }))?,
        )?;
        lease.postgres_url = Some(url.clone());
        Ok(Some(url))
    }
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
