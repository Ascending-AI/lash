//! The runtime's settled-session persistence contract and shared store types.
use crate::SessionId;
use crate::TurnId;
use crate::facade_support::SessionGraphFacadeOps;
pub mod attachment_manifest;
mod checkpoint;
pub mod namespace;
pub use checkpoint::{
    CHECKPOINT_COMPONENT_ENCODING_VERSION, CheckpointComponentDescriptor,
    EXECUTION_STATE_CHECKPOINT_COMPONENT, HydratedCheckpointComponent, HydratedSessionCheckpoint,
    PLUGIN_STATE_CHECKPOINT_COMPONENT, SESSION_CHECKPOINT_SCHEMA_VERSION, SessionCheckpoint,
    TOOL_STATE_CHECKPOINT_COMPONENT, ensure_checkpoint_component_encoding_version,
    ensure_checkpoint_component_hash_agreement,
};
pub mod claim_plan;
mod claim_settlement;
pub mod commit_budget;
mod commit_identity;
mod config_command_plan;
mod error;
pub mod fencing;
#[cfg(test)]
mod fencing_tests;
mod fleet_format;
mod fork_plan;
mod graph_commit;
mod lease_timings;
mod load;
mod maintenance;
mod park;
pub mod pending_follow_on;
mod preflight;
mod queued_run;
pub mod queued_work;
pub use queued_run::{
    BeginQueuedRun, QueuedRunAdmission, QueuedRunCommit, QueuedRunMember, QueuedRunOrigin,
    QueuedRunPosition, QueuedRunProgress, QueuedRunRequest, QueuedRunTerminal, SelectedQueuedRun,
};
mod control_intent;
mod drive_fence;
mod realization;
mod retention;
mod root;
pub mod runtime_commit;
mod runtime_commit_plan;
mod semantic_boundary;
pub mod session_execution_lease;
mod session_ingress;
pub mod session_ingress_plan;
mod state_version;
#[cfg(any(test, feature = "testing"))]
mod testing;
mod usage;
pub mod work_claim;

pub use crate::session_graph::RealizedNodeTimestamp;
pub use crate::session_ingress_vocabulary::{
    ClaimMode, ConfigRefusalCode, Delivery, IngressAffectedItem, IngressCancelReason, IngressClaim,
    IngressClaimIdentity, IngressEnqueueOutcome, IngressItem, IngressItemDraft, IngressItemId,
    IngressItemRead, IngressKind, IngressLane, IngressPayload, IngressReadStatus,
    IngressReclaimOutcome, IngressState, IngressSuffixWithdrawOutcome, IngressTerminalCause,
    IngressUndeliveredDisposition, IngressWithdrawOutcome, IngressWithdrawReceipt,
    IngressWithdrawSelector, IngressWithdrawTarget, SESSION_INGRESS_SUBMISSION_FAMILY_VERSION,
};
pub use attachment_manifest::{
    AttachmentCondemnation, AttachmentCondemnationPhase, AttachmentCondemnationProvenance,
    AttachmentCondemnationRecord, AttachmentDeleteArming, AttachmentIntent, AttachmentManifest,
    AttachmentManifestEntry, AttachmentOwner, AttachmentOwnerKind, AttachmentWriteFence,
    AttachmentWritePermit, AttachmentWriteToken, decode_attachment_condemnation_record,
    decode_attachment_owner,
};
pub use claim_plan::{
    ClaimPlanDecision, QueuedWorkClaimPlan, QueuedWorkClaimRow, QueuedWorkClaimWrite,
    QueuedWorkSettlementPlan, QueuedWorkSettlementRow, QueuedWorkSettlementRowClaim,
    QueuedWorkSettlementWrite, SettlementDecision, TerminalProcessWake, TurnInputClaimPlan,
    TurnInputClaimRow, TurnInputClaimWrite, TurnInputSettlementPlan, TurnInputSettlementRegime,
    TurnInputSettlementRow, TurnInputSettlementRowFacts, TurnInputSettlementStep,
    classify_empty_claim_scan, plan_queued_work_claim, plan_queued_work_settlement,
    plan_turn_input_claim, plan_turn_input_settlement,
};
pub use commit_budget::{CommitBudget, CommitBudgetLimit};
pub use commit_identity::{
    APPEND_REQUEST_IDENTITY_ENCODING_VERSION, OperationId, RuntimeCommitReceiptDecision,
    decide_runtime_commit_receipt, derive_history_node_id,
};
pub use config_command_plan::{ConfigCommandPlan, plan_config_commands};
pub use control_intent::{
    CONTROL_INTENT_FORMAT, ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState,
    ControlIntentStore, IntentApplication, RootIntentFacts, RootIntentPlan, RootIntentRefused,
    RootIntentRequest, RootVerb, decide_intent_acknowledgement, decide_intent_application,
    decide_intent_failure, decide_root_intent, forked_root, stored_intent_kind,
    stored_intent_state,
};
pub use drive_fence::{
    AdmissionId, DriveEpochSeal, DriveEpochSealDecision, DriveEpochStore, DriveFence,
    InMemoryDriveEpochs, RootStartNonce, SessionHeadRef, StoredDriveEpoch, close_admission,
    decide_drive_epoch_seal, require_current_drive_fence,
};
pub use error::{SessionExecutionLeaseRenewalInstallMismatch, StoreError};
pub use fencing::{
    BoundTurnInputCancel, EFFECT_REPLAY_IN_PROGRESS_STATUS, EffectReplayLeaseAuthority,
    EffectReplayLeaseFacts, EffectReplayLeaseVerdict, FENCED_WRITE_DISAGREEMENT_EVENT,
    FENCING_TRACE_TARGET, FenceTimeAuthority, FencedWrite, HeadPublicationVerdict,
    ProcessLeaseAuthority, ProcessLeaseFacts, ProcessLeaseVerdict, QueuedWorkSettlementFacts,
    TurnInputSettlementFacts, WakeDeliveryClaimFacts, WakeDeliveryClaimVerdict, WorkRowClaimFacts,
    WorkRowClaimability, bound_turn_input_cancel, effect_replay_lease_verdict,
    fenced_write_applied, head_publication_verdict, process_lease_verdict,
    queued_work_batch_claimability, require_fenced_write_applied,
    require_releasable_session_execution_lease, require_renewable_session_execution_lease,
    require_settleable_queued_work, require_settleable_turn_input,
    require_single_writer_head_publication, turn_input_claimability,
    unclaimed_turn_input_is_settleable, wake_delivery_claim_verdict,
};
pub use fleet_format::{
    FLEET_FORMAT_VERSION, FleetFormat, FleetFormatState, RECORD_UPCASTERS, ReadWindow,
    RecordUpcaster, SurfaceFormat, WriterPin, decode_versioned_json_record,
    decode_versioned_json_record_for_fleet, decode_versioned_msgpack_record_for_fleet,
    ensure_supported_record_schema_version_for_fleet, ensure_supported_schema_version_for_fleet,
    upcast_chain_covers, upcast_json_record,
};
pub use fork_plan::{ForkLineageAncestor, ForkNodeFacts, ForkPlan};
pub use lease_timings::{LeaseTimings, LeaseTimingsError};
pub use load::{
    LoadedPersistedSession, load_persisted_session, load_persisted_session_admitted,
    load_persisted_session_read_view, load_persisted_session_state,
    refresh_persisted_session_state,
};
pub use maintenance::{
    GcReport, MaintenanceFailure, MaintenanceRefusal, MaintenanceReport, MaintenanceResult,
    MaintenanceStop, MaintenanceSweep, SessionBlobReclaimReport, VacuumReport,
};
pub use park::{
    EnginePark, ParkCancelCause, ParkEventKind, ParkFeedCursor, ParkFeedEvent, ParkFeedPage,
    ParkId, ParkReason, ParkReasonCode, ParkSummary, ProcessPark, ProcessParkKey, ProcessParkQuery,
    ProcessParkWrite, StoredTurnParkHead, TurnPark, TurnParkQuery, TurnParkTarget, TurnParkWrite,
    TurnParkWriteDecision, UnparkCause, UnsettledTurnCounts, decide_turn_park_write,
};
pub use pending_follow_on::{
    DEFAULT_MAX_FOLLOW_ON_RECOVERIES, FollowOnBlocked, FollowOnClaim, FollowOnRecovery,
    PendingFollowOn, follow_on_blocks_claim, validate_follow_on_head_write,
};
pub use preflight::{
    DurableItem, DurablePayload, DurableScan, DurableScanPage, DurableSurface, ScanCoverage,
    StoreBackend, StoreComponentVersion, StorePreflight, StoreReleaseStamp, StoreReleaseState,
    StoreSchemaDatabase, StoreSchemaOutcome, StoreSchemaStatus, StoreSchemaVerdict,
    compare_releases, release_stamp_advances,
};
pub use queued_work::{
    PendingSessionWorkOrdering, PendingWorkOrderingKey, QueuedWorkClaimOutcome,
    QueuedWorkClaimRefusal, QueuedWorkClass, SelectedQueuedWorkClaimOutcome, TurnWorkClaimPrefix,
    TurnWorkClaimSelection,
};
pub use realization::commit_runtime_state_verified;
pub use retention::{
    FACADE_PLUGIN_COMMAND_OPERATION_TAG, FACADE_PLUGIN_TASK_OPERATION_TAG, FacadePluginOperation,
    PLUGIN_OPERATION_STATE_RECEIPT_KEY, RetentionBound, RetentionReport,
    is_facade_minted_operation_id, mint_facade_operation_id, plugin_operation_receipt_storage_key,
};
pub use root::{
    InMemoryRootLedger, RootStore, RootTerminal, RootTerminalCause, RootTerminalKind,
    RootTerminalWrite, RootTerminalWriteDecision, StoredRootTerminal, TurnCommitId,
    decide_root_terminal_write, root_binding_conflict, settled_queued_root_cause,
    settled_queued_root_terminal,
};
pub use runtime_commit::{
    AppendRequestIdentity, RUNTIME_COMMIT_RECEIPT_RECORD_KIND,
    RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION, RuntimeCommit, RuntimeCommitReceipt,
    RuntimeTurnCommitStamp, RuntimeUsageDelta, RuntimeUsageDeltaIdentity,
    SemanticBoundaryOperation, decode_runtime_commit_receipt,
    decode_runtime_commit_receipt_for_fleet, ensure_supported_receipt_version,
    ensure_supported_receipt_version_for_fleet,
};
pub use runtime_commit_plan::{
    FreshRuntimeCommitFacts, ParentNodeFacts, PlannedNodeFacts, PublishedLeafFacts,
    RuntimeCommitPlan, RuntimeCommitPlanner, RuntimeCommitReceiptRecord, RuntimeCommitReceiptWrite,
    RuntimeCommitReplay,
};
pub use semantic_boundary::{
    CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION,
    RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION,
    USAGE_LEDGER_REQUEST_IDENTITY_ENCODING_VERSION,
};
pub use session_execution_lease::{
    LeaseClaimNonce, LeaseOwnerIdentity, SessionExecutionLease, SessionExecutionLeaseAcquisition,
    SessionExecutionLeaseAuthority, SessionExecutionLeaseClaimOutcome,
    SessionExecutionLeaseDisplacement, SessionExecutionLeaseObservation,
};
pub use session_ingress::{
    IngressClaimPolicy, IngressClaimRef, IngressClaimSettlement, IngressCommandOutcome,
    IngressCommandResult, IngressRefusedWindow, IngressSettlementIntent, IngressSettlementReceipt,
    IngressTurnCancel, SessionIngressStore,
};
pub use state_version::{
    CURRENT_SESSION_STATE_VERSION, OLDEST_SUPPORTED_SESSION_STATE_VERSION, SessionStateAdmission,
    resolve_session_state_version,
};
#[cfg(any(test, feature = "testing"))]
pub use testing::{
    ConformancePersistence, StoreTestSupport, append_request_commit_with_clock_for_testing,
};
pub use usage::{merge_token_ledger_entries_checked, merge_token_ledger_entry_checked};
pub use work_claim::{WorkClaim, WorkCompletion};

fn default_root_session_id() -> SessionId {
    SessionId::from("root")
}
/// Version 9 combines full effect addresses and truthful attribution with
/// explicit ambient-or-restricted resident-tool authority. Both version 8
/// parent encodings are refused rather than inventing either identity or access.
/// Version 10 persists the host-selected reasoning-retention capability and
/// selection in the model snapshot. Version 9 heads are refused instead of
/// silently inventing a retention contract during cold reopen.
/// Version 11 nests restricted resident-tool definitions under `manifest` and
/// `contract` fields (FIG-1210); a version 10 head carrying the flattened
/// encoding is refused rather than reinterpreted field-by-field.
pub const SESSION_HEAD_META_SCHEMA_VERSION: u32 = 11;

#[cfg(test)]
mod prompt_persistence_compat_tests;

#[cfg(test)]
mod persisted_state_tests;

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SessionMeta {
    pub session_id: SessionId,
    pub relation: crate::SessionRelation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_observer_intents: Vec<crate::SessionObserverIntent>,
}

impl SessionMeta {
    /// Returns the parent session id, if any, derived from the canonical
    /// [`SessionRelation`](crate::SessionRelation) field.
    pub fn parent_session_id(&self) -> Option<&str> {
        self.relation.parent_session_id()
    }
}

/// Complete durable identity metadata supplied at session admission.
///
/// Session ids are opaque, non-empty UTF-8 strings. Lash deliberately imposes
/// no additional length or character-set policy; hosts that expose ids in URLs,
/// filenames, or other constrained namespaces own those boundary rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBinding {
    pub session_id: SessionId,
    pub relation: crate::SessionRelation,
}

impl SessionBinding {
    pub fn root(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            relation: crate::SessionRelation::Root,
        }
    }

    /// Projects the durable binding fields store implementors need from a create request.
    pub fn from_create_request(request: &crate::SessionStoreCreateRequest) -> Self {
        Self {
            session_id: request.session_id.clone(),
            relation: request.relation.clone(),
        }
    }

    /// Rejects an empty or NUL-containing session ID before store implementors admit the binding.
    pub fn validate(&self) -> Result<(), StoreError> {
        validate_session_id(&self.session_id)
    }
}

/// Outcome of admitting a session binding to a persistence handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAdmission {
    /// The admission durably created the session metadata row.
    Created,
    /// The handle was already durably bound to the same live session.
    Rebound,
}

pub fn validate_session_id(session_id: &SessionId) -> Result<(), StoreError> {
    if !namespace::is_valid_opaque_key(session_id) {
        Err(StoreError::InvalidSessionId {
            reason: "session ids must not be empty or contain NUL",
        })
    } else {
        Ok(())
    }
}

#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct BlobRef(pub String);

impl BlobRef {
    pub fn for_content(content: &[u8]) -> Self {
        Self(crate::stable_hash::blake3_hex("lash-blob/v2", content))
    }

    /// Exposes the opaque durable blob reference to store implementors for backend round-tripping
    /// without imposing path or URL semantics.
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionHead {
    #[serde(default = "default_root_session_id")]
    pub session_id: SessionId,
    #[serde(skip)]
    pub head_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_node_id: Option<crate::FrameNodeId>,
    /// The follow-on the head owes (ADR 0101 §3); its own head column.
    #[serde(skip)]
    pub pending_follow_on: Option<PendingFollowOn>,
    pub graph: crate::SessionGraph,
    pub config: crate::PersistedSessionConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_ref: Option<BlobRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_ledger: Vec<crate::TokenLedgerEntry>,
}

/// JSON-owned fields persisted in a session head's `head_json` column.
///
/// Revision and graph/checkpoint references live in dedicated columns and are
/// deliberately absent from this serializable payload.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionHeadPayload {
    pub schema_version: u32,
    #[serde(default = "default_root_session_id")]
    pub session_id: SessionId,
    pub config: crate::PersistedSessionConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_node_id: Option<crate::FrameNodeId>,
}

/// Fully assembled session-head metadata returned by a store.
///
/// This type is intentionally not serializable. Store implementations decode a
/// [`SessionHeadPayload`] and must supply the three column-owned values through
/// [`Self::assemble`].
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SessionHeadMeta {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub head_revision: u64,
    pub config: crate::PersistedSessionConfig,
    pub current_frame_node_id: Option<crate::FrameNodeId>,
    pub checkpoint_ref: Option<BlobRef>,
    pub leaf_node_id: Option<crate::NodeId>,
    /// The follow-on the head owes, from the `pending_follow_on_json` column
    /// (ADR 0101 §3). It is not part of [`SessionHeadPayload`].
    pub pending_follow_on: Option<PendingFollowOn>,
}

impl SessionHeadMeta {
    /// Attach the `pending_follow_on_json` column a store read beside the
    /// payload.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    pub fn with_pending_follow_on(mut self, pending_follow_on: Option<PendingFollowOn>) -> Self {
        self.pending_follow_on = pending_follow_on;
        self
    }

    /// The session's identity is owned by the row key the caller bound the
    /// query to, never by the payload: `session_id` is taken from
    /// `session_id` and the payload's copy is a checked redundancy. A payload
    /// naming a different session is refused as corrupt stored data rather
    /// than adopted, so a mis-keyed or hand-edited `head_json` can no longer
    /// rewrite a session's live identity.
    ///
    /// This remains public because external stores assemble rows from their
    /// own columns. Callers still enforce node derivation before assembly; the
    /// constructor cannot validate that fact from this projection alone.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    pub fn assemble(
        session_id: &SessionId,
        payload: SessionHeadPayload,
        head_revision: u64,
        checkpoint_ref: Option<BlobRef>,
        leaf_node_id: Option<crate::NodeId>,
    ) -> Result<Self, StoreError> {
        if payload.session_id != *session_id {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                message: format!(
                    "head_json names session `{}` but the row is keyed on session `{}`",
                    payload.session_id.as_str(),
                    session_id.as_str()
                ),
            });
        }
        Ok(Self {
            schema_version: payload.schema_version,
            session_id: session_id.clone(),
            head_revision,
            config: payload.config,
            current_frame_node_id: payload.current_frame_node_id,
            checkpoint_ref,
            leaf_node_id,
            pending_follow_on: None,
        })
    }

    /// Project the exact value that may be serialized into `head_json`.
    pub fn payload(&self) -> SessionHeadPayload {
        SessionHeadPayload {
            schema_version: self.schema_version,
            session_id: self.session_id.clone(),
            config: self.config.clone(),
            current_frame_node_id: self.current_frame_node_id.clone(),
        }
    }
}

pub fn persisted_session_config_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::PersistedSessionConfig {
    if let Some(config) = &state.authority.committed_config {
        return (**config).clone();
    }
    execution_session_config_from_state(state)
}

/// The config used by the running root, including its recorded execution view.
pub fn execution_session_config_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::PersistedSessionConfig {
    let mut config = crate::PersistedSessionConfig::from(&state.policy);
    config.tool_access = state.authority.tool_access.clone();
    config.subagent = state.authority.subagent.clone();
    config.protocol_turn_options = Some(state.protocol_turn_options.clone());
    config.config_revision = state.config_revision;
    config
}

#[derive(Clone, Debug)]
pub struct PersistedSessionRead {
    pub session_id: SessionId,
    pub head_revision: u64,
    pub config: crate::PersistedSessionConfig,
    pub current_frame_node_id: Option<crate::FrameNodeId>,
    /// The follow-on the head owes (ADR 0101 §3).
    pub pending_follow_on: Option<PendingFollowOn>,
    pub graph: crate::SessionGraph,
    pub checkpoint_ref: Option<BlobRef>,
    pub checkpoint: Option<HydratedSessionCheckpoint>,
    pub token_ledger: Vec<crate::TokenLedgerEntry>,
    /// Failure components loaded from this session's durable turn receipts.
    pub turn_failure_settlements: Vec<crate::TurnFailureSettlement>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum GraphAppend {
    Extend {
        nodes: Vec<crate::SessionNodeRecord>,
    },
    PreserveHead,
}

impl GraphAppend {
    pub fn nodes(&self) -> &[crate::SessionNodeRecord] {
        match self {
            Self::Extend { nodes } => nodes,
            Self::PreserveHead => &[],
        }
    }

    pub fn nodes_mut(&mut self) -> &mut [crate::SessionNodeRecord] {
        match self {
            Self::Extend { nodes } => nodes,
            Self::PreserveHead => &mut [],
        }
    }

    pub fn leaf_node_id(&self) -> Option<&crate::NodeId> {
        self.nodes().last().map(|node| &node.node_id)
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
    let actual = record_schema_version(record_kind, value, expected)?;
    ensure_supported_schema_version(record_kind, actual, expected)
}

/// The `schema_version` a persisted record carries — the read half of
/// [`ensure_supported_record_schema_version`], split out so the fleet's read
/// window can admit the extracted version instead of a bare `expected`.
fn record_schema_version(
    record_kind: &'static str,
    value: &serde_json::Value,
    expected: u32,
) -> Result<u32, StoreError> {
    let Some(schema_version) = value.get("schema_version") else {
        // A persisted record that did not decode to an object at all is
        // corruption, not a version refusal: there is no record here whose
        // version could be missing. Backends whose blobs carry no framing of
        // their own (PostgreSQL stores the checkpoint manifest as bare
        // MessagePack) otherwise report arbitrary corrupt bytes that happen to
        // form a valid scalar -- a lone `0x00` decodes as the integer `0` --
        // as `MissingRecordSchemaVersion`, while a framed backend reports
        // `StoredDataCorrupt` for the same bytes (FIG-2841).
        if !value.is_object() {
            return Err(StoreError::StoredDataCorrupt {
                record_kind,
                message: format!("persisted {record_kind} record did not decode to an object"),
            });
        }
        return Err(StoreError::MissingRecordSchemaVersion {
            record_kind,
            expected,
        });
    };
    schema_version
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| StoreError::InvalidRecordSchemaVersion {
            record_kind,
            actual: schema_version.to_string(),
            expected,
        })
}

fn build_persisted_turn_state(state: &crate::RuntimeSessionState) -> crate::PersistedTurnState {
    crate::PersistedTurnState {
        turn_index: state.turn_index,
        token_usage: state.token_usage.clone(),
        last_prompt_usage: state.last_prompt_usage.clone(),
        protocol_turn_options: state.protocol_turn_options.clone(),
    }
}

pub(crate) fn encode_checkpoint_component<T: serde::Serialize>(
    key: &str,
    value: &T,
) -> Result<Vec<u8>, StoreError> {
    rmp_serde::to_vec_named(value).map_err(|error| StoreError::RecordEncodingFailed {
        record_kind: format!("checkpoint component `{key}`"),
        message: error.to_string(),
    })
}

fn build_checkpoint_from_persisted_state(
    state: &crate::RuntimeSessionState,
    fleet_format: FleetFormat,
) -> Result<HydratedSessionCheckpoint, StoreError> {
    state
        .checkpoint_components
        .build_checkpoint(build_persisted_turn_state(state), fleet_format)
}

impl RuntimeCommit {
    /// Rejects an invalid or empty operation identity and any session-scoped operation whose
    /// session differs from the commit before store implementors write it.
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
        if completed
            .operation
            .scope
            .session_id()
            .is_some_and(|session_id| session_id != self.session_id)
        {
            return Err(StoreError::RuntimeTurnCommitConflict {
                session_id: self.session_id.clone(),
                operation_key: completed.operation.storage_key()?,
            });
        }
        commit_identity::validate_receipt_identity(self)?;
        self.validate_usage_delta_identities()?;
        Ok(())
    }

    fn validate_usage_delta_identities(&self) -> Result<(), StoreError> {
        let mut seen = std::collections::HashSet::with_capacity(self.usage_deltas.len());
        for delta in &self.usage_deltas {
            if delta.identity.operation_storage_key.trim().is_empty() {
                return Err(StoreError::Backend(
                    "runtime usage delta identity requires a non-empty operation storage key"
                        .to_string(),
                ));
            }
            let expected = RuntimeUsageDeltaIdentity::for_entry(
                delta.identity.operation_storage_key.clone(),
                delta.identity.entry_ordinal,
                &delta.entry,
            );
            if delta.identity.payload_encoding_version != expected.payload_encoding_version
                || delta.identity.payload_hash != expected.payload_hash
            {
                return Err(StoreError::Backend(format!(
                    "runtime usage delta identity payload encoding version or hash does not match canonical entry content ({}, {})",
                    delta.identity.operation_storage_key, delta.identity.entry_ordinal
                )));
            }
            if !seen.insert(&delta.identity) {
                return Err(StoreError::Backend(format!(
                    "runtime commit repeats usage delta identity ({}, {}, {}, {})",
                    delta.identity.operation_storage_key,
                    delta.identity.entry_ordinal,
                    delta.identity.payload_encoding_version,
                    delta.identity.payload_hash
                )));
            }
        }
        Ok(())
    }

    /// Exhaustive append-envelope allowlist. Adding a new commit member forces
    /// this destructure to be reconsidered, while the checks keep append
    /// commits from silently acquiring another unrelated settlement side
    /// effect. Usage is deliberately allowed because it has its own durable
    /// exactly-once identity.
    pub fn debug_assert_append_envelope_scope(&self) {
        let RuntimeCommit {
            commit_budget: _,
            session_id: _,
            expected_head_revision: _,
            session_execution_lease_fence: _,
            drive_fence: _,
            root_terminal,
            release_session_execution_lease: _,
            config: _,
            current_frame_node_id: _,
            graph: _,
            graph_base_leaf_node_id: _,
            checkpoint: _,
            usage_deltas: _,
            failure_evidence,
            turn_commit: _,
            completed_queue_claims,
            completed_turn_input_claims,
            undelivered_turn_input_claims,
            // Carried unchanged from the head; the store refuses a change.
            pending_follow_on: _,
            interrupted_turn_input_turn_id,
            interrupted_turn_input_cancellation,
            interrupted_turn_cancel_intent,
            turn_cancel_closure_settlement,
            adopted_intent_rows,
            committed_attachment_ids,
            queued_run: _,
        } = self;
        debug_assert!(
            completed_queue_claims.is_empty()
                && completed_turn_input_claims.is_empty()
                && undelivered_turn_input_claims.is_empty()
                && interrupted_turn_input_turn_id.is_none()
                && interrupted_turn_input_cancellation.is_none()
                && interrupted_turn_cancel_intent.is_none()
                && turn_cancel_closure_settlement.is_none()
                && *adopted_intent_rows == 0
                && failure_evidence.is_empty()
                && committed_attachment_ids.is_empty()
                && root_terminal.is_none(),
            "append-session-nodes constructor gained unrelated settlement side effects"
        );
    }

    /// Re-derives every appended node ID by ordinal for store implementors, using frame keys for
    /// frame-open nodes and operation identity for all other nodes.
    pub fn validate_node_derivation(&self) -> Result<(), StoreError> {
        let completed = &self.turn_commit;
        for (ordinal, node) in self.graph.nodes().iter().enumerate() {
            let expected = match &node.payload {
                crate::SessionNodePayload::FrameOpen { frame_key, .. } => crate::NodeId::new(
                    crate::session_graph::frame_node_id(&self.session_id, frame_key.as_str())
                        .into_inner(),
                ),
                _ => {
                    derive_history_node_id(&self.session_id, &completed.operation, ordinal as u64)?
                }
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

    /// Rejects duplicate node IDs within one append batch before store implementors mutate durable
    /// graph state.
    pub fn validate_append_node_ids_unique(&self) -> Result<(), StoreError> {
        let mut seen = std::collections::HashSet::with_capacity(self.graph.nodes().len());
        for node in self.graph.nodes() {
            if !seen.insert(node.node_id.as_str()) {
                return Err(StoreError::NodeIdCollision {
                    node_id: node.node_id.clone(),
                });
            }
        }
        Ok(())
    }

    /// Flattens application evidence from completed turn-input claims in completion and per-claim
    /// order for store implementors returning commit results.
    pub fn turn_input_applications(&self) -> Vec<crate::TurnInputApplication> {
        self.completed_turn_input_claims
            .iter()
            .flat_map(|completion| completion.applications.iter().cloned())
            .collect()
    }

    pub fn validate_claim_settlement(
        &self,
        originating_queue_claims: &[crate::QueuedWorkCompletion],
        originating_turn_input_claims: &[crate::TurnInputCompletion],
    ) -> Result<(), StoreError> {
        claim_settlement::validate_claim_settlement(
            self,
            originating_queue_claims,
            originating_turn_input_claims,
        )
    }

    /// Computes the canonical semantic commit hash store implementors use to distinguish idempotent
    /// replay from a conflicting operation reuse.
    pub fn turn_commit_hash(&self) -> Result<String, StoreError> {
        commit_identity::turn_commit_hash(self)
    }

    pub fn persisted_state_with_operation_and_budget(
        state: &mut crate::RuntimeSessionState,
        usage_deltas: &[crate::TokenLedgerEntry],
        operation: OperationId,
        commit_budget: CommitBudget,
        fleet_format: FleetFormat,
    ) -> Result<(Self, Vec<crate::NodeId>), StoreError> {
        let mut graph = state.pending_graph_commit();
        let mapping = graph.derive_node_ids(&state.session_id, &operation)?;
        state
            .session_graph
            .remap_node_ids(&state.session_id, &mapping);
        remap_optional_node_id(&mut state.current_frame_node_id, &mapping);
        state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
        let persisted_node_ids = mapping.iter().map(|(_, derived)| derived.clone()).collect();
        let commit = Self::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            usage_deltas,
            operation,
            commit_budget,
            fleet_format,
        )?;
        Ok((commit, persisted_node_ids))
    }

    pub fn persisted_state_with_operation_and_staged_usage_and_budget(
        state: &mut crate::RuntimeSessionState,
        usage_deltas: &[RuntimeUsageDelta],
        operation: OperationId,
        commit_budget: CommitBudget,
        fleet_format: FleetFormat,
    ) -> Result<(Self, Vec<crate::NodeId>), StoreError> {
        let mut graph = state.pending_graph_commit();
        let mapping = graph.derive_node_ids(&state.session_id, &operation)?;
        state
            .session_graph
            .remap_node_ids(&state.session_id, &mapping);
        remap_optional_node_id(&mut state.current_frame_node_id, &mapping);
        state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
        let persisted_node_ids = mapping.iter().map(|(_, derived)| derived.clone()).collect();
        let commit = Self::persisted_state_with_graph_commit_and_staged_usage_and_budget(
            state,
            graph,
            usage_deltas,
            operation,
            commit_budget,
            fleet_format,
        )?;
        Ok((commit, persisted_node_ids))
    }

    pub fn persisted_state_with_graph_commit_and_operation_and_budget(
        state: &crate::RuntimeSessionState,
        graph: GraphAppend,
        usage_deltas: &[crate::TokenLedgerEntry],
        operation: OperationId,
        commit_budget: CommitBudget,
        fleet_format: FleetFormat,
    ) -> Result<Self, StoreError> {
        let usage_deltas = RuntimeUsageDelta::for_operation(&operation, usage_deltas)?;
        Self::persisted_state_with_graph_commit_and_staged_usage_and_budget(
            state,
            graph,
            &usage_deltas,
            operation,
            commit_budget,
            fleet_format,
        )
    }

    pub fn persisted_state_with_graph_commit_and_staged_usage_and_budget(
        state: &crate::RuntimeSessionState,
        graph: GraphAppend,
        usage_deltas: &[RuntimeUsageDelta],
        operation: OperationId,
        commit_budget: CommitBudget,
        fleet_format: FleetFormat,
    ) -> Result<Self, StoreError> {
        let current_frame_node_id = graph.derive_current_frame_node_id(&state.session_graph);
        Ok(Self {
            commit_budget,
            session_id: state.session_id.clone(),
            expected_head_revision: state.head_revision,
            session_execution_lease_fence: None,
            drive_fence: None,
            root_terminal: None,
            release_session_execution_lease: None,
            config: persisted_session_config_from_state(state),
            current_frame_node_id,
            graph,
            graph_base_leaf_node_id: state.session_graph.leaf_node_id.clone(),
            checkpoint: build_checkpoint_from_persisted_state(state, fleet_format)?,
            usage_deltas: usage_deltas.to_vec(),
            failure_evidence: Vec::new(),
            turn_commit: RuntimeTurnCommitStamp::new(operation),
            queued_run: None,
            completed_queue_claims: Vec::new(),
            completed_turn_input_claims: Vec::new(),
            undelivered_turn_input_claims: Vec::new(),
            pending_follow_on: state.pending_follow_on.as_deref().cloned(),
            interrupted_turn_input_turn_id: None,
            interrupted_turn_input_cancellation: None,
            interrupted_turn_cancel_intent: None,
            turn_cancel_closure_settlement: None,
            adopted_intent_rows: 0,
            committed_attachment_ids: Vec::new(),
        })
    }

    /// Derive append-node identities, stamp the operation, and return the
    /// old-to-derived id mapping so callers can remap any resident graph that
    /// supplied the commit.
    pub fn with_operation(
        mut self,
        operation: OperationId,
    ) -> Result<(Self, Vec<(crate::NodeId, crate::NodeId)>), StoreError> {
        let session_id = self.session_id.clone();
        let node_id_mapping = self.graph.derive_node_ids(&session_id, &operation)?;
        remap_optional_node_id(&mut self.current_frame_node_id, &node_id_mapping);
        self.turn_commit = RuntimeTurnCommitStamp::new(operation);
        Ok((self, node_id_mapping))
    }

    /// Adds exact lease-completion evidence for store implementors to release atomically with the
    /// runtime commit rather than in a separate raceable write.
    pub fn releasing_session_execution_lease(
        mut self,
        completion: SessionExecutionLeaseAuthority,
    ) -> Self {
        self.release_session_execution_lease = Some(completion);
        self
    }

    /// Requires the caller's current authority without changing lane ownership.
    pub fn borrowing_session_execution_lease(
        mut self,
        fence: SessionExecutionLeaseAuthority,
    ) -> Self {
        self.session_execution_lease_fence = Some(fence);
        self
    }

    /// Adds one queued-work completion for store implementors to settle atomically with the runtime
    /// commit.
    pub fn completing_queue_claim(
        mut self,
        completed_queue_claim: crate::QueuedWorkCompletion,
    ) -> Self {
        self.completed_queue_claims.push(completed_queue_claim);
        self
    }

    /// Adds queued-work completions in caller order for store implementors to settle atomically
    /// with the runtime commit.
    pub fn completing_queue_claims(
        mut self,
        completed_queue_claims: impl IntoIterator<Item = crate::QueuedWorkCompletion>,
    ) -> Self {
        self.completed_queue_claims.extend(completed_queue_claims);
        self
    }

    /// Adds one turn-input completion for store implementors to settle atomically with the runtime
    /// commit.
    pub fn completing_turn_input_claim(
        mut self,
        completed_turn_input_claim: crate::TurnInputCompletion,
    ) -> Self {
        self.completed_turn_input_claims
            .push(completed_turn_input_claim);
        self
    }

    /// Adds turn-input completions in caller order for store implementors to settle atomically with
    /// the runtime commit.
    pub fn completing_turn_input_claims(
        mut self,
        completed_turn_input_claims: impl IntoIterator<Item = crate::TurnInputCompletion>,
    ) -> Self {
        self.completed_turn_input_claims
            .extend(completed_turn_input_claims);
        self
    }

    /// Marks one interrupted turn so store implementors atomically settle its
    /// undelivered active-turn inputs. `cancellation` is the exact evidence
    /// returned by the authoritative keyed gate; absence explicitly selects
    /// ordinary non-cancellation re-deferral.
    pub fn deferring_interrupted_turn_inputs(
        mut self,
        turn_id: impl Into<TurnId>,
        cancellation: Option<crate::TurnCancellationEvidence>,
    ) -> Self {
        self.interrupted_turn_input_turn_id = Some(turn_id.into());
        self.interrupted_turn_input_cancellation = cancellation;
        self.interrupted_turn_cancel_intent = Some(crate::TurnCancelIntentSnapshot::Absent);
        self
    }

    /// Replaces the attachment-ID set that manifest implementors must promote atomically with this
    /// runtime commit; caller order is preserved.
    pub fn with_committed_attachments(
        mut self,
        attachment_ids: impl IntoIterator<Item = crate::AttachmentId>,
    ) -> Self {
        self.committed_attachment_ids = attachment_ids.into_iter().collect();
        // Adoption is keyed on (session, attachment), so duplicate ids stamp
        // one manifest row — count unique ids. This builder only knows the
        // explicit list; the production path recomputes the count as the
        // deduped union with the turn's recorded write-ahead intents and
        // overwrites this value (runtime/turn_boundary.rs). ADR 0058 accepts
        // the remaining residual against the stamped row count — admission
        // never queries the store.
        self.adopted_intent_rows = self
            .committed_attachment_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            .try_into()
            .unwrap_or(u64::MAX);
        self
    }
}

/// The perf harness needs this test-gated seam to isolate receipt derivation
/// and real backend publication without adding timing hooks to production. It
/// deliberately stops before the host-owned parts of the production sequence:
/// protocol-plugin mutation/rollback, live plugin-state stamping, the host clock
/// (this hook uses `SystemClock`), staged token-ledger merging, and fresh
/// session-execution-lease acquisition. Keep those divergences visible here when
/// the production append sequence changes.
#[cfg(any(test, feature = "testing"))]
pub fn append_request_commit_for_testing(
    state: &mut crate::RuntimeSessionState,
    operation_id: &str,
    nodes: &[crate::SessionAppendNode],
    requested_ancestor_node_id: Option<&str>,
) -> Result<RuntimeCommit, StoreError> {
    append_request_commit_with_clock_for_testing(
        state,
        operation_id,
        nodes,
        requested_ancestor_node_id,
        &crate::SystemClock,
    )
}

#[expect(
    clippy::expect_used,
    reason = "`FrameNodeId::new` rejects only the empty string, and a remapping target is a derived node id, never empty"
)]
fn remap_optional_node_id(
    node_id: &mut Option<crate::FrameNodeId>,
    mapping: &[(crate::NodeId, crate::NodeId)],
) {
    let Some(current) = node_id.as_mut() else {
        return;
    };
    if let Some((_, derived)) = mapping.iter().find(|(draft, _)| draft == current.as_str()) {
        *current = crate::FrameNodeId::new(derived.as_str())
            .expect("derived graph node identities are non-empty");
    }
}

fn persisted_session_state_from_head(
    head: SessionHead,
    checkpoint: Option<HydratedSessionCheckpoint>,
    fleet: FleetFormat,
) -> Result<crate::RuntimeSessionState, StoreError> {
    // A cold load adopts the head onto a default state: every durable fact
    // comes from the head (adoption is head-authoritative, FIG-1875), and the
    // live-owned runtime-lease facts start from their defaults — the head's
    // turn budget, and no live session-id binding yet.
    let mut state =
        crate::RuntimeSessionState::new(crate::SessionPolicy::new(head.config.turn_budget));
    let live_owned = crate::runtime::state::LiveOwnedSessionFacts::of(&state.policy);
    crate::runtime::state::adopt_durable_head(&mut state, &head, checkpoint, live_owned, fleet)?;
    Ok(state)
}

#[cfg(any(test, feature = "testing"))]
impl Default for SessionHead {
    fn default() -> Self {
        Self {
            session_id: default_root_session_id(),
            head_revision: 0,
            current_frame_node_id: None,
            pending_follow_on: None,
            graph: crate::SessionGraph::default(),
            config: crate::PersistedSessionConfig::new(crate::TurnBudget::Unbounded),
            checkpoint_ref: None,
            token_ledger: Vec::new(),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl Default for SessionHeadPayload {
    fn default() -> Self {
        Self {
            schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
            session_id: default_root_session_id(),
            config: crate::PersistedSessionConfig::new(crate::TurnBudget::Unbounded),
            current_frame_node_id: None,
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
/// paste no-op manifest impls via
/// [`impl_noop_attachment_manifest!`](crate::impl_noop_attachment_manifest).
///
/// Checkpoint components have one backend-independent durable shape. When a
/// commit supplies a tool-state, plugin-state, or execution-state body, the
/// backend must store it under a content ref and return that ref in
/// [`RuntimeCommitReceipt::manifest`]. A later commit may carry the ref without
/// the body to mean "unchanged"; the backend must resolve the existing body
/// when hydrating the checkpoint. A ref-only commit whose component is absent
/// must fail instead of persisting a checkpoint that hydrates to `None`.
#[async_trait::async_trait]
pub trait SessionCommitStore: AttachmentManifest + Send + Sync {
    /// Legacy absent markers mean zero.
    async fn read_session_state_version(&self) -> Result<u32, StoreError> {
        Ok(OLDEST_SUPPORTED_SESSION_STATE_VERSION)
    }
    /// Revalidate `lease`, then classify the independently read session-state marker.
    async fn admit_session_state(
        &self,
        lease: &SessionExecutionLeaseAuthority,
    ) -> Result<SessionStateAdmission, StoreError> {
        let version = self.read_session_state_version().await?;
        Ok(SessionStateAdmission {
            session_id: lease.session_id.clone(),
            version,
            lease_fencing_token: lease.fencing_token,
        })
    }

    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError>;

    /// Read the current session head without hydrating graph, checkpoint, or
    /// usage history.
    ///
    /// Implementations must project this from at most one durable row. Runtime
    /// freshness checks depend on the revision, leaf, and checkpoint reference
    /// all being present in this read. The read must use the same session
    /// resolution and binding semantics as [`SessionCommitStore::load_session`]
    /// so the two projections agree about session presence. `Ok(None)` means
    /// resolution completed and found no readable session; inability to
    /// determine the head must be returned as `Err`, never collapsed to absence.
    async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError>;

    /// The session as it stood at `base`, the head one of its turns was
    /// admitted on (FIG-3682).
    ///
    /// A replay of an admitted turn rebuilds the turn's input state from this
    /// read, never from the live head: the turn's own commit, or a lane
    /// service, may have advanced the head since. The read carries the graph
    /// along `base.leaf` and the checkpoint `base.checkpoint`; the session's
    /// configuration, frame, usage ledger and failure settlements are the live
    /// ones. `base` always names a leaf or a checkpoint: the head before the
    /// session's first commit is rebuilt by the caller without a store read.
    ///
    /// A backend refuses [`StoreError::TurnBaseNotRetained`] when it no longer
    /// holds `base`, and never answers with another head. The default reads
    /// only the live head, so it answers exactly when the live head is still
    /// `base`; a backend that retains superseded heads
    /// ([`retain_admission_base`](Self::retain_admission_base)) overrides it.
    async fn load_session_at(
        &self,
        base: &SessionHeadRef,
    ) -> Result<PersistedSessionRead, StoreError> {
        match self.load_session().await? {
            Some(read)
                if read.head_revision == base.revision
                    && read.graph.leaf_node_id == base.leaf
                    && read.checkpoint_ref == base.checkpoint =>
            {
                Ok(read)
            }
            _ => Err(StoreError::TurnBaseNotRetained {
                revision: base.revision,
            }),
        }
    }

    /// Keep `base` readable by [`load_session_at`](Self::load_session_at)
    /// until the session's next admission replaces it (FIG-3682).
    ///
    /// Called by a turn's admission under the session's execution lease, once
    /// per first execution. While it stands, maintenance that reclaims
    /// unreferenced checkpoints treats `base.checkpoint` as a root, so a
    /// replay of the admitted turn can rebuild its input state even after the
    /// turn's own commit superseded the head and a vacuum ran. A backend that
    /// never reclaims a superseded checkpoint has nothing to do, which is the
    /// default.
    async fn retain_admission_base(
        &self,
        _lease: &SessionExecutionLeaseAuthority,
        _base: &SessionHeadRef,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, StoreError>;

    /// Does this session hold a durable commit receipt for `turn_id`?
    ///
    /// The narrowest possible read of the committed-turn fact every backend
    /// already writes with [`commit_runtime_state`](Self::commit_runtime_state):
    /// true means that turn's commit is durable, false means it is not (yet).
    /// It carries no ordering and no turn contents, so it stays a membership
    /// test rather than a second history projection.
    ///
    /// The parent-end recovery sweep is its only caller. A turn's parent-end
    /// ledger row is written to the process registry immediately after the
    /// turn commit, and the two are separate stores, so a crash between them
    /// leaves live children naming a turn that will never end again. Recovery
    /// re-derives the row only for candidate turns this read confirms; an
    /// uncommitted candidate is left alone, because a turn that crashed before
    /// its commit is interrupted rather than ended and its redrive re-registers
    /// exactly the children a sweep would have cancelled.
    ///
    /// The default refuses. Backends that report parent-end recovery candidates
    /// must implement it; a backend that reports none is never asked.
    async fn committed_turn_exists(&self, _turn_id: &crate::TurnId) -> Result<bool, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "committed_turn_exists",
        })
    }

    /// Does this session hold a durable end receipt for `drain_id`?
    ///
    /// The same membership read as [`committed_turn_exists`](Self::committed_turn_exists),
    /// keyed on the drain's `final` receipt: true means the drain's epilogue
    /// committed, false means it did not (yet). The parent-end recovery sweep
    /// is its only caller; a drain interrupted before its epilogue is left
    /// alone for the retried drain under the same `drain_id` to end.
    ///
    /// The default refuses. Backends that report parent-end recovery
    /// candidates must implement it; a backend that reports none is never
    /// asked.
    async fn drain_end_exists(&self, _drain_id: &str) -> Result<bool, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "drain_end_exists",
        })
    }

    /// Atomically persist one settled runtime commit and its durable receipt.
    ///
    /// A commit carrying [`RuntimeCommit::session_execution_lease_fence`]
    /// borrows a turn driver's held lane without claiming, rotating, renewing,
    /// or releasing it. Implementors must validate that fence inside the write
    /// transaction and before receipt lookup, so stale or expired authority
    /// vetoes even an otherwise replayable operation identity.
    ///
    /// Implementors must look up the `(session_id, operation storage key)`
    /// receipt inside the write transaction before the fresh append ancestor
    /// fence and head-revision compare-and-swap. Existing receipts must be
    /// adjudicated with [`decide_runtime_commit_receipt`]: replay returns the
    /// stored first-attempt [`RuntimeCommitReceipt`] with only
    /// [`RuntimeCommitReceipt::receipt_replayed`] set transiently, applies none
    /// of the attempted commit envelope, and may release the attempt's explicit
    /// execution-lease completion. Conflicts and corrupt count cross-checks
    /// mutate nothing.
    ///
    /// Every [`RuntimeUsageDelta`] is published idempotently on `(session_id,
    /// operation_storage_key, entry_ordinal, payload_encoding_version,
    /// payload_hash)`, where the versioned hand-written projection is
    /// documented on [`RuntimeUsageDeltaIdentity`]. A duplicate full identity
    /// is a no-op inside this same transaction. Fresh results list every
    /// identity made durable by the commit in
    /// [`RuntimeCommitReceipt::committed_usage_delta_identities`]; stored
    /// receipt results retain the original attempt's list so callers do not
    /// clear staged rows that the original transaction never carried.
    ///
    /// A fresh identity-bearing append enforces
    /// the optional ancestor in [`AppendRequestIdentity::Append`] against the
    /// transaction's active path, then atomically publishes graph, checkpoint,
    /// usage, queue/input settlements, attachment adoptions, and a receipt whose
    /// stored replay bit is `false`. Receipt lookup, fresh-only ancestor fencing,
    /// commit publication, and receipt insertion are one transaction.
    ///
    /// A queued-work completion that no longer owns a named row must return
    /// [`StoreError::QueuedWorkClaimSuperseded`] with its `row_id`. If another
    /// claim owns the live row, the error also carries its claim ID and lease
    /// generation so recovery preserves peer rows without weakening fencing.
    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError>;

    /// The follow-on the session head owes, if any (ADR 0101 §3): the head's
    /// `pending_follow_on` as it is committed now.
    ///
    /// Drive admission asks this to decide whether the follow-on is the next
    /// work it admits. It reads one head fact and is not a freshness probe of
    /// the resident head; the default reads it from the head meta.
    async fn load_pending_follow_on(&self) -> Result<Option<PendingFollowOn>, StoreError> {
        Ok(self
            .load_session_head_meta()
            .await?
            .and_then(|head| head.pending_follow_on))
    }

    /// Raise the head's pending follow-on recovery count by one (ADR 0101 §3).
    ///
    /// A drive that recovers a pending follow-on calls this before the
    /// follow-on's first effect. Implementations must, in one transaction,
    /// validate `lease` with the ordinary session-execution fence, refuse with
    /// [`StoreError::FollowOnNotPending`] unless the head's
    /// `pending_follow_on_json` names `follow_on_turn_id`, and write the fact
    /// back with `attempts` raised by one. The head revision does not move:
    /// the raise changes no other head fact, and nothing ever lowers the count.
    /// Returns the raised fact.
    async fn raise_pending_follow_on_attempts(
        &self,
        lease: &SessionExecutionLeaseAuthority,
        follow_on_turn_id: &crate::TurnId,
    ) -> Result<PendingFollowOn, StoreError>;

    /// Admit `binding.session_id` to this store and bind this handle to it.
    ///
    /// This is the authoritative durable admission seam for pre-opened stores,
    /// session-initialisation children, and parked resume. `SessionStoreFactory::create_store`
    /// is a convenience that must produce the same admission decision.
    ///
    /// Implementations must atomically:
    ///
    /// 1. reject an empty id with [`StoreError::InvalidSessionId`];
    /// 2. reject a permanent tombstone with [`StoreError::SessionDeleted`];
    /// 3. reject a handle bound to another id with
    ///    [`StoreError::SessionBindingMismatch`];
    /// 4. create metadata exactly from `binding` when absent, without replacing
    ///    existing metadata, and return [`SessionAdmission::Created`];
    /// 5. leave an already-bound same-id session unchanged and return
    ///    [`SessionAdmission::Rebound`];
    /// 6. reject a rebind whose `binding.relation` declares a lineage that
    ///    disagrees with the recorded one with
    ///    [`StoreError::SessionRelationMismatch`], leaving the stored metadata
    ///    unchanged. The relation is a durable fact, so the conflict is
    ///    answered rather than absorbed. [`SessionRelation::Root`] declares no
    ///    lineage — it is what every resume and plain reopen carries — so it
    ///    always rebinds; causal provenance and pending observer intents are not
    ///    compared. Use
    ///    [`store_backend_support::guard_rebind_lineage`](crate::store_backend_support::guard_rebind_lineage)
    ///    so all backends answer identically.
    async fn admit_and_bind_session(
        &self,
        binding: &SessionBinding,
    ) -> Result<SessionAdmission, StoreError>;

    /// Write this session's metadata, creating the row when it is absent.
    ///
    /// The recorded lineage is write-once. `meta.relation` must declare the
    /// same lineage the existing row records — the same parent, or the same
    /// fork source and node — or the write is refused with
    /// [`StoreError::SessionRelationMismatch`] and the row is left unchanged.
    /// Admission reads [`SessionRelation::Root`] as "no claim" on a rebind
    /// because a resume declares no lineage; a write cannot, because the row
    /// it would record replaces the recorded parent with that root. Causal
    /// provenance and the pending observer intents are
    /// not lineage and are replaced as given, which is what lets the observer
    /// intent settlement round-trip the metadata it loaded. Use
    /// [`store_backend_support::guard_session_meta_relation_rewrite`](crate::store_backend_support::guard_session_meta_relation_rewrite)
    /// so all backends answer identically.
    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError>;
    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError>;

    /// Record that the session's turn parked (FIG-3586, FIG-3600, FIG-3659).
    ///
    /// Written on the abort path of a turn whose refusal parks it, before its
    /// lease is released. A first park allocates the feed sequence the record's
    /// `park_id` names and appends a `Parked` event; a re-park of the same
    /// turn keeps `park_id` and `since_ms`, bumps `attempts` and
    /// `last_refused_ms`, and writes no event; a different turn's park
    /// supersedes the stored one (`Unparked{Superseded}` then `Parked`).
    /// Any commit of the session clears the park in the commit's transaction,
    /// as does a cancel that releases the parked turn's claim and the
    /// session's deletion: a park is live exactly while its turn is.
    ///
    /// Returns the record as stored, so the caller can report the allocated
    /// `park_id` and attempt count.
    async fn record_turn_park(
        &self,
        _park: &crate::store::TurnParkWrite,
    ) -> Result<crate::store::TurnPark, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "record_turn_park",
        })
    }

    /// The session's parked turn, if its turn is parked.
    async fn load_turn_park(
        &self,
        _session_id: &crate::SessionId,
    ) -> Result<Option<crate::store::TurnPark>, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "load_turn_park",
        })
    }
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
    /// Persist or validate the one cancellation authority selected for this
    /// session and, for a Process or runtime-operation controller, its physical
    /// journal scope. Session-bound turns keep their exact canonical address in
    /// each closure authorization, so distinct turns may share this authority.
    /// The check occurs under the current execution fence before any session
    /// work and never replaces the original selection.
    async fn validate_turn_cancellation_binding(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &crate::ExecutionScope,
    ) -> Result<(), StoreError>;

    /// Authorize exact closure of one cancellation gate pair for the admitted
    /// session and binding. The recorded lease identity authenticates the
    /// proposal, but its liveness and generation do not fence final settlement.
    /// A vacant slot accepts this value, an identical retry adopts it, and a
    /// different occupied value or retired physical scope returns a typed refusal.
    /// Final publication additionally requires the session-head CAS.
    async fn authorize_turn_cancel_closure(
        &self,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        authorization: &crate::TurnCancelClosureAuthorization,
    ) -> Result<crate::TurnCancelClosureAuthorizationOutcome, StoreError>;

    /// Load every unconsumed closure obligation for the bound session after
    /// validating the current execution fence, selected binding, and any
    /// original non-session physical scope.
    async fn pending_turn_cancel_closures(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &crate::ExecutionScope,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;

    /// Read unconsumed closure pins for lifecycle coordination without
    /// acquiring execution authority. This grants no right to settle or
    /// consume them; deletion and scope-retirement owners use it only to refuse
    /// destructive cleanup until an activation holder has drained the pins.
    async fn pending_turn_cancel_closure_pins(
        &self,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "pending_turn_cancel_closure_pins",
        })
    }

    /// Whether this turn's final runtime commit receipt is already durable.
    /// This closes the store-commit-to-terminal-publication window for late
    /// cancellation requests.
    async fn turn_is_committed(&self, _address: &crate::TurnAddress) -> Result<bool, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "turn_is_committed",
        })
    }

    /// Persist cancellation intent for one turn.
    ///
    /// Repeating the address returns the original request unless the incoming
    /// one is a timing escalation of it
    /// ([`TurnCancelRequest::escalates`](crate::TurnCancelRequest::escalates)):
    /// same undelivered-input disposition, stronger mode. A repeat that
    /// disagrees about the disposition is a conflict the authoritative gate
    /// refuses, and leaves both the row and its intent revision untouched.
    /// This row is provisional evidence until
    /// [`Self::reconcile_turn_cancel_winner`] projects the keyed-gate winner.
    /// If the turn's final receipt is already durable, implementations perform
    /// no write and may return the incoming request with no outcome rather than
    /// decode retained historical outcome payloads.
    async fn record_turn_cancel_request(
        &self,
        _request: crate::TurnCancelRequest,
    ) -> Result<crate::TurnCancelRequestRecord, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "record_turn_cancel_request",
        })
    }

    /// Read the durable cancellation request and any accumulated repair
    /// outcome for one turn.
    async fn turn_cancel_request(
        &self,
        _address: &crate::TurnAddress,
    ) -> Result<Option<crate::TurnCancelRequestRecord>, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "turn_cancel_request",
        })
    }

    /// Read only durable cancellation intent, without reconstructing affected
    /// input payloads. Recovery uses this after vacuum may have reclaimed
    /// payload tombstones belonging to an earlier repair of the same turn id.
    async fn turn_cancel_request_intent(
        &self,
        _address: &crate::TurnAddress,
    ) -> Result<crate::TurnCancelIntentSnapshot, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "turn_cancel_request_intent",
        })
    }

    /// Project the authoritative keyed-gate winner into durable request
    /// evidence without changing arbitration authority.
    async fn reconcile_turn_cancel_winner(
        &self,
        _address: &crate::TurnAddress,
        _observed: &crate::TurnCancelIntentSnapshot,
        _evidence: &crate::TurnCancellationEvidence,
    ) -> Result<bool, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "reconcile_turn_cancel_winner",
        })
    }

    /// Persist model-visible user input into the pending turn-input lifecycle.
    ///
    /// A draft that carries its own `input_id` names one admission: when a
    /// row with that id exists, an identical submission returns it unchanged
    /// (whatever its lifecycle state) and anything else is
    /// [`StoreError::PendingTurnInputIdConflict`]. A journaled turn acceptance
    /// provisions its id this way, so re-running its body never admits a
    /// second row (ADR 0069 §6).
    async fn enqueue_pending_turn_input(
        &self,
        input: crate::PendingTurnInputDraft,
    ) -> Result<crate::PendingTurnInput, StoreError>;

    /// List open user inputs for reconciliation or queue preview.
    ///
    /// Completed and cancelled rows are excluded. A claim matching the
    /// currently live session-execution-lease generation is returned as
    /// [`PendingTurnInputReadStatus::Held`](crate::PendingTurnInputReadStatus::Held)
    /// with that lease's exact expiry. Expired, released, and mismatched
    /// generations are returned as pending under ADR 0029; this read never
    /// infers whether a holder process is alive. A row whose claim is bound to
    /// an aborted direct turn is returned as
    /// [`PendingTurnInputReadStatus::TurnBound`](crate::PendingTurnInputReadStatus::TurnBound)
    /// whatever generation holds the lease (FIG-3589). Resubmitting the same input
    /// while its row is held creates a duplicate admission once the held row's
    /// original claim returns; hosts must wait out the reported expiry or
    /// reuse the same source key.
    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::PendingTurnInputRead>, StoreError>;

    /// Read canonical input applications from durable turn-commit records.
    ///
    /// Unlike live observation replay, this surface is not retention-window
    /// dependent. Implementations return settled applications in durable
    /// commit order so a host can reconcile admission identity after a gap.
    ///
    /// The default refuses as an unsupported operation: a store that keeps
    /// no application records cannot answer which turn applied an input.
    async fn list_turn_input_applications(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<crate::TurnInputApplication>, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "list_turn_input_applications",
        })
    }

    /// Cancel an unclaimed pending user input by id.
    ///
    /// A row bound to an aborted direct turn is cancellable by the input its
    /// receipt names, which returns the bound claim's other rows to the queue.
    /// A cancel of another bound row that does not also cover the receipt's
    /// input is refused as
    /// [`PendingTurnInputCancelOutcome::TurnBound`](crate::PendingTurnInputCancelOutcome::TurnBound)
    /// (FIG-3589).
    ///
    /// Provided convenience: the singular form is exactly
    /// [`cancel_pending_turn_inputs`](Self::cancel_pending_turn_inputs) with a
    /// one-element target list, so backends implement only the plural
    /// primitive.
    async fn cancel_pending_turn_input(
        &self,
        session_id: &SessionId,
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
        session_id: &SessionId,
        targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, StoreError>;

    /// Atomically cancel the same-session runtime-admission suffix from an anchor.
    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &SessionId,
        anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, StoreError>;

    /// Claim active-turn input at a checkpoint for the live turn id.
    ///
    /// The claim pins the caller's live session-execution-lease generation
    /// (`session_execution_lease.fencing_token`) rather than a TTL; it is live
    /// exactly while that generation still holds the session lease (ADR 0029).
    async fn claim_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError>;

    /// Claim queued next-turn input at idle.
    async fn claim_next_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError>;

    /// Abandon a held pending-turn-input claim so it can be reclaimed.
    async fn abandon_turn_input_claim(
        &self,
        claim: &crate::WorkClaim<crate::runtime::TurnInputClaimData>,
    ) -> Result<(), StoreError>;

    /// Release multiple held pending-turn-input claims in one backend batch.
    async fn abandon_turn_input_claims(
        &self,
        claims: &[crate::WorkClaim<crate::runtime::TurnInputClaimData>],
    ) -> Result<(), StoreError> {
        for claim in claims {
            self.abandon_turn_input_claim(claim).await?;
        }
        Ok(())
    }

    /// Bind the rows `claim` still holds to `turn_id`, the direct turn that
    /// drove them and aborted with `Err` (FIG-3589, ADR 0069 §7).
    ///
    /// A bound row keeps its claim, so a redrive of `turn_id` that replays the
    /// journaled drive settles it with the claim token the journal recorded.
    /// It stops lapsing with the claim's lease generation: no next-turn or
    /// checkpoint claim takes it, and it is not claimable work, under any
    /// generation. Only that settlement, [`Self::reclaim_turn_bound_inputs`]
    /// for the same turn, or a cancel of one of its rows releases it; the
    /// cancel returns the claim's other rows to the queue.
    ///
    /// Conditional on the claim: a row another driver settled or reclaimed
    /// meanwhile no longer carries it and is left alone, so binding a claim
    /// that no longer holds anything is a no-op. Only next-turn rows are
    /// bound. A crashed turn never reaches this call, so its claim still lapses
    /// with its generation and a successor recovers it (ADR 0029).
    ///
    /// `receipt_input_id` is the input the aborted turn's acceptance receipt
    /// names; a cancel of any other bound row is refused
    /// ([`PendingTurnInputCancelOutcome::TurnBound`](crate::PendingTurnInputCancelOutcome::TurnBound)).
    /// Rows a pending queued run owns are never bound: they lapse to the run.
    async fn bind_turn_input_claim(
        &self,
        _claim: &crate::WorkClaim<crate::runtime::TurnInputClaimData>,
        _turn_id: &crate::TurnId,
        _receipt_input_id: &crate::InputId,
    ) -> Result<(), StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "bind_turn_input_claim",
        })
    }

    /// [`Self::bind_turn_input_claim`] for a turn that does not know its
    /// drive claim: the drive effect failed after its body claimed rows but
    /// before its outcome reached the turn (FIG-3589).
    ///
    /// Binds the rows that share the claim identity the receipt's row
    /// `receipt_input_id` carries, provided that claim was taken under lease
    /// generation `generation`, the generation the failed drive ran under. The
    /// identity is read and the rows are bound in one transaction, fenced by
    /// that claim id and token. A receipt row that is unclaimed, or claimed
    /// under another generation, binds nothing.
    async fn bind_turn_input_claim_of_receipt(
        &self,
        _session_id: &SessionId,
        _receipt_input_id: &crate::InputId,
        _generation: u64,
        _turn_id: &crate::TurnId,
    ) -> Result<(), StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "bind_turn_input_claim_of_receipt",
        })
    }

    /// Claim, under the caller's live fence, every row bound to the aborted
    /// turn `turn_id`, releasing the binding (FIG-3589).
    ///
    /// The redrive of an aborted turn whose drive the effect host did not
    /// journal re-takes exactly the rows its first execution drove. A redrive
    /// that replays a journaled drive never calls this: it settles with the
    /// recorded claim. `None` when nothing is bound to `turn_id`; a store that
    /// never binds has nothing bound.
    async fn reclaim_turn_bound_inputs(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &SessionExecutionLeaseAuthority,
        _owner: &LeaseOwnerIdentity,
        _turn_id: &crate::TurnId,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError> {
        Ok(None)
    }

    /// Discover distinct turn ids with active-turn-scoped inputs eligible for
    /// recovery consideration under the caller's fence and scope.
    ///
    /// An input routed into a running turn is persisted `pending_active` (or
    /// `accepted` once that turn claims it) and addressed only by the turn id it
    /// names. The turn's own final commit is what normally re-defers whatever it
    /// did not deliver ([`RuntimeCommit::deferring_interrupted_turn_inputs`]),
    /// so a turn that stops without committing strands its inputs in a state no
    /// next-turn drain can claim and no later turn id can address. This is the
    /// recovery candidate for that state. Discovery does not establish whether
    /// a turn ended or whether cancellation won; the caller must consult
    /// durable intent and the keyed gate before asking for exact-turn repair.
    async fn orphaned_active_turn_ids(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &SessionExecutionLeaseAuthority,
        _scope: OrphanedTurnInputScope<'_>,
    ) -> Result<Vec<crate::TurnId>, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "orphaned_active_turn_ids",
        })
    }

    /// Repair one orphan only after the caller has consulted the authoritative
    /// cancellation gate.
    ///
    /// The store rechecks the execution fence and row eligibility in the same
    /// transaction as the mutation. `NoCancellationIntent` additionally
    /// rechecks that no request row exists, closing the request-versus-repair
    /// race without resolving a gate for a pre-named turn that may start later.
    /// A displaced or expired fence is refused with
    /// [`StoreError::SessionExecutionLeaseExpired`].
    async fn repair_orphaned_active_turn_inputs(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &SessionExecutionLeaseAuthority,
        _turn_id: &crate::TurnId,
        _observed: &crate::TurnCancelIntentSnapshot,
        _settlement: Option<&crate::TurnCancelClosureSettlement>,
    ) -> Result<TurnCancelRepairResult, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "repair_orphaned_active_turn_inputs",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnCancelRepairResult {
    Applied(crate::TurnCancelInputOutcome),
    IntentChanged,
}

impl TurnCancelRepairResult {
    pub fn into_applied(self) -> Option<crate::TurnCancelInputOutcome> {
        match self {
            Self::Applied(outcome) => Some(outcome),
            Self::IntentChanged => None,
        }
    }
}

/// Explicit cancellation authority supplied to one orphan-input repair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnCancelRepairDecision {
    /// The keyed gate settled with this cancellation evidence.
    CancellationWon(crate::TurnCancellationEvidence),
    /// The keyed gate was already sealed by completion.
    CancellationDidNotWin,
    /// No request existed when recovery observed the turn. The store must
    /// recheck absence transactionally and skip repair if intent appeared.
    NoCancellationIntent,
}

impl TurnCancelRepairDecision {
    pub fn disposition(&self) -> crate::TurnCancelDisposition {
        match self {
            Self::CancellationWon(evidence) => evidence.undelivered,
            Self::CancellationDidNotWin | Self::NoCancellationIntent => {
                crate::TurnCancelDisposition::Defer
            }
        }
    }
}

/// Bounds which active-turn-scoped rows
/// [`TurnInputStore::orphaned_active_turn_ids`] may consider for recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrphanedTurnInputScope<'a> {
    /// Exactly the rows pinned to one named turn after its local execution
    /// path tore down without a commit.
    Turn(&'a crate::TurnId),
    /// Rows not pinned to the caller's live lane generation, excluding the
    /// caller-provided resumable turn and its agent-frame follow-ons.
    ///
    /// The resumable identity is a conservative exclusion, not lifecycle
    /// authority. Other candidates can include a pre-named future turn, so
    /// discovery alone must never resolve a cancellation gate. With no durable
    /// request, exact repair may only perform the existing ordinary re-defer
    /// after transactionally confirming request absence.
    LaneGeneration {
        resumable_turn_id: Option<&'a TurnId>,
    },
}

/// Durable single-writer execution-lane capability, fenced by monotonic
/// fencing tokens.
#[async_trait::async_trait]
pub trait SessionExecutionLeaseStore: Send + Sync {
    /// Try to claim the durable single-writer execution lane for `session_id`.
    /// Returns [`SessionExecutionLeaseClaimOutcome::Busy`] when another owner
    /// holds an unexpired lease. Expired or released leases may be reclaimed
    /// and receive a higher fencing token. A live claim reenters only when its
    /// owner id, boot incarnation, and runtime-minted executor id all match;
    /// reentry rotates the lease token but preserves the fencing generation.
    /// Renewal never rotates either token. Any live mismatch is busy.
    ///
    /// A granted claim must carry
    /// [`SessionExecutionLeaseAcquisition::displaced`] naming the lapsed holder
    /// it took the lane from, read inside the same atomic operation. This is the
    /// only truthful report of a takeover: the displaced runner is frequently
    /// dead or frozen (that is why its lease lapsed), so nothing it would have
    /// logged is guaranteed to happen. A claim that displaced nobody, including
    /// exact owner/incarnation/executor reentry and a reclaim of a released row,
    /// reports `None`.
    /// `executor_id` is the caller's own runtime-open discriminator and must be
    /// supplied: it is identity, and a default that minted one per call would
    /// make reentry unreachable through this method while silently changing the
    /// claimant on every retry. Only the claim nonce - a per-attempt
    /// capability, not identity - is minted here, so a caller that needs one
    /// nonce across an ambiguous-outcome retry uses
    /// [`try_claim_session_execution_lease_with_token`](Self::try_claim_session_execution_lease_with_token)
    /// directly.
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &SessionId,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        let claim_nonce = LeaseClaimNonce::new();
        self.try_claim_session_execution_lease_with_token(
            session_id,
            owner,
            executor_id,
            &claim_nonce,
            lease_ttl_ms,
        )
        .await
    }

    /// Try one retry-safe claim attempt using an opaque claim nonce.
    ///
    /// The caller mints [`LeaseClaimNonce::new`] once for the logical claim and
    /// borrows it again only for an ambiguous-outcome retry. With no value-taking
    /// constructor, stable host identity cannot accidentally be
    /// reused as claim identity. Backends persist the nonce bytes as the lease
    /// token so a retry observes one settled rotation instead of rotating again.
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &SessionId,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError>;

    /// Extend a live session execution lease owned by the caller.
    ///
    /// Backends reject expired authority with [`StoreError::SessionExecutionLeaseExpired`];
    /// stale, released, or superseded owner/token authority uses
    /// [`StoreError::SessionExecutionLeaseRenewalRefused`] with structured decision evidence.
    /// Granted renewals echo the presented session and owner, never rotate either
    /// token, and return expiry at least as late; core refuses install otherwise.
    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError>;

    /// Release a session execution lease predicated on its owner and lease token.
    ///
    /// A stale, repeated, released, or superseded completion is refused with
    /// [`StoreError::SessionExecutionLeaseReleaseRefused`] and must not clear a
    /// successor lease. The fencing token remains generation evidence; lock
    /// lifecycle uses owner plus lease token. Named refusals record structured evidence.
    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError>;

    /// Read the current session-execution-lease row without claiming it.
    ///
    /// Returns the store-clock instant sampled alongside the optional persisted
    /// lease. The lease is `None` when the row is absent, unleased, or released.
    /// A returned lease may already be expired: expiry is a raw fact exposed
    /// read-side, mirroring
    /// [`ProcessRegistry::get_process_lease`](crate::ProcessRegistry::get_process_lease),
    /// so callers classify staleness themselves. This never mutates the lease
    /// and never advances a generation. Unknown session ids return an
    /// observation whose lease is `None`.
    ///
    /// This read is diagnostics only. The commit CAS is the single authority on
    /// who may publish (ADR 0029); a backend must never let a caller substitute
    /// this snapshot for the fence it presents on claim, renew, or release.
    async fn get_session_execution_lease(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionExecutionLeaseObservation, StoreError>;
}

/// Durable queued-work capability: ingress, ordered claiming, and claim leases
/// for non-input work (process wakes and session commands).
///
/// Claims granted here are completed atomically by
/// [`SessionCommitStore::commit_runtime_state`].
#[async_trait::async_trait]
pub trait QueuedWorkStore: Send + Sync {
    /// Acquire or resume the session's sole unfinished queued run under the
    /// current lane fence. Retry preserves identity and physical position.
    async fn begin_or_resume_queued_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        request: BeginQueuedRun,
    ) -> Result<QueuedRunAdmission, StoreError>;

    /// Select and claim the initial work, or reclaim exactly the recorded
    /// membership under the successor lane. Selection and claims are atomic.
    async fn select_queued_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        scope: &crate::ExecutionScope,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
        configuration: &crate::PersistedSessionConfig,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<SelectedQueuedRun, StoreError>;

    /// Pending-run discovery remains available after original members settle.
    async fn pending_queued_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<QueuedRunAdmission>, StoreError>;

    /// The admission recorded for the drain `scope`, pending or settled, or
    /// `None` when that drain never admitted a run (or forgot an unworked
    /// one).
    ///
    /// A read, never a claim: it takes no lane and admits nothing. A settled
    /// run is a drain end (ADR 0094, FIG-3419/3559), so this is how the
    /// parent-end recovery sweep tells a drain whose end is owed — `terminal`
    /// is recorded but the end receipt is not — from one that is merely
    /// interrupted and ends through its own retry (FIG-3563).
    async fn queued_run(
        &self,
        scope: &crate::ExecutionScope,
    ) -> Result<Option<QueuedRunAdmission>, StoreError>;

    /// Fenced disposition for an empty run or a failure before physical commit.
    async fn settle_queued_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        settlement: QueuedRunCommit,
    ) -> Result<QueuedRunAdmission, StoreError>;

    /// Persist a queued-work batch for later claiming.
    async fn enqueue_queued_work(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkBatch, StoreError> {
        self.enqueue_queued_work_with_outcome(batch)
            .await
            .map(crate::QueuedWorkEnqueueOutcome::into_batch)
    }

    /// Persist a queued-work batch and expose whether receiver idempotency
    /// absorbed it. The wake driver uses this for delivery evidence.
    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, StoreError>;

    /// Claim leading ready session-command work for `owner_id`.
    ///
    /// A command claim is returned only when the earliest ready claimable batch
    /// is classified as [`QueuedWorkClass::SessionCommand`]. Non-config
    /// commands remain one-batch claims; an adjacent `ApplyConfigPatch` prefix
    /// is claimed together so the runtime can commit it once while completing
    /// every accepted batch.
    /// Backends derive the class from queued payloads; no schema column is
    /// required.
    async fn claim_leading_ready_session_command(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>, StoreError>;

    /// Claim the next ready turn-work group for `owner_id`.
    ///
    /// A turn-work claim is returned only when the earliest ready claimable
    /// batch is classified as [`QueuedWorkClass::TurnWork`].
    /// Earlier ready session commands are not skipped and are never
    /// materialized as turn input.
    /// When the head belongs to an interrupted predecessor-generation claim,
    /// the successor reclaims exactly the rows carrying that durable claim id;
    /// later compatible rows wait for a subsequent claim.
    ///
    /// An attempt that acquires nothing must name why: an automatic drain has no
    /// batch ids to reason about afterwards, so the
    /// [`QueuedWorkClaimRefusal`] this returns is
    /// the only account of the empty drain a host ever gets.
    async fn claim_ready_queued_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::QueuedWorkClaimOutcome, StoreError>;

    /// Claim both ingress families admitted at an active-turn checkpoint.
    ///
    /// Backends must probe durable store state before opening a write
    /// transaction. When either family is pending, both claims are granted in
    /// one write transaction after validating the session-execution fence once.
    #[allow(clippy::too_many_arguments)]
    async fn claim_checkpoint_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<
        (
            Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>,
            Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>,
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
    /// Requested ids are interpreted in durable `enqueue_seq` order. A claim
    /// returns their maximal physically contiguous prefix that satisfies the
    /// ordinary key/boundary/budget law; an unrequested physical row is a
    /// barrier, and requested rows after it remain queued.
    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        batch_ids: &[crate::BatchId],
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::SelectedQueuedWorkClaimOutcome, StoreError>;

    /// Release a held queued-work claim without completing it.
    async fn abandon_queued_work_claim(
        &self,
        claim: &crate::WorkClaim<crate::runtime::QueuedWorkClaimData>,
    ) -> Result<(), StoreError>;

    /// Release multiple queued-work claims in one backend batch.
    async fn abandon_queued_work_claims(
        &self,
        claims: &[crate::WorkClaim<crate::runtime::QueuedWorkClaimData>],
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
    ///
    /// Cancelling a process-wake batch is a terminal transition of that wake:
    /// the session's redelivery fence rises to `max(floor, sequence)` in the
    /// same transaction as the removal, so a later redelivery of the same
    /// `(process, sequence)` is refused with
    /// [`StoreError::ProcessWakeSequenceRewound`] rather than re-admitted.
    async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, StoreError>;

    /// Whether `batch_id` has a durable completion marker written atomically
    /// with a session-command head commit. Cancellation removes the queued row
    /// without writing this marker, so an accepted batch that has vanished can
    /// be classified without mistaking cancellation for completion.
    async fn queued_work_batch_completed(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<bool, StoreError>;

    /// Project the earliest pending session-command and next-turn-input ordering
    /// keys without hydrating either payload family. The session-command side is
    /// the pending queued-work rows whose durable `work_kind` is `control`, so
    /// `cancel` rows — which preempt on their own path — enter neither side. Both
    /// sides apply the same live-claim filter as the corresponding list read.
    async fn pending_session_work_ordering(
        &self,
        session_id: &SessionId,
    ) -> Result<PendingSessionWorkOrdering, StoreError>;

    /// List all queued-work batches for a session, including batches held by a
    /// live claim.
    async fn list_queued_work(
        &self,
        session_id: &SessionId,
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
        session_id: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
}

/// Host-scheduled retention and garbage-collection capability over settled
/// state.
///
/// # Test-only hooks
///
/// No production store trait carries a `*_for_testing` member, and none ever
/// obligates an implementor to write one. Conformance and differential-test
/// probes (raw-row reads, fault injection, fixture seeding) live on
/// [`StoreTestSupport`], which exists only under
/// `cfg(any(test, feature = "testing"))`. The conformance suites take
/// [`ConformancePersistence`] (`RuntimePersistence + StoreTestSupport`) and
/// [`ConformanceSessionStoreFactory`] handles, so a backend opts in by
/// implementing the gated traits under the same gate it forwards to
/// `lash-core/testing` — the pattern `lash-s3-store` sets with its
/// `cfg`-gated `raw_blobs_for_testing`. A production build never writes,
/// names, or ships a testing method, and a build that enables
/// `lash-core/testing` without a backend's own `testing` feature still
/// compiles: the obligation lives only on the conformance entry points.
#[async_trait::async_trait]
pub trait StoreMaintenance: Send + Sync {
    /// Physically delete tombstoned graph-node rows and prune terminal
    /// pending-turn-input evidence rows for the bound session. See [`VacuumReport`].
    ///
    /// Vacuum never affects replay. Terminal pending-turn-input rows are
    /// admission evidence, not replay state: a replayed turn drives the drive
    /// set its first execution journaled (ADR 0069 §6) and never reads pending
    /// rows, and commit receipts and application history live in the turn-commit
    /// records vacuum does not touch. Pruning them is safe at any time.
    ///
    /// Vacuum is always scoped to the session bound to this store handle; it must
    /// never prune rows catalog-wide. So vacuum is not the only reclaim step: a
    /// node tombstoned *after* its owning session was deleted (unpinning a deleted
    /// leaf, fork ancestry retired at a child's delete, or a process prune
    /// retiring ancestry it does not own) is unreachable by any session-scoped
    /// vacuum. Both delete paths — session delete and process prune — therefore
    /// reclaim tombstoned rows owned by already-deleted sessions too, still never
    /// catalog-wide: live sessions' rows wait for their own vacuum.
    ///
    /// # Outcome
    ///
    /// Answers in the maintenance outcome contract ([`MaintenanceResult`]):
    /// `Ok` with non-zero counters is a sweep, `Ok` with zero counters is a
    /// witnessed nothing-to-do, and every stop — including
    /// [`StoreError::SessionNotBound`] on an unbound handle — rides in a
    /// [`MaintenanceFailure`] carrying the rows already reclaimed. A backend
    /// must never absorb its own error into a zero report.
    async fn vacuum(&self) -> MaintenanceResult<VacuumReport>;

    /// Delete blobs no longer reachable from any retained root.
    ///
    /// A read-only-tier auditor with a destructive repair arm (ADR 0067 §1):
    /// correctness never depends on it running. Same outcome contract as
    /// [`Self::vacuum`] — a backend that cannot enumerate its roots refuses
    /// with [`MaintenanceRefusal::UnwitnessedScope`] rather than reporting an
    /// empty sweep, and a backend that does not implement the lever at all
    /// fails with [`StoreError::UnsupportedStoreOperation`].
    async fn gc_unreachable(&self) -> MaintenanceResult<GcReport>;
}

/// The fleet-format generation a store's writers must emit (ADR 0106 §1,
/// FIG-3796).
///
/// Every durable writer on a store handle consults the recorded `F` through
/// [`FleetFormat::writer_version`] rather than stamping the build's own
/// constants: while a mixed-version fleet runs, `F` names the generation
/// every worker in the fleet can still read, and a build's newer format
/// knowledge stays unwritten until `finalize-upgrade` moves the row
/// (FIG-3800). A store bound to a session answers the `F` its backend
/// admitted at open; a store with no recorded row — an in-memory fake or a
/// pre-`F` store — answers [`FleetFormat::current`], the only generation such
/// a store could write.
pub trait FleetFormatStore: Send + Sync {
    /// The recorded fleet format this store's writers emit.
    fn fleet_format(&self) -> FleetFormat;
}

/// Exact settled-session persistence protocol required by the runtime.
///
/// `Arc<dyn RuntimePersistence>` is *the* runtime storage handle: one object
/// implementing every persistence capability segment —
/// [`SessionCommitStore`] (atomic graph/head commits, reads, metadata, and the
/// attachment write-ahead manifest), [`TurnInputStore`] (pending turn-input
/// lifecycle), [`QueuedWorkStore`] (queued-work ingress and claiming),
/// [`SessionExecutionLeaseStore`] (single-writer execution lane),
/// [`DriveEpochStore`] (the drive epoch a session drive's seal raises, FIG-3600),
/// [`RootStore`] (logical roots' terminal evidence and input bindings, FIG-3600
/// S7) and [`StoreMaintenance`] (vacuum/GC). The segments share one transactional
/// domain: claims granted by the input and queue segments settle atomically in
/// [`SessionCommitStore::commit_runtime_state`]. In-flight nondeterministic
/// work belongs to the active [`EffectHost`](crate::EffectHost), not to the
/// store contract.
///
/// Blanket-implemented for every type that implements all five segments;
/// backends implement the segment traits and never this trait directly.
///
/// This alias carries no test-only obligation in any configuration. The
/// conformance suites use the gated [`ConformancePersistence`] alias
/// (`RuntimePersistence + StoreTestSupport`) instead; see
/// [`StoreMaintenance`] for the norm.
pub trait RuntimePersistence:
    FleetFormatStore
    + SessionCommitStore
    + TurnInputStore
    + SessionExecutionLeaseStore
    + QueuedWorkStore
    + DriveEpochStore
    + RootStore
    + StoreMaintenance
{
}

impl<T> RuntimePersistence for T where
    T: FleetFormatStore
        + SessionCommitStore
        + TurnInputStore
        + SessionExecutionLeaseStore
        + QueuedWorkStore
        + DriveEpochStore
        + RootStore
        + StoreMaintenance
        + ?Sized
{
}

mod runtime_persistence_decorator;
pub use runtime_persistence_decorator::RuntimePersistenceDecorator;

#[cfg(test)]
mod tests;
