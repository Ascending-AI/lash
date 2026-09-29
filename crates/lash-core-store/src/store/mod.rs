//! The runtime's settled-session persistence contract and shared store types.
use crate::SessionId;
use crate::TurnId;
use crate::facade_support::SessionGraphFacadeOps;
pub mod attachment_manifest;
pub mod catalog;
mod checkpoint;
pub mod namespace;
pub use checkpoint::{
    CHECKPOINT_COMPONENT_ENCODING_VERSION, CheckpointComponentDescriptor,
    EXECUTION_STATE_CHECKPOINT_COMPONENT, HydratedCheckpointComponent, HydratedSessionCheckpoint,
    PLUGIN_STATE_CHECKPOINT_COMPONENT, SESSION_CHECKPOINT_SCHEMA_VERSION, SessionCheckpoint,
    TOOL_STATE_CHECKPOINT_COMPONENT, ensure_checkpoint_component_encoding_version,
    ensure_checkpoint_component_hash_agreement,
};
pub mod admission_plan;
pub mod commit_budget;
mod commit_identity;
mod error;
pub mod fencing;
#[cfg(test)]
mod fencing_tests;
mod fleet_format;
mod fork_plan;
pub mod generation_drain;
mod graph_commit;
pub mod history;
#[cfg(test)]
mod history_gate_tests;
pub mod ingress_obligation;
mod lease_timings;
mod maintenance;
pub mod obligation;
mod park;
pub mod pending_follow_on;
mod physical_turn;
mod preflight;
pub mod queued_work;
mod record_schema_version;
pub use physical_turn::PhysicalTurn;
mod control_intent;
mod drive_fence;
mod realization;
pub mod recovery_leader;
mod retention;
mod root;
pub mod runtime_commit;
mod runtime_commit_plan;
mod semantic_boundary;
mod session_config_views;
mod session_view;
pub use session_config_views::{
    execution_session_config_from_state, persisted_session_config_from_state,
    root_snapshot_config_from_state,
};
mod lease_owner;
pub mod session_delete;
mod state_version;
#[cfg(any(test, feature = "testing"))]
mod testing;
mod usage;
mod window_load;

use record_schema_version::record_schema_version;
pub use record_schema_version::{
    ensure_supported_record_schema_version, ensure_supported_schema_version,
};

pub use crate::session_graph::RealizedNodeTimestamp;
pub use crate::session_store_factory_types::SessionLookup;
pub use admission_plan::{
    IngressRowId, IngressSettlement, ROOT_ADMISSION_STEP, TerminalProcessWake, TurnLaneStop,
    deferred_wake_records, plan_turn_input_admission, require_admitted_to_root,
    require_open_command, turn_input_state_after_admission,
};
pub use attachment_manifest::{
    AttachmentCondemnation, AttachmentCondemnationPhase, AttachmentCondemnationProvenance,
    AttachmentCondemnationRecord, AttachmentDeleteArming, AttachmentIntent, AttachmentManifest,
    AttachmentManifestEntry, AttachmentOwner, AttachmentOwnerKind, AttachmentWriteFence,
    AttachmentWritePermit, AttachmentWriteToken, decode_attachment_condemnation_record,
    decode_attachment_owner,
};
pub use catalog::SessionCatalogStore;
pub use commit_budget::{CommitBudget, CommitBudgetLimit};
pub use commit_identity::{
    APPEND_REQUEST_IDENTITY_ENCODING_VERSION, OperationId, RuntimeCommitReceiptDecision,
    decide_runtime_commit_receipt, derive_history_node_id,
};
pub use control_intent::{
    CONTROL_INTENT_FORMAT, ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState,
    ControlIntentStore, IntentApplication, IntentSettle, RootIntentFacts, RootIntentPlan,
    RootIntentRefused, RootIntentRequest, RootVerb, decide_intent_acknowledgement,
    decide_intent_application, decide_intent_failure, decide_root_intent, forked_root,
    stored_intent_kind, stored_intent_state,
};
pub use drive_fence::{
    AdmissionId, DriveEpochSeal, DriveEpochSealDecision, DriveEpochStore, DriveFence,
    InMemoryDriveEpochs, RootStartNonce, SessionHeadRef, StoredDriveEpoch, close_admission,
    current_drive_fence, decide_drive_epoch_seal, require_current_drive_fence,
};
pub use error::{AnchorUnavailable, StoreError, WindowAnchorViolation};
pub use fencing::{
    FENCED_WRITE_DISAGREEMENT_EVENT, FENCING_TRACE_TARGET, FencedWrite, HeadPublicationVerdict,
    WakeDeliveryClaimFacts, WakeDeliveryClaimVerdict, fenced_write_applied,
    head_publication_verdict, require_fenced_write_applied, require_single_writer_head_publication,
    wake_delivery_claim_verdict,
};
pub use fleet_format::{
    FLEET_FORMAT_VERSION, FleetFormat, FleetFormatState, RECORD_UPCASTERS, ReadWindow,
    RecordUpcaster, SurfaceFormat, WriterPin, decode_versioned_json_record,
    decode_versioned_json_record_for_fleet, decode_versioned_msgpack_record_for_fleet,
    ensure_supported_record_schema_version_for_fleet, ensure_supported_schema_version_for_fleet,
    upcast_chain_covers, upcast_json_record,
};
pub use fork_plan::{ForkLineageAncestor, ForkNodeFacts, ForkPlan};
pub use history::{
    FailureEvidenceCursor, FailureEvidencePage, HistoryAnchor, HistoryBudget, HistoryCursor,
    HistoryNode, HistoryPage, HistoryStop, LineageStamp, SessionHistoryStore, SessionWindowRead,
    UsageLedgerCursor, UsageLedgerPage, UsageLedgerRow, WindowSelector,
};
pub use lease_owner::LeaseOwnerIdentity;
pub use lease_timings::{LeaseTimings, LeaseTimingsError};
pub use maintenance::{
    GcReport, MaintenanceFailure, MaintenanceRefusal, MaintenanceReport, MaintenanceResult,
    MaintenanceStop, MaintenanceSweep, SessionBlobReclaimReport, VacuumReport,
};
pub use obligation::*;
pub use park::{
    EnginePark, ParkCancelCause, ParkEventKind, ParkFeedCursor, ParkFeedEvent, ParkFeedPage,
    ParkId, ParkReason, ParkReasonCode, ParkSummary, ProcessPark, ProcessParkKey, ProcessParkQuery,
    ProcessParkWrite, StoredParkRedrive, StoredTurnParkHead, TurnPark, TurnParkQuery,
    TurnParkTarget, TurnParkWrite, TurnParkWriteDecision, UnparkCause, UnsettledTurnCounts,
    decide_turn_park_write,
};
pub use pending_follow_on::{
    DEFAULT_MAX_FOLLOW_ON_RECOVERIES, FollowOnAdmission, FollowOnBlocked, FollowOnRecovery,
    PendingFollowOn, follow_on_blocks_admission, validate_follow_on_head_write,
};
pub use preflight::{
    DurableItem, DurablePayload, DurableScan, DurableScanPage, DurableSurface, ScanCoverage,
    StoreBackend, StoreComponentVersion, StorePreflight, StoreReleaseStamp, StoreReleaseState,
    StoreSchemaDatabase, StoreSchemaOutcome, StoreSchemaStatus, StoreSchemaVerdict,
    compare_releases, release_stamp_advances,
};
pub use queued_work::{
    AdmissionRefusal, PendingSessionWorkOrdering, PendingWorkOrderingKey, QueuedWorkClass,
    TurnWorkPrefix, TurnWorkSelection,
};
pub use realization::commit_runtime_state_verified;
pub use recovery_leader::*;
pub use retention::{
    FACADE_PLUGIN_COMMAND_OPERATION_TAG, FACADE_PLUGIN_TASK_OPERATION_TAG, FacadePluginOperation,
    PLUGIN_OPERATION_STATE_RECEIPT_KEY, RetentionBound, RetentionReport,
    is_facade_minted_operation_id, mint_facade_operation_id, plugin_operation_receipt_storage_key,
};
pub use root::{
    AdmitRootRequest, AdmittedHead, CheckpointAdmission, CheckpointAdmissionRequest,
    InMemoryRootLedger, RootAdmission, RootAdmissionAnswer, RootAdmissionRefusal, RootEndedTurns,
    RootStore, RootTerminal, RootTerminalCause, RootTerminalKind, RootTerminalWrite,
    RootTerminalWriteDecision, StoredRootTerminal, TurnCommitId, UnfinishedRoot,
    decide_root_terminal_write, root_binding_conflict,
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

pub use session_view::{CarriesSession, SessionStore};
pub use state_version::{
    CURRENT_SESSION_STATE_VERSION, OLDEST_SUPPORTED_SESSION_STATE_VERSION, SessionStateAdmission,
    resolve_session_state_version,
};
#[cfg(any(test, feature = "testing"))]
pub use testing::{
    ConformanceStore, DecodedRowCounts, GraphRowCorruption, StoreTestSupport,
    append_request_commit_with_clock_for_testing,
};
pub use usage::merge_token_ledger_entry_checked;
pub use window_load::{
    LoadedSessionWindow, load_session_read_view, load_session_window_state, refresh_session_window,
    window_state,
};

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
    /// The process that runs this session as its own, recorded at creation
    /// (FIG-3607 R1): see [`SessionStoreCreateRequest::owning_process_id`](crate::SessionStoreCreateRequest::owning_process_id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owning_process_id: Option<crate::ProcessId>,
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
            drive_fence: _,
            root_terminal,
            park_root,
            config: _,
            execution_config: _,
            current_frame_node_id: _,
            graph: _,
            graph_base_leaf_node_id: _,
            checkpoint: _,
            usage_deltas: _,
            failure_evidence,
            turn_commit: _,
            ingress,
            applied_commands,
            // Carried unchanged from the head; the store refuses a change.
            pending_follow_on: _,
            interrupted_turn_input_turn_id,
            interrupted_turn_input_cancellation,
            interrupted_turn_cancel_intent,
            turn_cancel_closure_settlement,
            adopted_intent_rows,
            committed_attachment_ids,
        } = self;
        debug_assert!(
            ingress.is_none()
                && applied_commands.is_none()
                && interrupted_turn_input_turn_id.is_none()
                && interrupted_turn_input_cancellation.is_none()
                && interrupted_turn_cancel_intent.is_none()
                && turn_cancel_closure_settlement.is_none()
                && *adopted_intent_rows == 0
                && failure_evidence.is_empty()
                && committed_attachment_ids.is_empty()
                && root_terminal.is_none()
                && park_root.is_none(),
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

    /// Flattens application evidence from the completed turn inputs of the
    /// commit's ingress settlement, in completion order, for store
    /// implementors returning commit results.
    pub fn turn_input_applications(&self) -> Vec<crate::TurnInputApplication> {
        self.ingress
            .as_ref()
            .map(IngressSettlement::turn_input_applications)
            .unwrap_or_default()
    }

    /// Refuse a commit that settles ingress rows or applies commands
    /// without presenting a drive fence, or names one row twice, before any
    /// backend reads a row (FIG-3927).
    pub fn validate_ingress_settlement(&self) -> Result<(), StoreError> {
        let settles_rows = self
            .ingress
            .as_ref()
            .is_some_and(|ingress| !ingress.is_empty())
            || self
                .applied_commands
                .as_ref()
                .is_some_and(|commands| !commands.batch_ids.is_empty());
        if settles_rows && self.drive_fence.is_none() {
            return Err(StoreError::IngressSettlementUnfenced {
                session_id: self.session_id.clone(),
            });
        }
        if let Some(ingress) = self.ingress.as_ref() {
            ingress.validate(&self.session_id)?;
        }
        Ok(())
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
        let config = persisted_session_config_from_state(state);
        let execution_config = state
            .authority
            .committed_config
            .is_some()
            .then(|| execution_session_config_from_state(state))
            .filter(|execution| *execution != config)
            .map(Box::new);
        Ok(Self {
            commit_budget,
            session_id: state.session_id.clone(),
            expected_head_revision: state.head_revision,
            drive_fence: None,
            root_terminal: None,
            park_root: None,
            config,
            execution_config,
            current_frame_node_id,
            graph,
            graph_base_leaf_node_id: state.session_graph.leaf_node_id.clone(),
            checkpoint: build_checkpoint_from_persisted_state(state, fleet_format)?,
            usage_deltas: usage_deltas.to_vec(),
            failure_evidence: Vec::new(),
            turn_commit: RuntimeTurnCommitStamp::new(operation),
            ingress: None,
            applied_commands: None,
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

    /// Present `fence`: the drive fence the commit's root was sealed under.
    pub fn fenced_by(mut self, fence: DriveFence) -> Self {
        self.drive_fence = Some(Box::new(fence));
        self
    }

    /// Settle `ingress` atomically with the runtime commit.
    pub fn settling_ingress(mut self, ingress: IngressSettlement) -> Self {
        self.ingress = Some(ingress);
        self
    }

    /// Settle the session-command batches `commands` applied, atomically
    /// with the runtime commit.
    pub fn applying_commands(mut self, commands: crate::QueuedWorkCompletion) -> Self {
        self.applied_commands = Some(commands);
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

/// Adopt an empty-window head onto a default state, for the head-adoption
/// tests.
#[cfg(test)]
fn persisted_session_state_from_head(
    session_id: SessionId,
    head_revision: u64,
    config: crate::PersistedSessionConfig,
    checkpoint: Option<HydratedSessionCheckpoint>,
) -> Result<crate::RuntimeSessionState, StoreError> {
    let read = SessionWindowRead::new(
        session_id,
        head_revision,
        config,
        None,
        crate::SessionGraph::default(),
        None,
        checkpoint,
        crate::SessionUsageTotals::default(),
    )?;
    window_state(read, FleetFormat::current()).map(|loaded| loaded.state)
}

/// Refuse a window read that names another session than the one asked for.
fn validate_window_session(
    session_id: &SessionId,
    read: &SessionWindowRead,
) -> Result<(), StoreError> {
    if read.session_id == *session_id {
        Ok(())
    } else {
        Err(StoreError::StoredDataCorrupt {
            record_kind: "SessionWindowRead",
            message: format!(
                "a window read for session `{session_id}` names session `{}`",
                read.session_id
            ),
        })
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

/// Settled-session commit capability: the runtime's atomic transaction
/// facade for visible session state.
///
/// This segment owns session head commits, checkpoint hydration and usage,
/// final turn-commit idempotency, session metadata and turn parks.
/// The rows a root admitted also settle here —
/// [`commit_runtime_state`](Self::commit_runtime_state) completes, releases
/// or drops them by the commit's [`IngressSettlement`] in the same atomic
/// commit (FIG-3927). History reads are [`SessionHistoryStore`]'s. In-flight
/// nondeterministic work belongs to the active
/// [`EffectHost`](crate::EffectHost), not to the store contract.
///
/// Every operation names its session: it takes `session_id` first, or a
/// request that carries it (ADR 0112 §1).
///
/// Checkpoint components have one backend-independent durable shape. When a
/// commit supplies a tool-state, plugin-state, or execution-state body, the
/// backend must store it under a content ref and return that ref in
/// [`RuntimeCommitReceipt::manifest`]. A later commit may carry the ref without
/// the body to mean "unchanged"; the backend must resolve the existing body
/// when hydrating the checkpoint. A ref-only commit whose component is absent
/// must fail instead of persisting a checkpoint that hydrates to `None`.
#[async_trait::async_trait]
pub trait SessionCommitStore: Send + Sync {
    /// The session's physical session-state generation marker. A legacy
    /// absent marker reads as [`OLDEST_SUPPORTED_SESSION_STATE_VERSION`].
    async fn read_session_state_version(&self, session_id: &SessionId) -> Result<u32, StoreError>;

    /// Revalidate `fence`, then classify the independently read session-state
    /// marker of `fence.session()`.
    async fn admit_session_state(
        &self,
        fence: &DriveFence,
    ) -> Result<SessionStateAdmission, StoreError> {
        let version = self.read_session_state_version(fence.session()).await?;
        Ok(SessionStateAdmission {
            session_id: fence.session().clone(),
            version,
            drive_epoch: fence.epoch(),
        })
    }

    /// Read the session's current head without hydrating graph, checkpoint,
    /// or usage history.
    ///
    /// Implementations must project this from at most one durable row. Runtime
    /// freshness checks depend on the revision, leaf, and checkpoint reference
    /// all being present in this read. `Ok(None)` means the session has no
    /// head row; inability to determine the head must be returned as `Err`,
    /// never collapsed to absence.
    async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionHeadMeta>, StoreError>;

    /// Keep `base` readable by
    /// [`load_session_window(Admitted(base))`](SessionHistoryStore::load_session_window)
    /// until the session's next admission replaces it (FIG-3682).
    ///
    /// Called by a turn's admission under the session's drive fence, once per
    /// first execution. While it stands, maintenance that reclaims
    /// unreferenced checkpoints treats `base.checkpoint` as a root, so a
    /// replay of the admitted turn can rebuild its input state even after the
    /// turn's own commit superseded the head and a vacuum ran. A backend that
    /// never reclaims a superseded checkpoint answers `Ok(())` explicitly.
    async fn retain_admission_base(
        &self,
        fence: &DriveFence,
        base: &SessionHeadRef,
    ) -> Result<(), StoreError>;

    /// Does the session hold a durable commit receipt for `turn_id`?
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
    async fn committed_turn_exists(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<bool, StoreError>;

    /// Does the session hold a durable end receipt for `drain_id`?
    ///
    /// The same membership read as [`committed_turn_exists`](Self::committed_turn_exists),
    /// keyed on the drain's `final` receipt: true means the drain's epilogue
    /// committed, false means it did not (yet). The parent-end recovery sweep
    /// is its only caller; a drain interrupted before its epilogue is left
    /// alone for the retried drain under the same `drain_id` to end.
    async fn drain_end_exists(
        &self,
        session_id: &SessionId,
        drain_id: &str,
    ) -> Result<bool, StoreError>;

    /// Atomically persist one settled runtime commit and its durable receipt
    /// for `commit.session_id`.
    ///
    /// A commit carrying [`RuntimeCommit::drive_fence`] is refused
    /// [`StoreError::StaleDriveFence`] unless the fence is still the session's
    /// current one. Implementors must validate it inside the write
    /// transaction and before receipt lookup, so superseded authority vetoes
    /// even an otherwise replayable operation identity.
    ///
    /// Implementors must look up the `(session_id, operation storage key)`
    /// receipt inside the write transaction before the fresh append ancestor
    /// fence and head-revision compare-and-swap. Existing receipts must be
    /// adjudicated with [`decide_runtime_commit_receipt`]: replay returns the
    /// stored first-attempt [`RuntimeCommitReceipt`] with only
    /// [`RuntimeCommitReceipt::receipt_replayed`] set transiently, applies none
    /// of the attempted commit envelope. Conflicts and corrupt count
    /// cross-checks mutate nothing.
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
    /// Every row the commit's [`IngressSettlement`] names must still be bound
    /// to its root, or the commit is refused whole
    /// [`StoreError::IngressRowNotAdmitted`]; every applied command must still
    /// exist and be open, or it is refused
    /// [`StoreError::SessionCommandWithdrawn`]. A commit that writes its
    /// root's terminal releases, in the same transaction, every row still
    /// bound to the root (FIG-3927).
    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError>;

    /// The follow-on the session head owes, if any (ADR 0101 §3): the head's
    /// `pending_follow_on` as it is committed now.
    ///
    /// Drive admission asks this to decide whether the follow-on is the next
    /// work it admits. It reads one head fact and is not a freshness probe of
    /// the resident head. Provided: it composes
    /// [`load_session_head_meta`](Self::load_session_head_meta).
    async fn load_pending_follow_on(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PendingFollowOn>, StoreError> {
        Ok(self
            .load_session_head_meta(session_id)
            .await?
            .and_then(|head| head.pending_follow_on))
    }

    /// Raise the head's pending follow-on recovery count by one (ADR 0101 §3).
    ///
    /// A drive that recovers a pending follow-on calls this before the
    /// follow-on's first effect. Implementations must, in one transaction,
    /// validate `fence` against the session's current drive fence, refuse with
    /// [`StoreError::FollowOnNotPending`] unless the head's
    /// `pending_follow_on_json` names `follow_on_turn_id`, and write the fact
    /// back with `attempts` raised by one. The head revision does not move:
    /// the raise changes no other head fact, and nothing ever lowers the count.
    /// Returns the raised fact.
    async fn raise_pending_follow_on_attempts(
        &self,
        fence: &DriveFence,
        follow_on_turn_id: &TurnId,
    ) -> Result<PendingFollowOn, StoreError>;

    /// Write `meta.session_id`'s metadata, creating the row when it is absent.
    ///
    /// The recorded lineage is write-once. `meta.relation` must declare the
    /// same lineage the existing row records — the same parent, or the same
    /// fork source and node — or the write is refused with
    /// [`StoreError::SessionRelationMismatch`] and the row is left unchanged.
    /// Admission reads [`SessionRelation::Root`](crate::SessionRelation::Root)
    /// as "no claim" on a rebind because a resume declares no lineage; a write
    /// cannot, because the row it would record replaces the recorded parent
    /// with that root. Causal provenance and the pending observer intents are
    /// not lineage and are replaced as given, which is what lets the observer
    /// intent settlement round-trip the metadata it loaded. Use
    /// [`store_backend_support::guard_session_meta_relation_rewrite`](crate::store_backend_support::guard_session_meta_relation_rewrite)
    /// so all backends answer identically.
    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError>;

    /// The session's metadata, if the catalog holds it.
    async fn load_session_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError>;

    /// Commit preflight distinguishes a retired session from a session that
    /// was never materialized. SQL stores check their deletion tombstone here;
    /// the commit transaction repeats that check before any write.
    async fn load_session_meta_for_commit(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError>;

    /// Record that `park.session_id`'s turn parked (FIG-3586, FIG-3600,
    /// FIG-3659).
    ///
    /// Written on the abort path of a turn whose refusal parks it, before its
    /// lease is released. A first park allocates the feed sequence the record's
    /// `park_id` names and appends a `Parked` event; a re-park of the same
    /// turn keeps `park_id` and `since_ms`, bumps `attempts` and
    /// `last_refused_ms`, and writes no event; a different turn's park
    /// supersedes the stored one (`Unparked{Superseded}` then `Parked`).
    /// Any commit of the session clears the park in the commit's transaction,
    /// as does a cancel that ends the parked root and the session's
    /// deletion: a park is live exactly while its turn is.
    ///
    /// Returns the record as stored, so the caller can report the allocated
    /// `park_id` and attempt count.
    async fn record_turn_park(&self, park: &TurnParkWrite) -> Result<TurnPark, StoreError>;

    /// The session's parked turn, if its turn is parked.
    async fn load_turn_park(&self, session_id: &SessionId) -> Result<Option<TurnPark>, StoreError>;
}

/// What [`TurnInputStore::admit_pending_turn_inputs`] committed (FIG-3975).
///
/// The admission's own commit can answer the follow-ups the caller owes
/// next — the session state-version check, the claim of each admitted row's
/// still-due ingress obligation for the producer's immediate ask, and the
/// committed head the queue event publishes against — so a backend that can
/// fold them returns [`TurnInputAdmission::Fused`] instead of leaving the
/// caller three more store round-trips.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum TurnInputAdmission {
    /// The admission transaction did it all: `ingress_claims` holds — in
    /// request order — the claim of each admitted row's still-due ingress
    /// obligation, taken under the caller's TTL for the producer's immediate
    /// ask, and `committed_head` is the session head the same transaction
    /// read (`None` before the session's first checkpoint).
    Fused {
        /// The admitted rows, in request order.
        rows: Vec<crate::PendingTurnInput>,
        /// The claims the transaction took for the rows whose ingress
        /// obligation was still due, in row order. A replayed row whose
        /// obligation is claimed or settled contributes no claim, so
        /// `ingress_claims` may be shorter than `rows`.
        ingress_claims: Vec<ClaimedObligation>,
        /// The session head the committed transaction read.
        committed_head: Option<SessionHeadMeta>,
    },
    /// Only the rows were enqueued: the caller owes each admitted row's
    /// ingress claim through the relay and the head read itself.
    Enqueued(Vec<crate::PendingTurnInput>),
}

/// Durable model-visible user input (ADR 0101, `pending_turn_inputs`), its
/// lifecycle reads, and the turn-cancellation records that address it.
///
/// Rows enter here and wait open. A root binds the rows it drives
/// ([`RootStore::admit_root`], [`RootStore::admit_at_checkpoint`]), and only
/// that root's commit or terminal settles or releases them again
/// ([`SessionCommitStore::commit_runtime_state`], FIG-3927). User input must
/// not be represented as generic queued work ([`QueuedWorkStore`]).
#[async_trait::async_trait]
pub trait TurnInputStore: Send + Sync {
    /// Persist or validate the one cancellation authority selected for this
    /// session and, for a Process or runtime-operation controller, its physical
    /// journal scope. Session-bound turns keep their exact canonical address in
    /// each closure authorization, so distinct turns may share this authority.
    /// The check occurs under the current drive fence before any session work
    /// and never replaces the original selection.
    async fn validate_turn_cancellation_binding(
        &self,
        session_id: &SessionId,
        fence: &DriveFence,
        binding_id: &str,
        admitted_scope: &crate::ExecutionScope,
    ) -> Result<(), StoreError>;

    /// Authorize exact closure of one cancellation gate pair for the admitted
    /// session and binding. The drive fence authenticates the proposal, but
    /// its epoch does not fence final settlement.
    /// A vacant slot accepts this value, an identical retry adopts it, and a
    /// different occupied value or retired physical scope returns a typed refusal.
    /// Final publication additionally requires the session-head CAS.
    async fn authorize_turn_cancel_closure(
        &self,
        fence: &DriveFence,
        authorization: &crate::TurnCancelClosureAuthorization,
    ) -> Result<crate::TurnCancelClosureAuthorizationOutcome, StoreError>;

    /// Load every unconsumed closure obligation for the bound session after
    /// validating the current drive fence, selected binding, and any
    /// original non-session physical scope.
    async fn pending_turn_cancel_closures(
        &self,
        session_id: &SessionId,
        fence: &DriveFence,
        binding_id: &str,
        admitted_scope: &crate::ExecutionScope,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;

    /// Read `session_id`'s unconsumed closure pins for lifecycle
    /// coordination without presenting a drive fence. This grants no right to
    /// settle or consume them; deletion and scope-retirement owners use it
    /// only to refuse destructive cleanup until an activation holder has
    /// drained the pins. A store that cannot answer fails closed: callers
    /// never infer an empty set.
    async fn pending_turn_cancel_closure_pins(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;

    /// Whether this turn's final runtime commit receipt is already durable.
    /// This closes the store-commit-to-terminal-publication window for late
    /// cancellation requests.
    async fn turn_is_committed(&self, address: &crate::TurnAddress) -> Result<bool, StoreError>;

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
        request: crate::TurnCancelRequest,
    ) -> Result<crate::TurnCancelRequestRecord, StoreError>;

    /// Read the durable cancellation request and any accumulated repair
    /// outcome for one turn.
    async fn turn_cancel_request(
        &self,
        address: &crate::TurnAddress,
    ) -> Result<Option<crate::TurnCancelRequestRecord>, StoreError>;

    /// Read only durable cancellation intent, without reconstructing affected
    /// input payloads. Recovery uses this after vacuum may have reclaimed
    /// payload tombstones belonging to an earlier repair of the same turn id.
    async fn turn_cancel_request_intent(
        &self,
        address: &crate::TurnAddress,
    ) -> Result<crate::TurnCancelIntentSnapshot, StoreError>;

    /// Project the authoritative keyed-gate winner into durable request
    /// evidence without changing arbitration authority.
    async fn reconcile_turn_cancel_winner(
        &self,
        address: &crate::TurnAddress,
        observed: &crate::TurnCancelIntentSnapshot,
        evidence: &crate::TurnCancellationEvidence,
    ) -> Result<bool, StoreError>;

    /// Persist model-visible user input into the pending turn-input
    /// lifecycle: every draft of `batch`, in one transaction, answered in
    /// request order (FIG-3842).
    ///
    /// A draft filed under a source key the session already holds is the same
    /// submission when its digest equals the stored row's, and returns that
    /// row whatever became of it; any other content is
    /// [`StoreError::PendingTurnInputSourceKeyConflict`]. A draft that carries
    /// its own `input_id` names one admission the same way: an identical
    /// submission returns the row and anything else is
    /// [`StoreError::PendingTurnInputIdConflict`]. A journaled turn acceptance
    /// provisions its id this way, so re-running its body never admits a
    /// second row (ADR 0069 §6).
    ///
    /// Every other draft is enqueued, in request order, at consecutive
    /// positions of the session's ingress sequence: the store holds the
    /// session's write authority for the whole transaction, so no other
    /// producer's item lands inside the block.
    ///
    /// A draft with a non-default [`RunSpec`](crate::run_spec::RunSpec) interns
    /// the spec in the session's spec table in the same transaction, once per
    /// hash; different bytes under an interned hash are refused as
    /// [`StoreError::RunSpecHashCollision`]. An input addressed to a running
    /// turn whose explicit spec differs from that turn's is refused as
    /// [`StoreError::PendingTurnInputRunSpecMismatch`] (FIG-3838).
    ///
    /// Any refusal refuses the whole batch: nothing is stored, spec rows
    /// included.
    async fn enqueue_pending_turn_inputs(
        &self,
        batch: crate::PendingTurnInputBatch,
    ) -> Result<Vec<crate::PendingTurnInput>, StoreError>;

    /// Persist one draft: exactly
    /// [`enqueue_pending_turn_inputs`](Self::enqueue_pending_turn_inputs) of
    /// a batch of one, so backends implement only the batch primitive.
    async fn enqueue_pending_turn_input(
        &self,
        input: crate::PendingTurnInputDraft,
    ) -> Result<crate::PendingTurnInput, StoreError> {
        let batch = crate::PendingTurnInputBatch::one(input);
        crate::PendingTurnInputBatch::only(self.enqueue_pending_turn_inputs(batch).await?)
    }

    /// Admit `batch` as the producer's whole store-side round (FIG-3975):
    /// the session state-version gate plus
    /// [`enqueue_pending_turn_inputs`](Self::enqueue_pending_turn_inputs),
    /// with whatever else the admission's own commit can answer riding the
    /// same write transaction ([`TurnInputAdmission`]).
    ///
    /// An implementation that folds takes each admitted row's still-due
    /// ingress-obligation claim inside the transaction — under
    /// `ingress_claim_ttl_ms`, the relay's claim TTL, so the claim outlives
    /// the send it precedes — and reads the committed head before it
    /// commits. One that does not fold still runs the version check before
    /// the enqueue and answers
    /// [`TurnInputAdmission::Enqueued`], leaving the caller to claim through
    /// the relay and read the head itself.
    async fn admit_pending_turn_inputs(
        &self,
        batch: crate::PendingTurnInputBatch,
        ingress_claim_ttl_ms: u64,
    ) -> Result<TurnInputAdmission, StoreError>;

    /// The run spec `session_id` interned under `hash`, if it holds one
    /// (FIG-3838). Spec rows are immutable and live until their session is
    /// deleted.
    async fn load_run_spec(
        &self,
        session_id: &SessionId,
        hash: &crate::run_spec::RunSpecHash,
    ) -> Result<Option<crate::run_spec::RunSpec>, StoreError>;

    /// List undelivered user inputs for reconciliation or queue preview.
    ///
    /// Completed and cancelled rows are excluded. A row a root admitted is
    /// returned as
    /// [`PendingTurnInputReadStatus::Admitted`](crate::PendingTurnInputReadStatus::Admitted)
    /// naming that root, and every other row as
    /// [`Open`](crate::PendingTurnInputReadStatus::Open). An admitted row is
    /// answered by its root alone; resubmitting the same input under the same
    /// source key returns the row.
    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::PendingTurnInputRead>, StoreError>;

    /// Read canonical input applications from durable turn-commit records.
    ///
    /// Unlike live observation replay, this surface is not retention-window
    /// dependent. Implementations return settled applications in durable
    /// commit order so a host can reconcile admission identity after a gap.
    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::TurnInputApplication>, StoreError>;

    /// Cancel an open pending user input by id. An admitted input answers
    /// [`PendingTurnInputCancelOutcome::AlreadyAdmitted`](crate::PendingTurnInputCancelOutcome::AlreadyAdmitted)
    /// and changes nothing: the host cancels its root instead.
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
}

/// Durable queued work (ADR 0101, `queued_work_batches`): process wakes and
/// session commands, with their lifecycle reads.
///
/// Batches enter here and wait open. A root admits turn work
/// ([`RootStore::admit_root`], [`RootStore::admit_at_checkpoint`]), the
/// command lane applies session commands
/// ([`open_session_command_run`](Self::open_session_command_run)), and only
/// the admitting root's or the applying commit settles them
/// ([`SessionCommitStore::commit_runtime_state`], FIG-3927).
#[async_trait::async_trait]
pub trait QueuedWorkStore: Send + Sync {
    /// Persist a queued-work batch for later admission.
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

    /// The session's leading open session-command run, for the command lane
    /// to apply (ADR 0101 §4, design §2.7). Takes no admission.
    ///
    /// The run is returned only when the earliest open batch is classified
    /// as [`QueuedWorkClass::SessionCommand`]. A non-config command is a run
    /// of one; an adjacent `ApplyConfigPatch` prefix is returned together (up
    /// to [`MAX_SESSION_COMMAND_BATCHES_PER_RUN`](crate::store::queued_work::MAX_SESSION_COMMAND_BATCHES_PER_RUN))
    /// so the lane applies it in one commit. In one transaction fenced by
    /// `fence`, the run's ingress obligations are acknowledged delivered
    /// (ADR 0109 §3). The applying commit settles the rows
    /// ([`RuntimeCommit::applied_commands`]); a row withdrawn in between
    /// refuses that commit.
    async fn open_session_command_run(
        &self,
        fence: &DriveFence,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;

    /// Remove an open queued-work batch from durable ingress.
    ///
    /// Returns the removed batch when cancellation won the race. Returns `None`
    /// when the batch is missing or a root admitted it; callers must treat
    /// that as "already admitted or completed" and must not restore any stale
    /// local draft state.
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

    /// Project the earliest open session-command and next-turn-input ordering
    /// keys without hydrating either payload family. The session-command side
    /// is the open queued-work rows whose durable `work_kind` is `control`, so
    /// `cancel` rows — which preempt on their own path — enter neither side.
    /// Both sides exclude admitted rows, as the corresponding list read does.
    async fn pending_session_work_ordering(
        &self,
        session_id: &SessionId,
    ) -> Result<PendingSessionWorkOrdering, StoreError>;

    /// List all queued-work batches for a session, admitted ones included.
    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;

    /// List the queued-work batches no root admitted: still open for
    /// presentation, editing or cancellation.
    ///
    /// This is a distinct required query, not a derivation of
    /// [`list_queued_work`](Self::list_queued_work): backends answer each
    /// with its own query rather than leaking admission state to callers for
    /// client-side filtering.
    async fn list_open_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;

    /// Cheap durable read used to reject an idle queued-work notification
    /// before session state, plugins, and a runtime are hydrated: `true` when
    /// the session has an open queued batch or a deferred next-turn input.
    /// It never creates a session; a session the catalog does not hold
    /// answers `false`.
    async fn has_claimable_queued_work(&self, session_id: &SessionId) -> Result<bool, StoreError>;
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
/// [`ConformanceStore`] (`RuntimeStore + StoreTestSupport`) and
/// `ConformanceDeployment` handles, so a backend opts in by
/// implementing the gated traits under the same gate it forwards to
/// `lash-core/testing` — the pattern `lash-s3-store` sets with its
/// `cfg`-gated `raw_blobs_for_testing`. A production build never writes,
/// names, or ships a testing method, and a build that enables
/// `lash-core/testing` without a backend's own `testing` feature still
/// compiles: the obligation lives only on the conformance entry points.
#[async_trait::async_trait]
pub trait StoreMaintenance: Send + Sync {
    /// Physically delete tombstoned graph-node rows and prune terminal
    /// pending-turn-input evidence rows of `session_id`. See [`VacuumReport`].
    ///
    /// Vacuum never affects replay. Terminal pending-turn-input rows are
    /// admission evidence, not replay state: a replayed turn drives the drive
    /// set its first execution journaled (ADR 0069 §6) and never reads pending
    /// rows, and commit receipts and application history live in the turn-commit
    /// records vacuum does not touch. Pruning them is safe at any time.
    ///
    /// Vacuum is always scoped to `session_id`, including tombstoned rows of
    /// already-deleted sessions that this session's deletes retired; it must
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
    /// witnessed nothing-to-do, and every stop rides in a
    /// [`MaintenanceFailure`] carrying the rows already reclaimed. A backend
    /// must never absorb its own error into a zero report.
    async fn vacuum(&self, session_id: &SessionId) -> MaintenanceResult<VacuumReport>;

    /// Catalog-wide by definition: delete blobs no retained root reaches.
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

/// The runtime's store: one object per catalog, keyed by session (ADR 0112
/// §1).
///
/// `Arc<dyn RuntimeStore>` implements every store segment —
/// [`AttachmentManifest`] (the attachment write-ahead manifest),
/// [`SessionCatalogStore`] (admission, lookup, enumeration, forks and
/// deletion), [`SessionCommitStore`] (atomic head commits, metadata and
/// parks), [`SessionHistoryStore`] (frame windows and paged history),
/// [`TurnInputStore`] (pending turn-input lifecycle), [`QueuedWorkStore`]
/// (queued-work ingress and claiming), [`DriveEpochStore`] (the drive epoch a
/// session drive's seal raises, FIG-3600), [`RootStore`] (logical roots'
/// terminal evidence and input bindings) and [`StoreMaintenance`]
/// (vacuum/GC). The segments share one transactional domain: claims granted
/// by the input and queue segments settle atomically in
/// [`SessionCommitStore::commit_runtime_state`]. In-flight nondeterministic
/// work belongs to the active [`EffectHost`](crate::EffectHost), not to the
/// store contract.
///
/// Every session-scoped operation names its session, as its first parameter
/// or in a request that carries it. Runtime code reaches one session through
/// a [`SessionStore`] view.
///
/// Blanket-implemented for every type that implements all ten segments;
/// backends implement the segment traits and never this trait directly.
///
/// This alias carries no test-only obligation in any configuration. The
/// conformance suites use the gated [`ConformanceStore`] alias
/// (`RuntimeStore + StoreTestSupport`) instead; see [`StoreMaintenance`] for
/// the norm.
pub trait RuntimeStore:
    FleetFormatStore
    + AttachmentManifest
    + SessionCatalogStore
    + SessionCommitStore
    + SessionHistoryStore
    + TurnInputStore
    + QueuedWorkStore
    + DriveEpochStore
    + RootStore
    + StoreMaintenance
{
}

impl<T> RuntimeStore for T where
    T: FleetFormatStore
        + AttachmentManifest
        + SessionCatalogStore
        + SessionCommitStore
        + SessionHistoryStore
        + TurnInputStore
        + QueuedWorkStore
        + DriveEpochStore
        + RootStore
        + StoreMaintenance
        + ?Sized
{
}

mod runtime_store_decorator;
pub use runtime_store_decorator::RuntimeStoreDecorator;

#[cfg(test)]
mod tests;
