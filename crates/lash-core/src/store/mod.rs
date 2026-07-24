//! The runtime's settled-session persistence contract and shared store types.

mod attachment_manifest;
mod lease_timings;
pub mod queued_work;

pub use attachment_manifest::{
    AttachmentIntent, AttachmentManifest, AttachmentManifestEntry, AttachmentOwnerKind,
};
pub use lease_timings::{LeaseTimings, LeaseTimingsError};

const PROC_BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

fn default_root_session_id() -> String {
    "root".to_string()
}

pub const SESSION_HEAD_META_SCHEMA_VERSION: u32 = 1;
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
                agent_frames: Vec::new(),
                current_agent_frame_id: String::new(),
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
        assert!(
            state
                .agent_frames
                .iter()
                .all(|frame| frame.assignment.policy.recorded_provider_id() == "stored-provider")
        );
        assert_eq!(state.head_revision, Some(7));
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
        let err = decode_versioned_json_record::<SessionHeadMeta>(
            r#"{"schema_version":2}"#,
            "SessionHeadMeta",
            SESSION_HEAD_META_SCHEMA_VERSION,
        )
        .expect_err("unsupported session head schema version should fail");

        assert!(matches!(
            err,
            StoreError::UnsupportedRecordSchemaVersion {
                record_kind: "SessionHeadMeta",
                actual: 2,
                expected: SESSION_HEAD_META_SCHEMA_VERSION
            }
        ));
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "store is already bound to session `{bound_session_id}` and cannot be reused for `{attempted_session_id}`"
    )]
    SessionBindingMismatch {
        bound_session_id: String,
        attempted_session_id: String,
    },
    #[error("store does not support read scope {0:?}")]
    UnsupportedReadScope(SessionReadScope),
    #[error("store head revision conflict: expected {expected:?}, actual {actual}")]
    HeadRevisionConflict { expected: Option<u64>, actual: u64 },
    #[error(
        "runtime turn `{turn_id}` for session `{session_id}` was already committed with a different commit hash"
    )]
    RuntimeTurnCommitConflict { session_id: String, turn_id: String },
    #[error(
        "queued work claim `{claim_id}` for session `{session_id}` is superseded by a newer session-lease generation"
    )]
    QueuedWorkClaimSuperseded {
        session_id: String,
        claim_id: String,
    },
    #[error(
        "turn input claim `{claim_id}` for session `{session_id}` is superseded by a newer session-lease generation"
    )]
    TurnInputClaimSuperseded {
        session_id: String,
        claim_id: String,
    },
    #[error(
        "runtime commit for session `{session_id}` includes queued-work-derived content without settling claim `{claim_id}`"
    )]
    UnsettledQueuedWorkClaim {
        session_id: String,
        claim_id: String,
    },
    #[error(
        "runtime commit for session `{session_id}` includes turn-input-derived content without settling claim `{claim_id}`"
    )]
    UnsettledTurnInputClaim {
        session_id: String,
        claim_id: String,
    },
    #[error(
        "pending turn input source_key `{source_key}` for session `{session_id}` is already bound to input `{existing_input_id}` with different submitted content"
    )]
    PendingTurnInputSourceKeyConflict {
        session_id: String,
        source_key: String,
        existing_input_id: String,
    },
    #[error("session execution lease for session `{session_id}` is missing or expired")]
    SessionExecutionLeaseExpired { session_id: String },
    #[error(
        "{record_kind} schema_version {actual} is not supported by this binary (expected {expected})"
    )]
    UnsupportedRecordSchemaVersion {
        record_kind: &'static str,
        actual: u32,
        expected: u32,
    },
    #[error(
        "{record_kind} is missing schema_version and was written by unsupported pre-versioned state (expected {expected})"
    )]
    MissingRecordSchemaVersion {
        record_kind: &'static str,
        expected: u32,
    },
    #[error("{record_kind} schema_version {actual} is invalid (expected integer {expected})")]
    InvalidRecordSchemaVersion {
        record_kind: &'static str,
        actual: String,
        expected: u32,
    },
    #[error("store backend error: {0}")]
    Backend(String),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
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
    #[serde(default)]
    pub head_revision: u64,
    #[serde(default)]
    pub agent_frames: Vec<crate::AgentFrameRecord>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub current_agent_frame_id: crate::AgentFrameId,
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
    #[serde(default)]
    pub head_revision: u64,
    pub config: crate::PersistedSessionConfig,
    #[serde(default)]
    pub agent_frames: Vec<crate::AgentFrameRecord>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub current_agent_frame_id: crate::AgentFrameId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_ref: Option<BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf_node_id: Option<String>,
    #[serde(default)]
    pub graph_node_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_ledger: Vec<crate::TokenLedgerEntry>,
}

fn persisted_session_config_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::PersistedSessionConfig {
    crate::PersistedSessionConfig {
        provider_id: state.policy.recorded_provider_id().to_string(),
        model: state.policy.model.clone(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionReadScope {
    FullGraph,
    ActivePath { leaf_node_id: Option<String> },
}

#[derive(Clone, Debug)]
pub struct PersistedSessionRead {
    pub session_id: String,
    pub head_revision: u64,
    pub config: crate::PersistedSessionConfig,
    pub agent_frames: Vec<crate::AgentFrameRecord>,
    pub current_agent_frame_id: crate::AgentFrameId,
    pub graph: crate::SessionGraph,
    pub checkpoint_ref: Option<BlobRef>,
    pub checkpoint: Option<HydratedSessionCheckpoint>,
    pub token_ledger: Vec<crate::TokenLedgerEntry>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum GraphCommitDelta {
    Unchanged {
        leaf_node_id: Option<String>,
    },
    Append {
        nodes: Vec<crate::SessionNodeRecord>,
        leaf_node_id: Option<String>,
    },
    ReplaceFull(crate::SessionGraph),
}

impl GraphCommitDelta {
    pub fn leaf_node_id(&self) -> Option<&String> {
        match self {
            Self::Unchanged { leaf_node_id } | Self::Append { leaf_node_id, .. } => {
                leaf_node_id.as_ref()
            }
            Self::ReplaceFull(graph) => graph.leaf_node_id.as_ref(),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeCommit {
    pub session_id: String,
    pub expected_head_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_execution_lease: Option<SessionExecutionLeaseFence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_session_execution_lease: Option<SessionExecutionLeaseCompletion>,
    pub config: crate::PersistedSessionConfig,
    pub agent_frames: Vec<crate::AgentFrameRecord>,
    pub current_agent_frame_id: crate::AgentFrameId,
    pub graph: GraphCommitDelta,
    pub checkpoint: HydratedSessionCheckpoint,
    pub usage_deltas: Vec<crate::TokenLedgerEntry>,
    pub turn_commit: Option<RuntimeTurnCommitStamp>,
    pub completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
    pub completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_turn_input_turn_id: Option<String>,
    /// Attachment ids explicitly adopted by this commit. In the same
    /// transaction the backend also stamps every uncommitted manifest row owned
    /// by `turn_commit.turn_id`, including ids that appear only in plain tool
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
    pub turn_id: String,
    pub turn_commit_hash: String,
}

impl RuntimeTurnCommitStamp {
    pub fn new(
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        turn_commit_hash: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
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
        let mut semantic_commit = self.clone();
        semantic_commit.expected_head_revision = None;
        semantic_commit.session_execution_lease = None;
        semantic_commit.release_session_execution_lease = None;
        semantic_commit.turn_commit = None;
        let mut semantic_commit = serde_json::to_value(&semantic_commit).map_err(|err| {
            StoreError::Backend(format!("failed to serialize runtime turn commit: {err}"))
        })?;
        scrub_turn_commit_hash_value(&mut semantic_commit);
        crate::stable_hash::stable_json_sha256_hex(&semantic_commit).map_err(|err| {
            StoreError::Backend(format!(
                "failed to serialize runtime turn commit hash: {err}"
            ))
        })
    }

    pub fn persisted_state(
        state: &crate::RuntimeSessionState,
        usage_deltas: &[crate::TokenLedgerEntry],
    ) -> Self {
        Self {
            session_id: state.session_id.clone(),
            expected_head_revision: state.head_revision,
            session_execution_lease: None,
            release_session_execution_lease: None,
            config: persisted_session_config_from_state(state),
            agent_frames: state.agent_frames.clone(),
            current_agent_frame_id: state.current_agent_frame_id.clone(),
            graph: if state.graph_replace_required || state.head_revision.is_none() {
                GraphCommitDelta::ReplaceFull(state.session_graph.clone())
            } else {
                GraphCommitDelta::Unchanged {
                    leaf_node_id: state.session_graph.leaf_node_id.clone(),
                }
            },
            checkpoint: build_checkpoint_from_persisted_state(state),
            usage_deltas: usage_deltas.to_vec(),
            turn_commit: None,
            completed_queue_claims: Vec::new(),
            completed_turn_input_claims: Vec::new(),
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            committed_attachment_ids: Vec::new(),
        }
    }

    pub(crate) fn persisted_state_with_graph_commit(
        state: &crate::RuntimeSessionState,
        graph: GraphCommitDelta,
        usage_deltas: &[crate::TokenLedgerEntry],
    ) -> Self {
        Self {
            session_id: state.session_id.clone(),
            expected_head_revision: state.head_revision,
            session_execution_lease: None,
            release_session_execution_lease: None,
            config: persisted_session_config_from_state(state),
            agent_frames: state.agent_frames.clone(),
            current_agent_frame_id: state.current_agent_frame_id.clone(),
            graph,
            checkpoint: build_checkpoint_from_persisted_state(state),
            usage_deltas: usage_deltas.to_vec(),
            turn_commit: None,
            completed_queue_claims: Vec::new(),
            completed_turn_input_claims: Vec::new(),
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            committed_attachment_ids: Vec::new(),
        }
    }

    pub fn with_turn_commit(mut self, turn_commit: RuntimeTurnCommitStamp) -> Self {
        self.turn_commit = Some(turn_commit);
        self
    }

    pub fn with_session_execution_lease(mut self, lease: SessionExecutionLeaseFence) -> Self {
        self.session_execution_lease = Some(lease);
        self
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

fn scrub_turn_commit_hash_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let is_message = map.contains_key("role") && map.contains_key("parts");
            let is_message_part = map.contains_key("kind")
                && map.contains_key("content")
                && map.contains_key("prune_state");
            if is_message || is_message_part {
                map.remove("id");
            }
            for volatile_key in ["node_id", "parent_node_id", "leaf_node_id", "timestamp"] {
                map.remove(volatile_key);
            }
            for child in map.values_mut() {
                scrub_turn_commit_hash_value(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                scrub_turn_commit_hash_value(item);
            }
        }
        _ => {}
    }
}

fn persisted_session_state_from_head(
    head: SessionHead,
    checkpoint: Option<HydratedSessionCheckpoint>,
) -> crate::RuntimeSessionState {
    let mut state = crate::RuntimeSessionState {
        session_id: head.session_id,
        policy: crate::SessionPolicy::default(),
        agent_frames: head.agent_frames,
        current_agent_frame_id: head.current_agent_frame_id,
        session_graph: head.graph,
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
        head_revision: Some(head.head_revision),
        graph_replace_required: false,
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
            agent_frames: Vec::new(),
            current_agent_frame_id: String::new(),
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
            agent_frames: Vec::new(),
            current_agent_frame_id: String::new(),
            checkpoint_ref: None,
            leaf_node_id: None,
            graph_node_count: 0,
            token_ledger: Vec::new(),
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
    /// Durability tier this session store provides; defaults to
    /// [`DurabilityTier::Inline`](crate::DurabilityTier::Inline).
    fn durability_tier(&self) -> crate::DurabilityTier {
        crate::DurabilityTier::Inline
    }

    async fn load_session(
        &self,
        scope: SessionReadScope,
    ) -> Result<Option<PersistedSessionRead>, StoreError>;

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, StoreError>;

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitResult, StoreError>;

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
    /// Mark graph nodes as tombstoned so reads exclude them until
    /// [`vacuum`](Self::vacuum) physically removes them.
    async fn tombstone_nodes(&self, ids: &[String]) -> Result<(), StoreError>;

    /// Physically delete tombstoned graph-node rows and prune terminal
    /// pending-turn-input evidence rows. See [`VacuumReport`].
    async fn vacuum(&self) -> Result<VacuumReport, StoreError>;

    /// Delete blobs no longer reachable from any retained root.
    async fn gc_unreachable(&self) -> Result<GcReport, StoreError>;
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

fn persisted_session_state_from_read(read: PersistedSessionRead) -> crate::RuntimeSessionState {
    persisted_session_state_from_head(
        SessionHead {
            session_id: read.session_id,
            head_revision: read.head_revision,
            agent_frames: read.agent_frames,
            current_agent_frame_id: read.current_agent_frame_id,
            graph: read.graph,
            config: read.config,
            checkpoint_ref: read.checkpoint_ref,
            token_ledger: read.token_ledger,
        },
        read.checkpoint,
    )
}

pub async fn load_persisted_session_state(
    store: &(dyn RuntimePersistence + '_),
) -> Result<Option<crate::RuntimeSessionState>, StoreError> {
    Ok(store
        .load_session(SessionReadScope::FullGraph)
        .await?
        .map(persisted_session_state_from_read))
}

pub async fn load_persisted_session_state_active_path(
    store: &(dyn RuntimePersistence + '_),
    leaf_node_id: Option<String>,
) -> Result<Option<crate::RuntimeSessionState>, StoreError> {
    Ok(store
        .load_session(SessionReadScope::ActivePath { leaf_node_id })
        .await?
        .map(persisted_session_state_from_read))
}

pub async fn refresh_persisted_session_state(
    store: &(dyn RuntimePersistence + '_),
    state: &mut crate::RuntimeSessionState,
) -> Result<(), StoreError> {
    if let Some(mut fresh) = load_persisted_session_state(store).await? {
        fresh.policy.session_id = state.policy.session_id.clone();
        fresh.policy.max_turns = state.policy.max_turns;
        *state = fresh;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{LeaseOwnerIdentity, LeaseOwnerLiveness};

    fn local_liveness(
        host_id: &str,
        boot_id: &str,
        pid: u32,
        process_start: &str,
    ) -> LeaseOwnerLiveness {
        LeaseOwnerLiveness::local_process_for_test(host_id, boot_id, pid, process_start)
    }

    #[test]
    fn lease_owner_identity_requires_same_incarnation() {
        let first = LeaseOwnerIdentity::opaque("owner", "incarnation-a");
        let same = LeaseOwnerIdentity::opaque("owner", "incarnation-a");
        let next = LeaseOwnerIdentity::opaque("owner", "incarnation-b");

        assert!(first.same_incarnation(&same));
        assert!(!first.same_incarnation(&next));
    }

    #[test]
    fn local_liveness_only_proves_same_host_boot_dead_processes() {
        let holder = local_liveness(
            "host-a",
            "boot-a",
            std::process::id(),
            "not-the-current-process-start",
        );
        let same_host_boot = local_liveness("host-a", "boot-a", std::process::id(), "claimant");
        let other_host = local_liveness("host-b", "boot-a", std::process::id(), "claimant");
        let other_boot = local_liveness("host-a", "boot-b", std::process::id(), "claimant");

        assert!(holder.is_definitely_dead_for_claimant(&same_host_boot));
        assert!(!holder.is_definitely_dead_for_claimant(&other_host));
        assert!(!holder.is_definitely_dead_for_claimant(&other_boot));
        assert!(!holder.is_definitely_dead_for_claimant(&LeaseOwnerLiveness::Opaque));
        assert!(!LeaseOwnerLiveness::Opaque.is_definitely_dead_for_claimant(&same_host_boot));
    }
}
