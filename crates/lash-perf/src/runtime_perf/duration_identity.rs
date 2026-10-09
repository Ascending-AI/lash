//! Experiment identity is a precondition for comparing measurements.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::scenarios::RuntimePerfScenario;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ExperimentIdentity {
    pub(crate) host: String,
    pub(crate) compiler: String,
    pub(crate) allocator: String,
    pub(crate) build_profile: String,
    pub(crate) backend: String,
    /// Configured storage policy, never a measured durability claim.
    pub(crate) configured_durability: String,
    pub(crate) workload: String,
    pub(crate) configured_geometry: String,
    pub(crate) postgres_settings: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityDimension {
    Host,
    Compiler,
    Allocator,
    BuildProfile,
    Backend,
    Durability,
    Workload,
    Geometry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComparisonRefusal {
    MissingIdentity(IdentityDimension),
    IdentityMismatch(IdentityDimension),
}

impl ExperimentIdentity {
    pub(crate) fn capture(configured_geometry: serde_json::Value) -> Self {
        // Hash machine identity and hardware/kernel facts; do not store machine IDs.
        let host = std::fs::read_to_string("/etc/machine-id")
            .ok()
            .filter(|id| !id.trim().is_empty())
            .map(|id| {
                let cpu = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
                let model = cpu
                    .lines()
                    .find(|line| line.starts_with("model name"))
                    .unwrap_or("");
                let kernel =
                    std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
                format!(
                    "{:x}",
                    Sha256::digest(format!(
                        "{};{};{};{model};{kernel}",
                        id.trim(),
                        std::env::consts::OS,
                        std::env::consts::ARCH
                    ))
                )
            })
            .unwrap_or_default();
        Self {
            host,
            compiler: env!("LASH_PERF_COMPILER").to_string(),
            allocator: format!("system+{}", crate::ALLOCATION_MODE),
            build_profile: format!(
                "{};debug_assertions={}",
                env!("LASH_PERF_BUILD_PROFILE"),
                cfg!(debug_assertions)
            ),
            backend: String::new(),
            configured_durability: String::new(),
            workload: String::new(),
            configured_geometry: configured_geometry.to_string(),
            postgres_settings: None,
        }
    }

    /// Read the selected server's policy before the measured window. Unknown
    /// policy is retained as missing identity and cannot enter a comparison.
    pub(crate) async fn read_postgres_settings(&mut self, scenarios: &[RuntimePerfScenario]) {
        if !scenarios.iter().any(|scenario| {
            scenario.uses_postgres() || *scenario == RuntimePerfScenario::StoreHardeningHotPaths
        }) {
            return;
        }
        let Some(url) = super::super::measurement::configured_postgres_database_url() else {
            return;
        };
        let settings = async {
            let mut connection = <sqlx::PgConnection as sqlx::Connection>::connect(&url).await?;
            let rows: Vec<(String, String)> = sqlx::query_as("SELECT name, setting FROM pg_settings WHERE name IN ('fsync', 'synchronous_commit', 'full_page_writes', 'wal_level', 'server_version') ORDER BY name")
                .fetch_all(&mut connection).await?;
            Ok::<_, sqlx::Error>(rows.into_iter().collect::<BTreeMap<_, _>>())
        }.await;
        match settings {
            Ok(settings) => self.postgres_settings = Some(serde_json::json!(settings).to_string()),
            Err(error) => eprintln!(
                "warning: duration comparison refused: missing PostgreSQL durability identity ({error})"
            ),
        }
    }

    pub(crate) fn for_scenario(&self, name: &str) -> Self {
        let mut identity = self.clone();
        identity.workload = name.to_string();
        let scenario = RuntimePerfScenario::KNOWN
            .iter()
            .find(|scenario| scenario.name() == name);
        let uses_pg = scenario.is_some_and(|scenario| {
            scenario.uses_postgres() || *scenario == RuntimePerfScenario::StoreHardeningHotPaths
        });
        let file = scenario.is_some_and(|scenario| scenario.is_durable());
        let no_store = scenario.is_some_and(|scenario| {
            matches!(
                scenario,
                RuntimePerfScenario::ToolDiscoverySearch
                    | RuntimePerfScenario::OpenAiResponsesSseParse
                    | RuntimePerfScenario::DirectLlmClient
            )
        });
        identity.backend = if no_store {
            "none (standalone client/parser)"
        } else if scenario
            .is_some_and(|scenario| *scenario == RuntimePerfScenario::StoreHardeningHotPaths)
        {
            "store-ports/sqlite-memory+sqlite-file+postgresql"
        } else if uses_pg {
            "lash-durable/postgresql"
        } else if file {
            "lash-durable/sqlite-file"
        } else {
            "lash-durable/sqlite-memory"
        }
        .to_string();
        identity.configured_durability = if no_store {
            "not applicable (no store)".to_string()
        } else if uses_pg {
            if scenario
                .is_some_and(|scenario| *scenario == RuntimePerfScenario::StoreHardeningHotPaths)
            {
                self.postgres_settings
                    .as_ref()
                    .map(|pg| {
                        format!(
                            "postgres={pg};sqlite_file={:?};sqlite_memory={:?}",
                            lash_sqlite_store::SqliteStoreSetOptions::standard(
                                lash_sqlite_store::SqliteSynchronous::Normal
                            ),
                            lash_sqlite_store::SqliteStoreSetOptions::memory()
                        )
                    })
                    .unwrap_or_default()
            } else {
                self.postgres_settings.clone().unwrap_or_default()
            }
        } else {
            // These are the exact options selected by the runtime harness.
            format!(
                "{:?}",
                if file {
                    lash_sqlite_store::SqliteStoreSetOptions::standard(
                        lash_sqlite_store::SqliteSynchronous::Normal,
                    )
                } else {
                    lash_sqlite_store::SqliteStoreSetOptions::memory()
                }
            )
        };
        if !uses_pg {
            identity.postgres_settings = None;
        }
        identity
    }

    pub(crate) fn compare(&self, other: &Self) -> Result<(), ComparisonRefusal> {
        use IdentityDimension as D;
        for (dimension, left, right) in [
            (D::Host, &self.host, &other.host),
            (D::Compiler, &self.compiler, &other.compiler),
            (D::Allocator, &self.allocator, &other.allocator),
            (D::BuildProfile, &self.build_profile, &other.build_profile),
            (D::Backend, &self.backend, &other.backend),
            (
                D::Durability,
                &self.configured_durability,
                &other.configured_durability,
            ),
            (D::Workload, &self.workload, &other.workload),
            (
                D::Geometry,
                &self.configured_geometry,
                &other.configured_geometry,
            ),
        ] {
            if left.trim().is_empty() || right.trim().is_empty() {
                return Err(ComparisonRefusal::MissingIdentity(dimension));
            }
            if left != right {
                return Err(ComparisonRefusal::IdentityMismatch(dimension));
            }
        }
        Ok(())
    }
}
