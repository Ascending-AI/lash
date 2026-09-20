use lash_sansio::SessionId;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core::SessionStoreFactory;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::replay::ReplayError;
use crate::runtime_boundaries::RuntimeEffectReplayStore;
use crate::runtime_replay::{
    BackendReplayError, BoundaryDivergence, ReplayBackend, replay_trace_through_backend,
};
use crate::store::BackendCheckpointReplayEvidence;
use crate::trace::{
    AbstractWorldSummary, OracleVerdict, SimulationTrace, TraceIoError, read_trace,
};

pub const SQLITE_REPLAY_REPORT_SCHEMA: &str = "lash.sim.sqlite-runtime-replay-report.v4";
pub const SQLITE_DIVERGENCE_SCHEMA: &str = "lash.sim.sqlite-runtime-divergence.v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SqliteReplayReport {
    pub schema: String,
    pub trace_path: PathBuf,
    pub database_path: PathBuf,
    pub terminal_verdict: OracleVerdict,
    pub delivered_event_count: usize,
    pub runtime_replayed_boundary_count: usize,
    pub replayed_boundary_families: Vec<String>,
    pub carried_forward_boundary_count: usize,
    pub checkpoint_replay: BackendCheckpointReplayEvidence,
    pub reopened_sessions: Vec<SqliteReopenedSessionEvidence>,
    pub final_summary: AbstractWorldSummary,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SqliteDivergenceArtifact {
    pub schema: String,
    pub trace_path: PathBuf,
    pub database_path: PathBuf,
    pub verdict: OracleVerdict,
    pub expected_summary: AbstractWorldSummary,
    pub actual_summary: AbstractWorldSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<SqliteBoundaryDivergence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SqliteBoundaryDivergence {
    pub boundary_id: String,
    pub boundary_kind: String,
    pub expected_observed: Value,
    pub actual_observed: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SqliteReopenedSessionEvidence {
    pub session_id: SessionId,
    pub database_path: PathBuf,
    pub turn_index: usize,
    pub graph_node_count: usize,
    pub transcript_message_count: usize,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum SqliteReplayError {
    TraceIo(TraceIoError),
    Replay(ReplayError),
    Io(std::io::Error),
    Json(serde_json::Error),
    Runtime(String),
    Assertion(String),
    Divergence(String),
}

impl fmt::Display for SqliteReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TraceIo(err) => write!(f, "{err}"),
            Self::Replay(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "SQLite runtime replay I/O failed: {err}"),
            Self::Json(err) => write!(f, "SQLite runtime replay JSON failed: {err}"),
            Self::Runtime(message) => write!(f, "SQLite runtime replay failed: {message}"),
            Self::Assertion(message) => {
                write!(f, "SQLite runtime replay assertion failed: {message}")
            }
            Self::Divergence(message) => write!(f, "SQLite runtime replay diverged: {message}"),
        }
    }
}

impl std::error::Error for SqliteReplayError {}

impl From<TraceIoError> for SqliteReplayError {
    fn from(value: TraceIoError) -> Self {
        Self::TraceIo(value)
    }
}

impl From<ReplayError> for SqliteReplayError {
    fn from(value: ReplayError) -> Self {
        Self::Replay(value)
    }
}

impl From<std::io::Error> for SqliteReplayError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for SqliteReplayError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl BackendReplayError for SqliteReplayError {
    fn runtime(message: String) -> Self {
        Self::Runtime(message)
    }

    fn assertion(message: String) -> Self {
        Self::Assertion(message)
    }

    fn divergence(message: String) -> Self {
        Self::Divergence(message)
    }
}

pub async fn replay_trace_file_to_sqlite(
    trace_path: &Path,
    db_path: &Path,
    report_path: Option<&Path>,
) -> Result<SqliteReplayReport, SqliteReplayError> {
    let trace = read_trace(trace_path)?;
    replay_trace_to_sqlite(trace_path, &trace, db_path, report_path).await
}

pub async fn replay_trace_to_sqlite(
    trace_path: &Path,
    trace: &SimulationTrace,
    db_path: &Path,
    report_path: Option<&Path>,
) -> Result<SqliteReplayReport, SqliteReplayError> {
    prepare_database_root(db_path)?;

    let outcome = replay_trace_through_backend(
        SqliteReplayBackend {
            database_root: db_path.to_path_buf(),
        },
        trace_path,
        trace,
        report_path,
    )
    .await?;
    let reopened_sessions = outcome
        .reopened_sessions
        .into_iter()
        .map(|observation| SqliteReopenedSessionEvidence {
            session_id: observation.session_id,
            database_path: lash_sqlite_store::SqliteSessionStoreFactory::new(db_path.to_path_buf())
                .catalog_path(),
            turn_index: observation.turn_index,
            graph_node_count: observation.graph_node_count,
            transcript_message_count: observation.transcript_message_count,
        })
        .collect();
    let report = SqliteReplayReport {
        schema: SQLITE_REPLAY_REPORT_SCHEMA.to_string(),
        trace_path: trace_path.to_path_buf(),
        database_path: db_path.to_path_buf(),
        terminal_verdict: outcome.terminal_verdict,
        delivered_event_count: trace.events.len(),
        runtime_replayed_boundary_count: outcome.runtime_replayed_boundary_count,
        replayed_boundary_families: outcome.replayed_boundary_families,
        carried_forward_boundary_count: 0,
        checkpoint_replay: outcome.checkpoint_replay,
        reopened_sessions,
        final_summary: outcome.final_summary,
    };
    if let Some(report_path) = report_path {
        if let Some(parent) = report_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(report_path, serde_json::to_vec_pretty(&report)?)?;
    }
    Ok(report)
}

struct SqliteReplayBackend {
    database_root: PathBuf,
}

impl ReplayBackend for SqliteReplayBackend {
    type Error = SqliteReplayError;

    const TARGET: &str = "SQLite";
    const ASSERT_INGRESS_SESSION_ID: bool = true;

    fn session_store_factory(
        &self,
        clock: &Arc<crate::clock::SimClock>,
    ) -> Arc<dyn SessionStoreFactory> {
        let effect_replay_path = self.database_root.join("runtime-effects.sqlite");
        let process_registry_path = effect_replay_path.with_extension("process-registry.sqlite");
        Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new_with_process_registry(
                self.database_root.clone(),
                process_registry_path,
            )
            .with_clock(clock.clone()),
        )
    }

    fn effect_replay_store(&self) -> RuntimeEffectReplayStore {
        RuntimeEffectReplayStore::sqlite_file(self.database_root.join("runtime-effects.sqlite"))
    }

    async fn process_env_store(
        &self,
    ) -> Result<Arc<dyn lash::persistence::ProcessExecutionEnvStore>, Self::Error> {
        Ok(Arc::new(
            lash_sqlite_store::Store::open(&self.database_root.join("process-env.sqlite"))
                .await
                .map_err(|err| SqliteReplayError::Runtime(err.to_string()))?,
        ))
    }

    fn attachment_root(&self) -> PathBuf {
        self.database_root.join("attachments")
    }

    fn write_divergence_artifact(
        &self,
        trace_path: &Path,
        report_path: Option<&Path>,
        verdict: OracleVerdict,
        expected_summary: &AbstractWorldSummary,
        actual_summary: &AbstractWorldSummary,
        boundary: Option<BoundaryDivergence>,
    ) -> Result<(), Self::Error> {
        let Some(report_path) = report_path else {
            return Ok(());
        };
        let divergence_path = report_path.with_file_name("sqlite-divergence.json");
        let artifact = SqliteDivergenceArtifact {
            schema: SQLITE_DIVERGENCE_SCHEMA.to_string(),
            trace_path: trace_path.to_path_buf(),
            database_path: self.database_root.clone(),
            verdict,
            expected_summary: expected_summary.clone(),
            actual_summary: actual_summary.clone(),
            boundary: boundary.map(|boundary| SqliteBoundaryDivergence {
                boundary_id: boundary.boundary_id,
                boundary_kind: boundary.boundary_kind,
                expected_observed: boundary.expected_observed,
                actual_observed: boundary.actual_observed,
            }),
        };
        if let Some(parent) = divergence_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(divergence_path, serde_json::to_vec_pretty(&artifact)?)?;
        Ok(())
    }
}

fn prepare_database_root(path: &Path) -> Result<(), SqliteReplayError> {
    if path.exists() {
        if path.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else {
            std::fs::remove_file(path)?;
        }
    }
    std::fs::create_dir_all(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate_workload;
    use crate::runner::run_generated_workload_for_fixture;
    use std::collections::BTreeSet;

    #[test]
    fn sqlite_replay_report_schema_is_pinned() {
        assert_eq!(
            SQLITE_REPLAY_REPORT_SCHEMA,
            "lash.sim.sqlite-runtime-replay-report.v4"
        );
        assert_eq!(
            SQLITE_DIVERGENCE_SCHEMA,
            "lash.sim.sqlite-runtime-divergence.v1"
        );
    }

    #[tokio::test]
    async fn sqlite_replay_runs_trace_through_real_lash_sqlite_persistence() {
        let workload = generate_workload(7, "fast-random", 24).expect("workload");
        let trace = run_generated_workload_for_fixture(workload, "bundle")
            .await
            .expect("trace");
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("sqlite-store");
        let report_path = tmp.path().join("sqlite-replay.json");

        let report = replay_trace_to_sqlite(
            Path::new("trace.json"),
            &trace,
            &db_path,
            Some(&report_path),
        )
        .await
        .expect("sqlite replay");

        assert_eq!(report.schema, SQLITE_REPLAY_REPORT_SCHEMA);
        assert_eq!(report.delivered_event_count, trace.events.len());
        assert_eq!(report.runtime_replayed_boundary_count, trace.events.len());
        assert_eq!(report.carried_forward_boundary_count, 0);
        assert!(!report.checkpoint_replay.recorded_runtime.is_empty());
        assert_eq!(
            report.checkpoint_replay.recorded_runtime,
            report.checkpoint_replay.observed_runtime
        );
        assert!(
            !report.checkpoint_replay.carried.is_empty(),
            "contract and suspend fixture writes must be explicitly carried"
        );
        let families = report
            .replayed_boundary_families
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        for expected_family in [
            "ingress",
            "queued_ingress",
            "provider",
            "tool",
            "exec_code",
            "durable_effect",
            "process_wake",
            "worker",
            "observer",
            "cancellation",
            "trigger",
            "backend_failure",
            "provider_mutation",
            "lease_time",
        ] {
            assert!(
                families.contains(expected_family),
                "missing replayed family {expected_family}"
            );
        }
        assert_eq!(report.final_summary, trace.final_summary);
        assert!(!report.reopened_sessions.is_empty());
        assert!(
            report
                .reopened_sessions
                .iter()
                .all(|session| session.database_path.exists())
        );
        assert!(db_path.is_dir());
        assert!(report_path.exists());
    }
}
