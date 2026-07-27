//! The runtime's settled-session persistence contract and shared store types.

mod attachment_manifest;
mod commit_budget;
mod commit_identity;
mod error;
mod graph_commit;
mod incarnation;
mod lease_timings;
mod load;
pub mod queued_work;
mod realization;

pub use attachment_manifest::{
    AttachmentIntent, AttachmentManifest, AttachmentManifestEntry, AttachmentOwnerKind,
};
pub use commit_identity::{OperationId, derive_history_node_id, graph_realization_digest};
pub use error::StoreError;
pub use incarnation::{EphemeralRunId, IncarnationId, SessionLifetime};
pub use lease_timings::{LeaseTimings, LeaseTimingsError};
pub use load::{load_persisted_session_state, refresh_persisted_session_state};
pub use realization::{RealizedNodeTimestamp, commit_runtime_state_verified};

const PROC_BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

fn default_root_session_id() -> String {
    "root".to_string()
}

pub const SESSION_HEAD_META_SCHEMA_VERSION: u32 = 2;
pub const SESSION_CHECKPOINT_SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod persisted_state_tests {
    use super::*;

    #[test]
    fn persisted_state_hydrates_provider_id_without_live_provider_rebinding() {
        let state = persisted_session_state_from_head(
            SessionHead {
                session_id: "stored".to_string(),
                head_revision: 7,
                current_frame_node_id: None,
                graph: crate::SessionGraph::default(),
                config: crate::PersistedSessionConfig {
                    provider_id: "stored-provider".to_string(),
                    model: crate::ModelSpec::default(),
                },
                checkpoint_ref: None,
                token_ledger: Vec::new(),
            },
            None,
        );

        assert_eq!(state.policy.recorded_provider_id(), "stored-provider");
        assert_eq!(state.policy.recorded_provider_id(), "stored-provider");
        assert_eq!(state.head_revision, 7);
    }

    #[test]
    fn versioned_json_record_rejects_missing_schema_version() {
        let err = decode_versioned_json_record::<SessionHeadMeta>(
            "{}",
            "SessionHeadMeta",
            SESSION_HEAD_META_SCHEMA_VERSION,
        )
        .expect_err("pre-versioned session head should fail");

        assert!(matches!(
            err,
            StoreError::MissingRecordSchemaVersion {
                record_kind: "SessionHeadMeta",
                expected: SESSION_HEAD_META_SCHEMA_VERSION
            }
        ));
    }

    #[test]
    fn versioned_json_record_rejects_invalid_schema_version() {
        let err = decode_versioned_json_record::<SessionHeadMeta>(
            r#"{"schema_version":"1"}"#,
            "SessionHeadMeta",
            SESSION_HEAD_META_SCHEMA_VERSION,
        )
        .expect_err("invalid session head schema version should fail");

        assert!(matches!(
            err,
            StoreError::InvalidRecordSchemaVersion {
                record_kind: "SessionHeadMeta",
                expected: SESSION_HEAD_META_SCHEMA_VERSION,
                ..
            }
        ));
    }

    #[test]
    fn versioned_json_record_rejects_unsupported_schema_version() {
        let unsupported = SESSION_HEAD_META_SCHEMA_VERSION + 1;
        let err = decode_versioned_json_record::<SessionHeadMeta>(
            &format!(r#"{{"schema_version":{unsupported}}}"#),
            "SessionHeadMeta",
            SESSION_HEAD_META_SCHEMA_VERSION,
        )
        .expect_err("unsupported session head schema version should fail");

        assert!(matches!(
            err,
            StoreError::UnsupportedRecordSchemaVersion {
                record_kind: "SessionHeadMeta",
                actual,
                expected: SESSION_HEAD_META_SCHEMA_VERSION
            } if actual == unsupported
        ));
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub incarnation_id: IncarnationId,
    pub session_name: String,
    pub created_at: String,
    pub model: String,
    pub cwd: Option<String>,
    pub relation: crate::SessionRelation,
}

impl SessionMeta {
    /// Returns the parent session id, if any, derived from the canonical
    /// [`SessionRelation`] field.
    pub fn parent_session_id(&self) -> Option<&str> {
        self.relation.parent_session_id()
    }
}

/// Lightweight session info for the resume picker.
#[derive(Clone, Debug)]
pub struct SessionPickerInfo {
    pub session_id: String,
    pub cwd: Option<String>,
    pub relation: crate::SessionRelation,
    pub first_user_message: String,
    pub user_message_count: usize,
}

impl SessionPickerInfo {
    pub fn parent_session_id(&self) -> Option<&str> {
        self.relation.parent_session_id()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct BlobRef(pub String);

impl BlobRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BlobRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for BlobRef {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub root_count: usize,
    pub retained_blob_count: usize,
    pub deleted_blob_count: usize,
}

/// Result of a `StoreMaintenance::vacuum()` call.
/// `removed_node_count` counts the tombstoned graph-node rows that were
/// physically deleted from the store. `removed_pending_turn_input_tombstone_count`
/// counts terminal pending-input evidence rows pruned by host-scheduled
/// retention. Returned so hosts can emit metrics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VacuumReport {
    pub removed_node_count: usize,
    pub removed_pending_turn_input_tombstone_count: usize,
}

/// Result of comparing cached node counts with references re-derived from the
/// indexed edge and root rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeRefcountVerification {
    pub checked_node_count: usize,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionCheckpoint {
    pub schema_version: u32,
    pub turn_state: crate::PersistedTurnState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_state_ref: Option<BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_snapshot_ref: Option<BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_snapshot_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_state_ref: Option<BlobRef>,
}

impl Default for SessionCheckpoint {
    fn default() -> Self {
        Self {
            schema_version: SESSION_CHECKPOINT_SCHEMA_VERSION,
            turn_state: crate::PersistedTurnState::default(),
            tool_state_ref: None,
            plugin_snapshot_ref: None,
            plugin_snapshot_revision: None,
            execution_state_ref: None,
        }
    }
}

impl SessionCheckpoint {
    pub fn new(
        turn_state: crate::PersistedTurnState,
        tool_state_ref: Option<BlobRef>,
        plugin_snapshot_ref: Option<BlobRef>,
        plugin_snapshot_revision: Option<u64>,
        execution_state_ref: Option<BlobRef>,
    ) -> Self {
        Self {
            schema_version: SESSION_CHECKPOINT_SCHEMA_VERSION,
            turn_state,
            tool_state_ref,
            plugin_snapshot_ref,
            plugin_snapshot_revision,
            execution_state_ref,
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct HydratedSessionCheckpoint {
    pub turn_state: crate::PersistedTurnState,
    pub tool_state_ref: Option<BlobRef>,
    pub tool_state: Option<crate::ToolState>,
    pub plugin_snapshot_ref: Option<BlobRef>,
    pub plugin_snapshot: Option<crate::PluginSessionSnapshot>,
    pub plugin_snapshot_revision: Option<u64>,
    pub execution_state_ref: Option<BlobRef>,
    pub execution_state: Option<Vec<u8>>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionHead {
    #[serde(default = "default_root_session_id")]
    pub session_id: String,
    #[serde(skip)]
    pub head_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_node_id: Option<String>,
    pub graph: crate::SessionGraph,
    pub config: crate::PersistedSessionConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_ref: Option<BlobRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_ledger: Vec<crate::TokenLedgerEntry>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionHeadMeta {
    pub schema_version: u32,
    #[serde(default = "default_root_session_id")]
    pub session_id: String,
    #[serde(skip)]
    pub head_revision: u64,
    pub config: crate::PersistedSessionConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_node_id: Option<String>,
    #[serde(skip)]
    pub checkpoint_ref: Option<BlobRef>,
    #[serde(skip)]
    pub leaf_node_id: Option<String>,
}

fn persisted_session_config_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::PersistedSessionConfig {
    crate::PersistedSessionConfig {
        provider_id: state.policy.recorded_provider_id().to_string(),
        model: state.policy.model.clone(),
    }
}

#[derive(Clone, Debug)]
pub struct PersistedSessionRead {
    pub session_id: String,
    pub head_revision: u64,
    pub config: crate::PersistedSessionConfig,
    pub current_frame_node_id: Option<String>,
    pub graph: crate::SessionGraph,
    pub checkpoint_ref: Option<BlobRef>,
    pub checkpoint: Option<HydratedSessionCheckpoint>,
    pub token_ledger: Vec<crate::TokenLedgerEntry>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct GraphAppend {
    pub nodes: Vec<crate::SessionNodeRecord>,
    pub leaf_node_id: Option<String>,
}

impl GraphAppend {
    pub fn leaf_node_id(&self) -> Option<&String> {
        self.leaf_node_id.as_ref()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeCommit {
    pub session_id: String,
    pub session_lifetime: SessionLifetime,
    pub expected_head_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_session_execution_lease: Option<SessionExecutionLeaseCompletion>,
    pub config: crate::PersistedSessionConfig,
    pub current_frame_node_id: Option<String>,
    pub graph: GraphAppend,
    pub checkpoint: HydratedSessionCheckpoint,
    pub usage_deltas: Vec<crate::TokenLedgerEntry>,
    pub turn_commit: RuntimeTurnCommitStamp,
    pub completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
    pub completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_turn_input_turn_id: Option<String>,
    /// Attachment ids explicitly adopted by this commit. In the same
    /// transaction the backend also stamps every uncommitted manifest row owned
    /// by the turn id in `turn_commit.operation`, including ids that appear only in plain tool
    /// JSON. This list preserves typed-output and cross-turn re-references;
    /// adoption updates existing rows only and deliberately no-ops when this
    /// session has no matching intent.
    pub committed_attachment_ids: Vec<crate::AttachmentId>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeCommitResult {
    pub head_revision: u64,
    pub checkpoint_ref: BlobRef,
    pub manifest: SessionCheckpoint,
    /// Store-recorded digest of the graph proposal accepted for this operation.
    ///
    /// On a receipt hit this compares the retry proposal with the first
    /// attempt's recorded proposal independently of node derivation. Physical
    /// row realization still relies on the backend transaction being atomic.
    pub realization_digest: String,
    /// Store-realized timestamps for nodes appended by this operation.
    ///
    /// Node timestamps are clock-derived and excluded from commit intent, so a
    /// receipt replay must return the first attempt's values for the resident
    /// graph to converge with durable history.
    pub realized_node_timestamps: Vec<RealizedNodeTimestamp>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enqueued_queue_batches: Vec<crate::QueuedWorkBatch>,
    /// Canonical input applications settled by this idempotent turn commit.
    ///
    /// Keeping these identities in the durable turn-commit result lets hosts
    /// reconcile after the bounded live observation window has been lost.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_input_applications: Vec<crate::TurnInputApplication>,
}

/// Stable identity for the holder of a session-execution lease.
///
/// Callers using [`Self::local_process`] must choose a `host_id` that uniquely
/// identifies one PID namespace among all lease contenders sharing a store.
/// Reusing an image-baked machine id across containers can make a peer inspect
/// its own PID namespace and falsely fence a live owner as definitely dead.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeaseOwnerIdentity {
    pub owner_id: String,
    pub incarnation_id: String,
    #[serde(default)]
    pub liveness: LeaseOwnerLiveness,
}

impl LeaseOwnerIdentity {
    pub fn opaque(
        owner_id: impl Into<String>,
        incarnation_id: impl Into<String>,
    ) -> LeaseOwnerIdentity {
        LeaseOwnerIdentity {
            owner_id: owner_id.into(),
            incarnation_id: incarnation_id.into(),
            liveness: LeaseOwnerLiveness::Opaque,
        }
    }

    /// Stable owner identity for one Restate process execution invocation.
    ///
    /// Construction and recognition share this single representation so a
    /// formatting drift cannot silently turn a continuation into a fresh
    /// execution.
    pub fn restate_process_execution(
        process_id: &str,
        execution_id: impl Into<String>,
    ) -> LeaseOwnerIdentity {
        Self::opaque(format!("restate:{process_id}"), execution_id)
    }

    /// Return the Restate execution id when this owner belongs to `process_id`.
    pub fn restate_process_execution_id(&self, process_id: &str) -> Option<&str> {
        let expected = Self::restate_process_execution(process_id, &self.incarnation_id);
        self.same_incarnation(&expected)
            .then_some(self.incarnation_id.as_str())
    }

    pub fn local_process(
        owner_id: impl Into<String>,
        incarnation_id: impl Into<String>,
        host_id: impl Into<String>,
    ) -> LeaseOwnerIdentity {
        let liveness = LeaseOwnerLiveness::current_local_process(host_id.into())
            .unwrap_or(LeaseOwnerLiveness::Opaque);
        LeaseOwnerIdentity {
            owner_id: owner_id.into(),
            incarnation_id: incarnation_id.into(),
            liveness,
        }
    }

    pub fn same_incarnation(&self, other: &LeaseOwnerIdentity) -> bool {
        self.owner_id == other.owner_id && self.incarnation_id == other.incarnation_id
    }

    pub fn is_definitely_dead_for_claimant(&self, claimant: &LeaseOwnerIdentity) -> bool {
        self.liveness
            .is_definitely_dead_for_claimant(&claimant.liveness)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeaseOwnerLiveness {
    LocalProcess {
        host_id: String,
        boot_id: String,
        pid: u32,
        process_start: String,
    },
    #[default]
    Opaque,
}

impl LeaseOwnerLiveness {
    pub fn current_local_process(host_id: impl Into<String>) -> Option<LeaseOwnerLiveness> {
        let boot_id = std::fs::read_to_string(PROC_BOOT_ID_PATH)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())?;
        let pid = std::process::id();
        let process_start = read_linux_process_start(pid)?;
        Some(LeaseOwnerLiveness::LocalProcess {
            host_id: host_id.into(),
            boot_id,
            pid,
            process_start,
        })
    }

    pub fn local_process_for_test(
        host_id: impl Into<String>,
        boot_id: impl Into<String>,
        pid: u32,
        process_start: impl Into<String>,
    ) -> LeaseOwnerLiveness {
        LeaseOwnerLiveness::LocalProcess {
            host_id: host_id.into(),
            boot_id: boot_id.into(),
            pid,
            process_start: process_start.into(),
        }
    }

    pub fn is_definitely_dead_for_claimant(&self, claimant: &LeaseOwnerLiveness) -> bool {
        let (
            LeaseOwnerLiveness::LocalProcess {
                host_id,
                boot_id,
                pid,
                process_start,
            },
            LeaseOwnerLiveness::LocalProcess {
                host_id: claimant_host_id,
                boot_id: claimant_boot_id,
                ..
            },
        ) = (self, claimant)
        else {
            return false;
        };
        if host_id != claimant_host_id || boot_id != claimant_boot_id {
            return false;
        }
        matches!(linux_process_is_live(*pid, process_start), Some(false))
    }
}

fn read_linux_process_start(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_linux_process_start(&stat)
}

fn linux_process_is_live(pid: u32, expected_process_start: &str) -> Option<bool> {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => parse_linux_process_start(&stat).map(|start| start == expected_process_start),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

fn parse_linux_process_start(stat: &str) -> Option<String> {
    let after_comm = stat.rsplit_once(") ")?.1;
    after_comm.split_whitespace().nth(19).map(ToOwned::to_owned)
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionExecutionLease {
    pub session_id: String,
    pub owner: LeaseOwnerIdentity,
    pub lease_token: String,
    pub fencing_token: u64,
    pub claimed_at_epoch_ms: u64,
    pub expires_at_epoch_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionExecutionLeaseFence {
    pub session_id: String,
    pub owner: LeaseOwnerIdentity,
    pub lease_token: String,
    pub fencing_token: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionExecutionLeaseCompletion {
    pub session_id: String,
    pub owner: LeaseOwnerIdentity,
    pub lease_token: String,
    pub fencing_token: u64,
}

impl SessionExecutionLease {
    pub fn fence(&self) -> SessionExecutionLeaseFence {
        SessionExecutionLeaseFence {
            session_id: self.session_id.clone(),
            owner: self.owner.clone(),
            lease_token: self.lease_token.clone(),
            fencing_token: self.fencing_token,
        }
    }

    pub fn completion(&self) -> SessionExecutionLeaseCompletion {
        SessionExecutionLeaseCompletion {
            session_id: self.session_id.clone(),
            owner: self.owner.clone(),
            lease_token: self.lease_token.clone(),
            fencing_token: self.fencing_token,
        }
    }
}

impl SessionExecutionLeaseCompletion {
    pub fn from_lease(lease: &SessionExecutionLease) -> Self {
        lease.completion()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionExecutionLeaseClaimOutcome {
    Acquired(SessionExecutionLease),
    Busy { holder: SessionExecutionLease },
}

impl SessionExecutionLeaseClaimOutcome {
    pub fn acquired(self) -> Option<SessionExecutionLease> {
        match self {
            Self::Acquired(lease) => Some(lease),
            Self::Busy { .. } => None,
        }
    }
}

/// Reject a persisted record whose `schema_version` does not match the
/// version this binary supports. Backends call this immediately after
/// deserializing a record from durable storage.
pub fn ensure_supported_schema_version(
    record_kind: &'static str,
    actual: u32,
    expected: u32,
) -> Result<(), StoreError> {
    if actual == expected {
        Ok(())
    } else {
        Err(StoreError::UnsupportedRecordSchemaVersion {
            record_kind,
            actual,
            expected,
        })
    }
}

pub fn ensure_supported_record_schema_version(
    record_kind: &'static str,
    value: &serde_json::Value,
    expected: u32,
) -> Result<(), StoreError> {
    let Some(schema_version) = value.get("schema_version") else {
        return Err(StoreError::MissingRecordSchemaVersion {
            record_kind,
            expected,
        });
    };
    let Some(actual) = schema_version
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
    else {
        return Err(StoreError::InvalidRecordSchemaVersion {
            record_kind,
            actual: schema_version.to_string(),
            expected,
        });
    };
    ensure_supported_schema_version(record_kind, actual, expected)
}

pub fn decode_versioned_json_record<T>(
    json: &str,
    record_kind: &'static str,
    expected: u32,
) -> Result<T, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| StoreError::Backend(format!("failed to decode {record_kind}: {err}")))?;
    ensure_supported_record_schema_version(record_kind, &value, expected)?;
    serde_json::from_value(value)
        .map_err(|err| StoreError::Backend(format!("failed to decode {record_kind}: {err}")))
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeTurnCommitStamp {
    pub session_id: String,
    pub operation: OperationId,
    pub turn_commit_hash: String,
}

impl RuntimeTurnCommitStamp {
    pub fn new(
        session_id: impl Into<String>,
        operation: OperationId,
        turn_commit_hash: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            operation,
            turn_commit_hash: turn_commit_hash.into(),
        }
    }
}

fn build_persisted_turn_state(state: &crate::RuntimeSessionState) -> crate::PersistedTurnState {
    crate::PersistedTurnState {
        turn_index: state.turn_index,
        token_usage: state.token_usage.clone(),
        last_prompt_usage: state.last_prompt_usage.clone(),
        protocol_turn_options: state.protocol_turn_options.clone(),
    }
}

fn build_checkpoint_from_persisted_state(
    state: &crate::RuntimeSessionState,
) -> HydratedSessionCheckpoint {
    HydratedSessionCheckpoint {
        turn_state: build_persisted_turn_state(state),
        tool_state_ref: state.tool_state_ref.clone(),
        tool_state: state.tool_state_snapshot.clone(),
        plugin_snapshot_ref: state.plugin_snapshot_ref.clone(),
        plugin_snapshot_revision: state.plugin_snapshot_revision,
        plugin_snapshot: state.plugin_snapshot.clone(),
        execution_state_ref: state.execution_state_ref.clone(),
        execution_state: state.execution_state_snapshot.clone(),
    }
}

impl RuntimeCommit {
    pub fn durable_incarnation_id(
        &self,
        boundary: &'static str,
    ) -> Result<&IncarnationId, StoreError> {
        self.session_lifetime.as_durable().ok_or_else(|| {
            StoreError::EphemeralSessionAtDurableBoundary {
                session_id: self.session_id.clone(),
                boundary,
            }
        })
    }

    pub fn validate_operation_session(&self) -> Result<(), StoreError> {
        let completed = &self.turn_commit;
        completed
            .operation
            .scope
            .validate()
            .map_err(|err| StoreError::Backend(err.to_string()))?;
        if completed.operation.key.trim().is_empty() {
            return Err(StoreError::Backend(
                "commit operation identity requires a non-empty key".to_string(),
            ));
        }
        if completed.session_id != self.session_id
            || completed
                .operation
                .scope
                .session_id()
                .is_some_and(|session_id| session_id != self.session_id)
        {
            return Err(StoreError::RuntimeTurnCommitConflict {
                session_id: completed.session_id.clone(),
                turn_id: completed.operation.storage_key()?,
            });
        }
        Ok(())
    }

    pub fn validate_node_derivation(
        &self,
        durable_incarnation_id: &IncarnationId,
    ) -> Result<(), StoreError> {
        let commit_incarnation_id = self.durable_incarnation_id("runtime commit validation")?;
        if commit_incarnation_id != durable_incarnation_id {
            return Err(StoreError::SessionIncarnationMismatch {
                session_id: self.session_id.clone(),
                expected_incarnation_id: durable_incarnation_id.to_string(),
                actual_incarnation_id: commit_incarnation_id.to_string(),
            });
        }
        let completed = &self.turn_commit;
        for (ordinal, node) in self.graph.nodes.iter().enumerate() {
            let expected = match &node.payload {
                crate::SessionNodePayload::FrameOpen { frame_key, .. } => {
                    crate::session_graph::frame_node_id(
                        &self.session_id,
                        durable_incarnation_id,
                        frame_key,
                    )
                }
                _ => derive_history_node_id(
                    durable_incarnation_id,
                    &completed.operation,
                    ordinal as u64,
                )?,
            };
            if node.node_id != expected {
                return Err(StoreError::NodeIdDerivationMismatch {
                    node_id: node.node_id.clone(),
                    expected_node_id: expected,
                });
            }
        }
        Ok(())
    }

    pub fn validate_append_node_ids_unique(&self) -> Result<(), StoreError> {
        let mut seen = std::collections::HashSet::with_capacity(self.graph.nodes.len());
        for node in &self.graph.nodes {
            if !seen.insert(node.node_id.as_str()) {
                return Err(StoreError::NodeIdCollision {
                    node_id: node.node_id.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn turn_input_applications(&self) -> Vec<crate::TurnInputApplication> {
        self.completed_turn_input_claims
            .iter()
            .flat_map(|completion| completion.applications.iter().cloned())
            .collect()
    }

    pub(crate) fn validate_claim_settlement(
        &self,
        originating_queue_claims: &[crate::QueuedWorkCompletion],
        originating_turn_input_claims: &[crate::TurnInputCompletion],
    ) -> Result<(), StoreError> {
        for originating in originating_queue_claims {
            if !self.completed_queue_claims.iter().any(|completed| {
                completed.session_id == originating.session_id
                    && completed.claim_id == originating.claim_id
            }) {
                return Err(StoreError::UnsettledQueuedWorkClaim {
                    session_id: originating.session_id.clone(),
                    claim_id: originating.claim_id.clone(),
                });
            }
        }
        for originating in originating_turn_input_claims {
            if !self.completed_turn_input_claims.iter().any(|completed| {
                completed.session_id == originating.session_id
                    && completed.claim_id == originating.claim_id
            }) {
                return Err(StoreError::UnsettledTurnInputClaim {
                    session_id: originating.session_id.clone(),
                    claim_id: originating.claim_id.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn turn_commit_hash(&self) -> Result<String, StoreError> {
        commit_identity::turn_commit_hash(self)
    }

    #[doc(hidden)]
    #[track_caller]
    pub fn persisted_state_for_test(
        state: &crate::RuntimeSessionState,
        usage_deltas: &[crate::TokenLedgerEntry],
    ) -> Self {
        let caller = std::panic::Location::caller();
        let operation = OperationId::new(
            crate::ExecutionScope::runtime_operation(format!(
                "test-commit:{}:{}:{}",
                caller.file(),
                caller.line(),
                state.head_revision
            )),
            "commit",
        );
        let mut graph = state.pending_graph_commit();
        graph
            .derive_node_ids(
                &state.session_id,
                state
                    .durable_incarnation_id("test runtime commit")
                    .expect("test runtime commit requires a durable session incarnation"),
                &operation,
            )
            .expect("test commit node ids must be derivable");
        Self::persisted_state_with_graph_commit_and_operation(state, graph, usage_deltas, operation)
            .expect("test commit must be hashable")
    }

    pub(crate) fn persisted_state_with_operation(
        state: &mut crate::RuntimeSessionState,
        usage_deltas: &[crate::TokenLedgerEntry],
        operation: OperationId,
    ) -> Result<(Self, Vec<String>), StoreError> {
        let mut graph = state.pending_graph_commit();
        let mapping = graph.derive_node_ids(
            &state.session_id,
            state.durable_incarnation_id("history node derivation")?,
            &operation,
        )?;
        state
            .session_graph
            .remap_node_ids(&state.session_id, &mapping);
        remap_optional_node_id(&mut state.current_frame_node_id, &mapping);
        state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
        let persisted_node_ids = mapping.iter().map(|(_, derived)| derived.clone()).collect();
        let commit = Self::persisted_state_with_graph_commit_and_operation(
            state,
            graph,
            usage_deltas,
            operation,
        )?;
        Ok((commit, persisted_node_ids))
    }

    #[doc(hidden)]
    #[track_caller]
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn persisted_state_with_graph_commit(
        state: &crate::RuntimeSessionState,
        mut graph: GraphAppend,
        usage_deltas: &[crate::TokenLedgerEntry],
    ) -> Self {
        let caller = std::panic::Location::caller();
        let operation = OperationId::new(
            crate::ExecutionScope::runtime_operation(format!(
                "test-graph-commit:{}:{}:{}",
                caller.file(),
                caller.line(),
                state.head_revision
            )),
            "commit",
        );
        graph
            .derive_node_ids(
                &state.session_id,
                state
                    .durable_incarnation_id("test graph commit")
                    .expect("test graph commit requires a durable session incarnation"),
                &operation,
            )
            .expect("test graph commit node ids must be derivable");
        Self::persisted_state_with_graph_commit_and_operation(state, graph, usage_deltas, operation)
            .expect("test graph commit must be hashable")
    }

    pub(crate) fn persisted_state_with_graph_commit_and_operation(
        state: &crate::RuntimeSessionState,
        graph: GraphAppend,
        usage_deltas: &[crate::TokenLedgerEntry],
        operation: OperationId,
    ) -> Result<Self, StoreError> {
        let mut projected_graph = state.session_graph.clone();
        for node in &graph.nodes {
            if projected_graph.find_node(&node.node_id).is_none() {
                projected_graph.extend_node_records(std::iter::once(node.clone()));
            }
        }
        projected_graph.set_leaf_node_id(graph.leaf_node_id().cloned());
        let current_frame_node_id = projected_graph
            .nearest_frame_node_id(projected_graph.leaf_node_id.as_deref())
            .map(ToOwned::to_owned);
        let mut commit = Self {
            session_id: state.session_id.clone(),
            session_lifetime: state.session_lifetime.clone(),
            expected_head_revision: state.head_revision,
            release_session_execution_lease: None,
            config: persisted_session_config_from_state(state),
            current_frame_node_id,
            graph,
            checkpoint: build_checkpoint_from_persisted_state(state),
            usage_deltas: usage_deltas.to_vec(),
            turn_commit: RuntimeTurnCommitStamp::new(
                state.session_id.clone(),
                operation,
                String::new(),
            ),
            completed_queue_claims: Vec::new(),
            completed_turn_input_claims: Vec::new(),
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            committed_attachment_ids: Vec::new(),
        };
        commit.turn_commit.turn_commit_hash = commit.turn_commit_hash()?;
        Ok(commit)
    }

    /// Derive append-node identities, stamp the operation, and return the
    /// old-to-derived id mapping so callers can remap any resident graph that
    /// supplied the commit.
    pub fn with_operation(
        mut self,
        operation: OperationId,
    ) -> Result<(Self, Vec<(String, String)>), StoreError> {
        let session_id = self.session_id.clone();
        let incarnation_id = self
            .durable_incarnation_id("history node derivation")?
            .clone();
        let node_id_mapping =
            self.graph
                .derive_node_ids(&session_id, &incarnation_id, &operation)?;
        remap_optional_node_id(&mut self.current_frame_node_id, &node_id_mapping);
        self.turn_commit =
            RuntimeTurnCommitStamp::new(self.session_id.clone(), operation, String::new());
        let hash = self.turn_commit_hash()?;
        self.turn_commit.turn_commit_hash = hash;
        Ok((self, node_id_mapping))
    }

    pub fn releasing_session_execution_lease(
        mut self,
        completion: SessionExecutionLeaseCompletion,
    ) -> Self {
        self.release_session_execution_lease = Some(completion);
        self
    }

    pub fn completing_queue_claim(
        mut self,
        completed_queue_claim: crate::QueuedWorkCompletion,
    ) -> Self {
        self.completed_queue_claims.push(completed_queue_claim);
        self
    }

    pub fn completing_queue_claims(
        mut self,
        completed_queue_claims: impl IntoIterator<Item = crate::QueuedWorkCompletion>,
    ) -> Self {
        self.completed_queue_claims.extend(completed_queue_claims);
        self
    }

    pub fn completing_turn_input_claim(
        mut self,
        completed_turn_input_claim: crate::TurnInputCompletion,
    ) -> Self {
        self.completed_turn_input_claims
            .push(completed_turn_input_claim);
        self
    }

    pub fn completing_turn_input_claims(
        mut self,
        completed_turn_input_claims: impl IntoIterator<Item = crate::TurnInputCompletion>,
    ) -> Self {
        self.completed_turn_input_claims
            .extend(completed_turn_input_claims);
        self
    }

    pub fn deferring_interrupted_turn_inputs(mut self, turn_id: impl Into<String>) -> Self {
        self.interrupted_turn_input_turn_id = Some(turn_id.into());
        self
    }

    pub fn with_committed_attachments(
        mut self,
        attachment_ids: impl IntoIterator<Item = crate::AttachmentId>,
    ) -> Self {
        self.committed_attachment_ids = attachment_ids.into_iter().collect();
        self
    }
}

fn remap_optional_node_id(node_id: &mut Option<String>, mapping: &[(String, String)]) {
    let Some(current) = node_id.as_mut() else {
        return;
    };
    if let Some((_, derived)) = mapping.iter().find(|(draft, _)| draft == current) {
        *current = derived.clone();
    }
}

fn persisted_session_state_from_head(
    head: SessionHead,
    checkpoint: Option<HydratedSessionCheckpoint>,
) -> crate::RuntimeSessionState {
    let persisted_node_ids = head
        .graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect();
    let graph = head.graph;
    let agent_frames = graph.agent_frame_records(&head.session_id);
    let mut state = crate::RuntimeSessionState {
        session_id: head.session_id,
        session_lifetime: SessionLifetime::default(),
        policy: crate::SessionPolicy::default(),
        agent_frames,
        current_frame_node_id: head.current_frame_node_id,
        session_graph: graph,
        turn_index: 0,
        token_usage: crate::TokenUsage::default(),
        last_prompt_usage: None,
        protocol_turn_options: crate::ProtocolTurnOptions::default(),
        tool_state_ref: None,
        tool_state_generation: None,
        tool_state_snapshot: None,
        plugin_snapshot_ref: None,
        plugin_snapshot_revision: None,
        plugin_snapshot: None,
        execution_state_ref: None,
        execution_state_snapshot: None,
        token_ledger: head.token_ledger,
        checkpoint_ref: head.checkpoint_ref.clone(),
        head_revision: head.head_revision,
        persisted_node_ids,
    };
    state.policy.model = head.config.model.clone();
    state.policy.provider_id = head.config.provider_id.clone();
    if let Some(checkpoint) = checkpoint {
        state.turn_index = checkpoint.turn_state.turn_index;
        state.token_usage = checkpoint.turn_state.token_usage;
        state.last_prompt_usage = checkpoint.turn_state.last_prompt_usage;
        state.protocol_turn_options = checkpoint.turn_state.protocol_turn_options;
        state.tool_state_ref = checkpoint.tool_state_ref.clone();
        state.tool_state_generation = checkpoint
            .tool_state
            .as_ref()
            .map(|snapshot| snapshot.generation());
        state.tool_state_snapshot = checkpoint.tool_state;
        state.plugin_snapshot_ref = checkpoint.plugin_snapshot_ref.clone();
        state.plugin_snapshot_revision = checkpoint.plugin_snapshot_revision;
        state.plugin_snapshot = checkpoint.plugin_snapshot;
        state.execution_state_ref = checkpoint.execution_state_ref.clone();
        state.execution_state_snapshot = checkpoint.execution_state;
    }
    state.ensure_agent_frame_initialized();
    state
}

impl Default for SessionHead {
    fn default() -> Self {
        Self {
            session_id: default_root_session_id(),
            head_revision: 0,
            current_frame_node_id: None,
            graph: crate::SessionGraph::default(),
            config: crate::PersistedSessionConfig::default(),
            checkpoint_ref: None,
            token_ledger: Vec::new(),
        }
    }
}

impl Default for SessionHeadMeta {
    fn default() -> Self {
        Self {
            schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
            session_id: default_root_session_id(),
            head_revision: 0,
            config: crate::PersistedSessionConfig::default(),
            current_frame_node_id: None,
            checkpoint_ref: None,
            leaf_node_id: None,
        }
    }
}

/// Settled-session commit/read capability: the runtime's atomic transaction
/// facade for visible session state.
///
/// This segment owns session graph/head commits, checkpoint hydration and
/// usage, final turn-commit idempotency, session metadata, and the attachment
/// write-ahead manifest. Queued-work and turn-input *completions* also settle
/// here — [`commit_runtime_state`](Self::commit_runtime_state) consumes claims
/// granted by [`QueuedWorkStore`] and [`TurnInputStore`] in the same atomic
/// commit. In-flight nondeterministic work belongs to the active
/// [`EffectHost`](crate::EffectHost), not to the store contract.
///
/// The [`AttachmentManifest`] supertrait is required so the runtime can wrap
/// any persistence backend with a
/// [`SessionAttachmentStore`](crate::SessionAttachmentStore)
/// without dual-trait casting. Backends with no attachment-write story can
/// paste no-op manifest impls via [`impl_noop_attachment_manifest!`].
#[async_trait::async_trait]
pub trait SessionCommitStore: AttachmentManifest + Send + Sync {
    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError>;

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, StoreError>;

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitResult, StoreError>;

    /// Create the session metadata row if absent, then return the identity
    /// read back from the store.
    ///
    /// Implementations must mint inside this guarded store operation. The
    /// returned value, not a caller-local candidate, is the only identity a
    /// persistent runtime may bind into its state.
    async fn ensure_session_incarnation(
        &self,
        session_id: &str,
        policy: &crate::SessionPolicy,
    ) -> Result<IncarnationId, StoreError>;

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError>;
    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError>;
}

/// Pending turn-input lifecycle capability: durable ingress for model-visible
/// user input.
///
/// Active-turn ingress is claimed only by the matching live turn at a
/// checkpoint. Next-turn ingress is claimed only by idle dispatch. User input
/// must not be represented as generic queued work. Claims granted here are
/// completed atomically by [`SessionCommitStore::commit_runtime_state`].
#[async_trait::async_trait]
pub trait TurnInputStore: Send + Sync {
    /// Persist model-visible user input into the pending turn-input lifecycle.
    async fn enqueue_pending_turn_input(
        &self,
        input: crate::PendingTurnInputDraft,
    ) -> Result<crate::PendingTurnInput, StoreError>;

    /// List pending user inputs for UI reconciliation and queue preview.
    ///
    /// This excludes completed/cancelled rows and rows currently held by a live
    /// claim. Expired claims are visible again according to their state.
    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::PendingTurnInput>, StoreError>;

    /// Read canonical input applications from durable turn-commit records.
    ///
    /// Unlike live observation replay, this surface is not retention-window
    /// dependent. Implementations return settled applications in durable
    /// commit order so a host can reconcile admission identity after a gap.
    async fn list_turn_input_applications(
        &self,
        _session_id: &str,
    ) -> Result<Vec<crate::TurnInputApplication>, StoreError> {
        Err(StoreError::Backend(
            "turn input application reconciliation is not implemented by this store".to_string(),
        ))
    }

    /// Cancel an unclaimed pending user input by id.
    ///
    /// Provided convenience: the singular form is exactly
    /// [`cancel_pending_turn_inputs`](Self::cancel_pending_turn_inputs) with a
    /// one-element target list, so backends implement only the plural
    /// primitive.
    async fn cancel_pending_turn_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<crate::PendingTurnInputCancelOutcome, StoreError> {
        let target = crate::PendingTurnInputCancelTarget::input_id(input_id);
        let targets = vec![target];
        let mut outcomes = self
            .cancel_pending_turn_inputs(session_id, &targets)
            .await?;
        Ok(outcomes
            .pop()
            .map(|result| result.outcome)
            .unwrap_or(crate::PendingTurnInputCancelOutcome::NotFound))
    }

    /// Atomically cancel a list of pending user inputs by input id or source key.
    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelResult>, StoreError>;

    /// Atomically cancel the same-session runtime-admission suffix from an anchor.
    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, StoreError>;

    /// Claim active-turn input at a checkpoint for the live turn id.
    ///
    /// The claim pins the caller's live session-execution-lease generation
    /// (`session_execution_lease.fencing_token`) rather than a TTL; it is live
    /// exactly while that generation still holds the session lease (ADR 0029).
    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        turn_id: &str,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<crate::TurnInputClaim>, StoreError>;

    /// Claim queued next-turn input at idle.
    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<crate::TurnInputClaim>, StoreError>;

    /// Abandon a held pending-turn-input claim so it can be reclaimed.
    async fn abandon_turn_input_claim(
        &self,
        claim: &crate::TurnInputClaim,
    ) -> Result<(), StoreError>;

    /// Release multiple held pending-turn-input claims in one backend batch.
    async fn abandon_turn_input_claims(
        &self,
        claims: &[crate::TurnInputClaim],
    ) -> Result<(), StoreError> {
        for claim in claims {
            self.abandon_turn_input_claim(claim).await?;
        }
        Ok(())
    }
}

/// Durable single-writer execution-lane capability, fenced by monotonic
/// fencing tokens.
#[async_trait::async_trait]
pub trait SessionExecutionLeaseStore: Send + Sync {
    /// Try to claim the durable single-writer execution lane for `session_id`.
    ///
    /// Returns [`SessionExecutionLeaseClaimOutcome::Busy`] when another owner
    /// holds an unexpired lease. Expired or released leases may be reclaimed
    /// and receive a higher fencing token. An unexpired lease held by the same
    /// owner id but a different incarnation is busy.
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError>;

    /// Reclaim an unexpired session execution lease whose observed holder is
    /// definitely dead according to persisted local-process liveness metadata.
    ///
    /// Backends must CAS on `observed_holder` so a stale claimant cannot clear
    /// a newer live lease that won the race after the busy observation.
    async fn reclaim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        observed_holder: &SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError>;

    /// Extend a live session execution lease owned by the caller.
    ///
    /// Backends must reject stale, released, superseded, or expired fences with
    /// [`StoreError::SessionExecutionLeaseExpired`].
    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError>;

    /// Release a session execution lease fenced by its completion token.
    ///
    /// This operation is idempotent and must not clear a newer owner's lease.
    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseCompletion,
    ) -> Result<(), StoreError>;
}

/// Durable queued-work capability: ingress, ordered claiming, and claim leases
/// for non-input work (process wakes and session commands).
///
/// Claims granted here are completed atomically by
/// [`SessionCommitStore::commit_runtime_state`].
#[async_trait::async_trait]
pub trait QueuedWorkStore: Send + Sync {
    /// Persist a queued-work batch for later claiming.
    async fn enqueue_queued_work(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkBatch, StoreError>;

    /// Claim a leading ready session-command batch for `owner_id`.
    ///
    /// A command claim is returned only when the earliest ready claimable batch
    /// is classified as [`crate::runtime::QueuedWorkClass::SessionCommand`].
    /// Backends derive the class from queued payloads; no schema column is
    /// required.
    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<crate::QueuedWorkClaim>, StoreError>;

    /// Claim the next ready turn-work group for `owner_id`.
    ///
    /// A turn-work claim is returned only when the earliest ready claimable
    /// batch is classified as [`crate::runtime::QueuedWorkClass::TurnWork`].
    /// Earlier ready session commands are not skipped and are never
    /// materialized as turn input.
    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        max_batches: usize,
    ) -> Result<Option<crate::QueuedWorkClaim>, StoreError>;

    /// Claim both ingress families admitted at an active-turn checkpoint.
    ///
    /// Backends must probe durable store state before opening a write
    /// transaction. When either family is pending, both claims are granted in
    /// one write transaction after validating the session-execution fence once.
    #[allow(clippy::too_many_arguments)]
    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        turn_id: &str,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
        max_batches: usize,
    ) -> Result<
        (
            Option<crate::TurnInputClaim>,
            Option<crate::QueuedWorkClaim>,
        ),
        StoreError,
    >;

    /// Claim a specific ready batch set selected from the durable queue.
    ///
    /// This is the host-facing counterpart to
    /// [`claim_ready_queued_work`](Self::claim_ready_queued_work): callers that
    /// project queued work into a UI can claim the exact batch ids they
    /// rendered instead of reconstructing authority from local draft state.
    ///
    /// This selection is intentionally allowed to bypass earlier unrelated
    /// ready work. The logical-turn driver uses it to reclaim an atomic outbox
    /// handoff immediately, preserving foreground frame-chain ordering.
    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        batch_ids: &[String],
    ) -> Result<Option<crate::QueuedWorkClaim>, StoreError>;

    /// Release a held queued-work claim without completing it.
    async fn abandon_queued_work_claim(
        &self,
        claim: &crate::QueuedWorkClaim,
    ) -> Result<(), StoreError>;

    /// Release multiple queued-work claims in one backend batch.
    async fn abandon_queued_work_claims(
        &self,
        claims: &[crate::QueuedWorkClaim],
    ) -> Result<(), StoreError> {
        for claim in claims {
            self.abandon_queued_work_claim(claim).await?;
        }
        Ok(())
    }

    /// Remove an unclaimed queued-work batch from durable ingress.
    ///
    /// Returns the removed batch when cancellation won the race. Returns `None`
    /// when the batch is missing or currently held by a live claim; callers must
    /// treat that as "already claimed or completed" and must not restore any
    /// stale local draft state.
    async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, StoreError>;

    /// List all queued-work batches for a session, including batches held by a
    /// live claim.
    async fn list_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;

    /// List queued-work batches that are still pending presentation/editing.
    ///
    /// This excludes batches currently held by a live claim. A claim counts as
    /// live only while the session-execution-lease generation it pins still
    /// holds the session lease; batches pinned to a superseded or released
    /// generation are pending again because they can be reclaimed or cancelled.
    ///
    /// This is a distinct required query, not a derivation of
    /// [`list_queued_work`](Self::list_queued_work): the two differ by
    /// claim-state filter, and backends answer each with its own query over
    /// claim rows rather than leaking claim state to callers for client-side
    /// filtering.
    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
}

/// Host-scheduled retention and garbage-collection capability over settled
/// state.
#[async_trait::async_trait]
pub trait StoreMaintenance: Send + Sync {
    /// Physically delete tombstoned graph-node rows and prune terminal
    /// pending-turn-input evidence rows. See [`VacuumReport`].
    async fn vacuum(&self) -> Result<VacuumReport, StoreError>;

    /// Delete blobs no longer reachable from any retained root.
    async fn gc_unreachable(&self) -> Result<GcReport, StoreError>;

    /// Re-derive every live node's incoming references from edge rows.
    ///
    /// Process roots deliberately remain outside this stored count because
    /// they live in another store family; `live_reference_summary` continues
    /// to aggregate those roots on demand (ADR 0024).
    async fn verify_node_refcounts(&self) -> Result<NodeRefcountVerification, StoreError>;
}

/// Exact settled-session persistence protocol required by the runtime.
///
/// `Arc<dyn RuntimePersistence>` is *the* runtime storage handle: one object
/// implementing every persistence capability segment —
/// [`SessionCommitStore`] (atomic graph/head commits, reads, metadata, and the
/// attachment write-ahead manifest), [`TurnInputStore`] (pending turn-input
/// lifecycle), [`QueuedWorkStore`] (queued-work ingress and claiming),
/// [`SessionExecutionLeaseStore`] (single-writer execution lane), and
/// [`StoreMaintenance`] (vacuum/GC). The segments share one transactional
/// domain: claims granted by the input and queue segments settle atomically in
/// [`SessionCommitStore::commit_runtime_state`]. In-flight nondeterministic
/// work belongs to the active [`EffectHost`](crate::EffectHost), not to the
/// store contract.
///
/// Blanket-implemented for every type that implements all five segments;
/// backends implement the segment traits and never this trait directly.
pub trait RuntimePersistence:
    SessionCommitStore
    + TurnInputStore
    + SessionExecutionLeaseStore
    + QueuedWorkStore
    + StoreMaintenance
{
}

impl<T> RuntimePersistence for T where
    T: SessionCommitStore
        + TurnInputStore
        + SessionExecutionLeaseStore
        + QueuedWorkStore
        + StoreMaintenance
        + ?Sized
{
}

#[cfg(test)]
mod tests;
