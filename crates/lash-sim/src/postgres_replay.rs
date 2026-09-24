use lash_sansio::SessionId;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_postgres_store::PostgresStorage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::replay::ReplayError;
use crate::runtime_boundaries::RuntimeEffectReplayStore;
use crate::runtime_replay::{
    BackendReplayError, BoundaryDivergence, ReplayBackend, replay_trace_through_backend,
};
use crate::scheduler::BoundaryKind;
use crate::store::BackendCheckpointReplayEvidence;
use crate::trace::{
    AbstractWorldSummary, OracleVerdict, SimulationTrace, TraceIoError, read_trace,
};

pub const POSTGRES_REPLAY_REPORT_SCHEMA: &str = "lash.sim.postgres-runtime-replay-report.v5";
pub const POSTGRES_DIVERGENCE_SCHEMA: &str = "lash.sim.postgres-runtime-divergence.v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PostgresReplayReport {
    pub schema: String,
    pub trace_path: PathBuf,
    pub database_url_redacted: String,
    pub terminal_verdict: OracleVerdict,
    pub delivered_event_count: usize,
    pub runtime_replayed_boundary_count: usize,
    pub replayed_boundary_families: Vec<String>,
    pub carried_forward_boundary_count: usize,
    pub checkpoint_replay: BackendCheckpointReplayEvidence,
    pub effect_history_replay: PostgresEffectHistoryReplayEvidence,
    pub reopened_sessions: Vec<PostgresReopenedSessionEvidence>,
    pub final_summary: AbstractWorldSummary,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PostgresEffectHistoryReplayEvidence {
    pub status: String,
    pub native_controller: String,
    pub runtime_boundary_controller: String,
    pub store_table: String,
    pub replay_semantics: Vec<String>,
    pub conformance_evidence: Vec<String>,
    pub smallest_required_api_change: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PostgresDivergenceArtifact {
    pub schema: String,
    pub trace_path: PathBuf,
    pub database_url_redacted: String,
    pub verdict: OracleVerdict,
    pub expected_summary: AbstractWorldSummary,
    pub actual_summary: AbstractWorldSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<PostgresBoundaryDivergence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PostgresBoundaryDivergence {
    pub boundary_id: String,
    pub boundary_kind: String,
    pub expected_observed: Value,
    pub actual_observed: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PostgresReopenedSessionEvidence {
    pub session_id: SessionId,
    pub turn_index: usize,
    pub graph_node_count: usize,
    pub transcript_message_count: usize,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum PostgresReplayError {
    TraceIo(TraceIoError),
    Replay(ReplayError),
    Io(std::io::Error),
    Json(serde_json::Error),
    Runtime(String),
    Assertion(String),
    Divergence(String),
}

impl fmt::Display for PostgresReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TraceIo(err) => write!(f, "{err}"),
            Self::Replay(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "Postgres runtime replay I/O failed: {err}"),
            Self::Json(err) => write!(f, "Postgres runtime replay JSON failed: {err}"),
            Self::Runtime(message) => write!(f, "Postgres runtime replay failed: {message}"),
            Self::Assertion(message) => {
                write!(f, "Postgres runtime replay assertion failed: {message}")
            }
            Self::Divergence(message) => write!(f, "Postgres runtime replay diverged: {message}"),
        }
    }
}

impl std::error::Error for PostgresReplayError {}

impl From<TraceIoError> for PostgresReplayError {
    fn from(value: TraceIoError) -> Self {
        Self::TraceIo(value)
    }
}

impl From<ReplayError> for PostgresReplayError {
    fn from(value: ReplayError) -> Self {
        Self::Replay(value)
    }
}

impl From<std::io::Error> for PostgresReplayError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for PostgresReplayError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl BackendReplayError for PostgresReplayError {
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

pub async fn replay_trace_file_to_postgres(
    trace_path: &Path,
    database_url: &str,
    report_path: Option<&Path>,
) -> Result<PostgresReplayReport, PostgresReplayError> {
    let trace = read_trace(trace_path)?;
    replay_trace_to_postgres(trace_path, &trace, database_url, report_path).await
}

pub async fn replay_trace_to_postgres(
    trace_path: &Path,
    trace: &SimulationTrace,
    database_url: &str,
    report_path: Option<&Path>,
) -> Result<PostgresReplayReport, PostgresReplayError> {
    let storage = Arc::new(
        PostgresStorage::connect(database_url)
            .await
            .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?,
    );
    reset_postgres_for_replay(storage.as_ref()).await?;

    let attachment_root = report_path
        .and_then(Path::parent)
        .map(|parent| parent.join("postgres-attachments"))
        .unwrap_or_else(|| PathBuf::from("target/lash-sim/postgres-attachments"));
    let database_url_redacted = redact_database_url(database_url);
    let outcome = replay_trace_through_backend(
        PostgresReplayBackend {
            storage,
            attachment_root,
            database_url_redacted: database_url_redacted.clone(),
        },
        trace_path,
        trace,
        report_path,
    )
    .await?;
    let reopened_sessions = outcome
        .reopened_sessions
        .into_iter()
        .map(|observation| PostgresReopenedSessionEvidence {
            session_id: observation.session_id,
            turn_index: observation.turn_index,
            graph_node_count: observation.graph_node_count,
            transcript_message_count: observation.transcript_message_count,
        })
        .collect();
    let report = PostgresReplayReport {
        schema: POSTGRES_REPLAY_REPORT_SCHEMA.to_string(),
        trace_path: trace_path.to_path_buf(),
        database_url_redacted,
        terminal_verdict: outcome.terminal_verdict,
        delivered_event_count: trace.events.len(),
        runtime_replayed_boundary_count: outcome.runtime_replayed_boundary_count,
        replayed_boundary_families: outcome.replayed_boundary_families,
        carried_forward_boundary_count: 0,
        checkpoint_replay: outcome.checkpoint_replay,
        effect_history_replay: postgres_effect_history_replay_evidence(),
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

fn postgres_effect_history_replay_evidence() -> PostgresEffectHistoryReplayEvidence {
    PostgresEffectHistoryReplayEvidence {
        status: "native_postgres_runtime_effect_controller".to_string(),
        native_controller: "lash_postgres_store::PostgresRuntimeEffectController".to_string(),
        runtime_boundary_controller: "postgres_runtime_effect_controller".to_string(),
        store_table: "lash_runtime_effect_replay".to_string(),
        replay_semantics: vec![
            "scope_id_plus_replay_key_primary_key".to_string(),
            "stable_envelope_hash_conflict_rejection".to_string(),
            "lease_owner_and_token_fenced_finalize".to_string(),
            "completed_and_failed_outcome_replay".to_string(),
            "sleep_due_at_ms_preservation".to_string(),
        ],
        conformance_evidence: vec![
            "postgres_runtime_effect_controller_satisfies_conformance_when_configured".to_string(),
            "postgres_session_store_factory_reopens_generated_sessions".to_string(),
            "postgres_runtime_persistence_validates_queued_work_process_wakes".to_string(),
            "postgres_runtime_persistence_validates_session_execution_lease_failover".to_string(),
            "replay-postgres_runtime_boundaries_use_PostgresRuntimeEffectController".to_string(),
        ],
        smallest_required_api_change: "none".to_string(),
    }
}

struct PostgresReplayBackend {
    storage: Arc<PostgresStorage>,
    attachment_root: PathBuf,
    database_url_redacted: String,
}

impl ReplayBackend for PostgresReplayBackend {
    type Error = PostgresReplayError;

    const TARGET: &str = "Postgres";
    const ASSERT_INGRESS_SESSION_ID: bool = false;

    async fn backend(
        &self,
        clock: &Arc<crate::clock::SimClock>,
    ) -> Result<Arc<dyn lash::Backend>, Self::Error> {
        Ok(Arc::new(
            lash_postgres_store::PostgresBackend::with_options_and_clock(
                self.storage.as_ref(),
                Arc::new(lash::persistence::FileAttachmentStore::new(
                    self.attachment_root.clone(),
                )),
                crate::backend::sim_postgres_options(),
                clock.clone(),
            ),
        ))
    }

    fn effect_replay_store(&self) -> RuntimeEffectReplayStore {
        RuntimeEffectReplayStore::postgres(Arc::clone(&self.storage))
    }

    fn normalize_observed_extra(kind: BoundaryKind, value: &mut Value) {
        if matches!(
            kind,
            BoundaryKind::Tool | BoundaryKind::ExecCode | BoundaryKind::DurableEffect
        ) && let Some(controller) = value
            .as_object_mut()
            .and_then(|object| object.get_mut("runtime_effect"))
            .and_then(Value::as_object_mut)
            .and_then(|effect| effect.get_mut("controller"))
        {
            *controller = Value::String("<backend-runtime-effect-controller>".to_string());
        }
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
        let divergence_path = report_path.with_file_name("postgres-divergence.json");
        let artifact = PostgresDivergenceArtifact {
            schema: POSTGRES_DIVERGENCE_SCHEMA.to_string(),
            trace_path: trace_path.to_path_buf(),
            database_url_redacted: self.database_url_redacted.clone(),
            verdict,
            expected_summary: expected_summary.clone(),
            actual_summary: actual_summary.clone(),
            boundary: boundary.map(|boundary| PostgresBoundaryDivergence {
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

pub(crate) async fn reset_postgres_for_replay(
    storage: &PostgresStorage,
) -> Result<(), PostgresReplayError> {
    sqlx::query(
        r#"
        TRUNCATE
            lash_trigger_deliveries,
            lash_trigger_occurrences,
            lash_trigger_subscriptions,
            lash_process_wake_deliveries,
            lash_process_observers,
            lash_process_tombstones,
            lash_process_segment_handovers,
            lash_process_leases,
            lash_runtime_effect_replay,
            lash_process_events,
            lash_processes,
            lash_queued_work_items,
            lash_queued_work_batches,
            lash_pending_turn_inputs,
            lash_runtime_turn_commits,
            lash_session_execution_leases,
            lash_session_meta,
            lash_usage_deltas,
            lash_graph_nodes,
            lash_sessions,
            lash_attachment_manifest,
            lash_lashlang_artifacts,
            lash_blobs
        RESTART IDENTITY CASCADE
        "#,
    )
    .execute(storage.pool())
    .await
    .map_err(|err| PostgresReplayError::Runtime(err.to_string()))?;
    Ok(())
}

pub(crate) fn redact_database_url(database_url: &str) -> String {
    let Some((scheme, rest)) = database_url.split_once("://") else {
        return "[redacted]".to_string();
    };
    let host_and_path = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    format!("{scheme}://[redacted]@{host_and_path}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate_workload;
    use crate::runner::run_generated_workload_for_fixture;
    use crate::runtime_replay::normalize_backend_observed;
    use crate::store::ModelStore;
    use serde_json::json;

    #[test]
    fn postgres_replay_report_schema_is_pinned() {
        assert_eq!(
            POSTGRES_REPLAY_REPORT_SCHEMA,
            "lash.sim.postgres-runtime-replay-report.v5"
        );
        assert_eq!(
            POSTGRES_DIVERGENCE_SCHEMA,
            "lash.sim.postgres-runtime-divergence.v1"
        );
    }

    #[test]
    fn postgres_effect_history_evidence_claims_native_controller() {
        let evidence = postgres_effect_history_replay_evidence();

        assert_eq!(evidence.status, "native_postgres_runtime_effect_controller");
        assert_eq!(
            evidence.native_controller,
            "lash_postgres_store::PostgresRuntimeEffectController"
        );
        assert_eq!(
            evidence.runtime_boundary_controller,
            "postgres_runtime_effect_controller"
        );
        assert_eq!(evidence.store_table, "lash_runtime_effect_replay");
        assert_eq!(evidence.smallest_required_api_change, "none");
        assert!(
            evidence
                .replay_semantics
                .contains(&"stable_envelope_hash_conflict_rejection".to_string())
        );
    }

    #[test]
    fn postgres_normalization_ignores_backend_controller_identity_only() {
        let observed = json!({
            "runtime_effect": {
                "kind": "tool_attempt",
                "controller": "postgres_runtime_effect_controller",
                "local_executor_called": true,
            },
            "tool_output": "same",
        });

        let normalized =
            normalize_backend_observed::<PostgresReplayBackend>(BoundaryKind::Tool, &observed);

        assert_eq!(
            normalized["runtime_effect"]["controller"],
            "<backend-runtime-effect-controller>"
        );
        assert_eq!(normalized["runtime_effect"]["kind"], "tool_attempt");
        assert_eq!(normalized["tool_output"], "same");
    }

    #[tokio::test]
    async fn postgres_replay_observes_checkpoint_writes_when_configured() {
        // Replay against a database created for this test alone. Against the
        // shared database the conformance suites truncate this replay's session
        // metadata mid-run, which surfaces as a spurious commit failure.
        let Some(database) = crate::postgres_test_isolation::isolated_database().await else {
            return;
        };
        let database_url = database.url().to_string();
        let workload = generate_workload(7, "fast-random", 24).expect("workload");
        let mut trace = run_generated_workload_for_fixture(workload, "postgres-observer")
            .await
            .expect("trace");
        // Postgres session leases use transaction time by contract. Remove only
        // the simulator's instant clock-advance Worker proof; all runtime turns
        // and their checkpoint commits still execute against Postgres below.
        trace
            .events
            .retain(|event| event.kind != BoundaryKind::Worker);
        let mut model = ModelStore::default();
        for event in &trace.events {
            model.apply_observed_boundary(&event.as_event(), &event.observed);
        }
        trace.final_summary = model
            .summarize_with_trace_checkpoint_writes(&trace.events, &trace.durable_writes)
            .expect("summary");
        let tmp = tempfile::tempdir().expect("tempdir");
        let report_path = tmp.path().join("postgres-replay.json");

        let report = replay_trace_to_postgres(
            Path::new("trace.json"),
            &trace,
            &database_url,
            Some(&report_path),
        )
        .await
        .expect("Postgres replay");

        assert!(!report.checkpoint_replay.recorded_runtime.is_empty());
        assert_eq!(
            report.checkpoint_replay.recorded_runtime,
            report.checkpoint_replay.observed_runtime
        );
        assert!(report_path.exists());
    }
}
