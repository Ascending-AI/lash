//! The runtime's settled-session persistence contract and shared store types.
/// version_surface = "coexist"
/// version_guard(items(LASH_BLOB_DOMAIN_VERSION), items(path = "crates/lash-core-store/src/store/session_head.rs", for_content))
const LASH_BLOB_DOMAIN_VERSION: &str = "lash-blob/v2";

use crate::SessionId;
use crate::TurnId;
use crate::facade_support::SessionGraphFacadeOps;
pub mod artifact_cleanup;
pub mod attachment_referrers;
pub mod catalog;
mod checkpoint;
pub mod namespace;
pub use checkpoint::{
    CHECKPOINT_COMPONENT_ENCODING_VERSION, CheckpointComponentDescriptor,
    EXECUTION_STATE_CHECKPOINT_COMPONENT, HydratedCheckpointComponent, HydratedSessionCheckpoint,
    PLUGIN_ADMISSION_CHECKPOINT_COMPONENT, PLUGIN_STATE_CHECKPOINT_COMPONENT,
    SESSION_CHECKPOINT_SCHEMA_VERSION, SessionCheckpoint, TOOL_STATE_CHECKPOINT_COMPONENT,
    ensure_checkpoint_component_encoding_version, ensure_checkpoint_component_hash_agreement,
};
pub mod admission_plan;
pub mod commit_budget;
mod commit_identity;
mod enumeration;
mod error;
#[cfg(test)]
mod error_class_tests;
#[cfg(any(test, feature = "testing"))]
mod error_samples;
pub mod fencing;
#[cfg(test)]
mod fencing_tests;
mod fleet_format;
mod fork_plan;
mod graph_commit;
mod head_ownership;
pub mod history;
pub mod ingress_obligation;
mod ingress_terminal;
pub mod plugin_writers;
pub use ingress_terminal::{IngressTerminal, IngressTerminalCause};
mod lease_timings;
mod maintenance;
pub use enumeration::*;
pub mod obligation;
mod park;
pub mod pending_follow_on;
mod physical_turn;
mod preflight;
pub mod queued_work;
mod record_schema_version;
#[cfg(feature = "synthetic-next")]
mod synthetic_next;
#[cfg(feature = "synthetic-next")]
mod synthetic_next_versions;
pub use physical_turn::PhysicalTurn;
mod control_intent;
mod realization;
pub mod recovery_leader;
mod retention;
mod run;
mod session_head;
mod shift_admission;
pub use session_head::{
    BlobRef, SessionAdmission, SessionHeadMeta, SessionHeadPayload, SessionMeta,
    validate_session_id,
};
pub mod runtime_commit;
mod runtime_commit_plan;
mod semantic_boundary;
mod session_config_views;
mod session_view;
mod shift_fence;
pub mod tool_material;
mod tool_receipts;
pub use session_config_views::{
    execution_session_config_from_state, persisted_session_config_from_state,
    recorded_session_policy_from_state, root_snapshot_config_from_state,
};
pub use tool_receipts::{ToolCompletionReceipt, ToolRequestReceipt, require_tool_request_matches};
mod lease_owner;
pub mod session_delete;
mod session_fault;
mod state_version;
#[cfg(any(test, feature = "testing"))]
mod testing;
mod window_load;
pub mod worker_recovery;

use record_schema_version::record_schema_version;
pub use record_schema_version::{
    ensure_supported_record_schema_version, ensure_supported_schema_version,
};

pub use crate::session_graph::RealizedNodeTimestamp;
pub use crate::session_store_factory_types::{RetainedRevision, Retention, SessionLookup, Target};
pub use admission_plan::{
    IngressRowId, IngressSettlement, RUN_ADMISSION_STEP, TerminalProcessWake, TurnLaneStop,
    deferred_wake_records, plan_checkpoint_input_admission, plan_next_turn_input_admission,
    require_admitted_to_run, require_open_command, turn_input_state_after_admission,
};
pub use artifact_cleanup::{ArtifactCleanupLedger, CleanupUpsert};
pub use attachment_referrers::{
    AdoptedAttachmentCondemnation, AttachmentCondemnation, AttachmentCondemnationAdoption,
    AttachmentCondemnationPhase, AttachmentCondemnationProvenance, AttachmentCondemnationRecord,
    AttachmentCondemnationSettlement, AttachmentDeleteArming, AttachmentDeleteStallReason,
    AttachmentReferrers, AttachmentSettlementOutcome, AttachmentSweepGeneration, AttachmentWrite,
    AttachmentWriteFence, AttachmentWritePermit, AttachmentWriteToken,
    MAX_ATTACHMENT_DELETE_ATTEMPTS, SessionReferrerState, StoredAttachmentCondemnation,
    decode_attachment_condemnation_record,
};
pub use catalog::{SessionCatalogStore, admit_created_session};
pub use commit_budget::{CommitBudget, CommitBudgetLimit};
pub use commit_identity::{
    APPEND_REQUEST_IDENTITY_ENCODING_VERSION, OperationId, RuntimeCommitReceiptDecision,
    decide_runtime_commit_receipt, derive_history_node_id,
};
pub use control_intent::{
    CONTROL_INTENT_FORMAT, ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState,
    ControlIntentStore, IntentApplication, IntentObligation, IntentSettle, RunIntentFacts,
    RunIntentPlan, RunIntentRefused, RunIntentRequest, RunVerb, decide_intent_acknowledgement,
    decide_intent_application, decide_intent_refusal, decide_run_intent, forked_run,
    stored_intent_kind, stored_intent_state,
};
pub use error::{AnchorUnavailable, StoreError, StoreFault, StoreRefusal, WindowAnchorViolation};
pub use fencing::{
    FENCED_WRITE_DISAGREEMENT_EVENT, FENCING_TRACE_TARGET, FencedWrite, HeadPublicationVerdict,
    WakeDeliveryClaimFacts, WakeDeliveryClaimVerdict, fenced_write_applied,
    head_publication_verdict, require_fenced_write_applied, require_single_writer_head_publication,
    wake_delivery_claim_verdict,
};
pub use fleet_format::{
    DurableRecord, FLEET_FORMAT_VERSION, FLEET_WRITABLE_RANGE, FleetFormat, FleetFormatState,
    GUARDED_SURFACES, GuardedSurface, Lift, RECORD_UPCASTERS, ReadWindow, RecordUpcaster,
    SurfaceFormat, SurfaceReads, WriterPin, decode_versioned_json_record,
    decode_versioned_json_record_for_fleet, decode_versioned_msgpack_record_for_fleet,
    ensure_supported_record_schema_version_for_fleet, ensure_supported_schema_version_for_fleet,
    guarded_surface, upcast_chain_covers, upcast_json_record, upcaster,
};
pub use fork_plan::{ForkLineageAncestor, ForkNodeFacts, ForkPlan};
pub use head_ownership::{
    HeadOwnershipFacts, SessionHeadOwner, follow_on_owning_the_head, head_write_needs_ownership,
    require_unowned_head,
};
pub use history::{
    FailureEvidenceCursor, FailureEvidencePage, HistoryAnchor, HistoryBudget, HistoryCursor,
    HistoryNode, HistoryPage, HistoryStop, LineageStamp, SessionHistoryStore, SessionWindowRead,
    WindowSelector,
};
pub use lease_owner::LeaseOwnerIdentity;
pub use lease_timings::{LeaseTimings, LeaseTimingsError};
pub use maintenance::{
    GcReport, MaintenanceFailure, MaintenanceRefusal, MaintenanceReport, MaintenanceResult,
    MaintenanceStop, MaintenanceSweep, SessionBlobReclaimReport, VacuumReport,
};
pub use obligation::*;
pub use park::{
    EnginePark, ParkCancelCause, ParkEventColumns, ParkEventKind, ParkFeedCursor, ParkFeedEvent,
    ParkFeedPage, ParkId, ParkReason, ParkReasonCode, ParkReport, ProcessPark, ProcessParkKey,
    ProcessParkQuery, ProcessParkWrite, StoreTransition, StoredParkRedrive, StoredTurnParkHead,
    TurnPark, TurnParkOrigin, TurnParkQuery, TurnParkTarget, TurnParkWrite, TurnParkWriteDecision,
    UnparkCause, UnsettledTurnCounts, decide_turn_park_write,
};
pub use pending_follow_on::{
    DEFAULT_MAX_FOLLOW_ON_RECOVERIES, FollowOnAdmission, FollowOnBlocked, FollowOnRecovery,
    FollowOnRecoveryAnswer, FollowOnWork, PendingFollowOn, RunContinuation, RunOpenerState,
    SuspendedCell, follow_on_blocks_admission, validate_follow_on_head_write,
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
pub use retention::{RetentionBound, RetentionReport};
pub use run::{
    AdmitRunRequest, AdmittedHead, AdmittedHeadVerdict, CheckpointAdmission,
    CheckpointAdmissionRequest, InMemoryRunLedger, PreparedRunAdmission, RunAdmission,
    RunAdmissionAnswer, RunAdmissionRefusal, RunCommittedOutcome, RunEndOutcome, RunExecutor,
    RunStore, RunTerminal, RunTerminalCause, RunTerminalKind, RunTerminalWrite,
    RunTerminalWriteDecision, RunTurns, StoredRunTerminal, TurnCancellationBinding, TurnCommitId,
    UnfinishedRun, admit_run_with_trace, decide_run_terminal_write, refused_execution_owns_run,
    run_binding_conflict,
};
pub use runtime_commit::{
    AppendRequestIdentity, FrameTransition, InterruptedTurnClosure,
    RUNTIME_COMMIT_RECEIPT_RECORD_KIND, RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION, RuntimeCommit,
    RuntimeCommitReceipt, RuntimeTurnCommitStamp, SemanticBoundaryOperation, TurnChange,
    TurnChangeCursor, TurnChangeKind, TurnChangePage, TurnCommitFailureCause, TurnCommitOutcome,
    TurnProjectionWatermark, TurnTraceReceipt, decode_runtime_commit_receipt,
    decode_runtime_commit_receipt_for_fleet, ensure_supported_receipt_version,
    ensure_supported_receipt_version_for_fleet, frames_left_by_commit,
    validate_turn_commit_outcome_code,
};
pub use runtime_commit_plan::{
    FreshRuntimeCommitFacts, ParentNodeFacts, PlannedNodeFacts, PublishedLeafFacts,
    RuntimeCommitPlan, RuntimeCommitPlanner, RuntimeCommitReceiptRecord, RuntimeCommitReceiptWrite,
    RuntimeCommitReplay,
};
pub use semantic_boundary::{
    CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION,
    RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION,
};
pub use session_fault::{SessionFault, SessionFaultOrigin, SessionFaultRecord};
pub use shift_admission::*;
pub use shift_fence::{
    AdmissionId, HeldRun, InMemoryShiftEpochs, RunHold, RunStartNonce, SessionHeadRef,
    ShiftEpochSeal, ShiftEpochSealDecision, ShiftEpochStore, ShiftFence, ShiftRaise,
    StoredShiftEpoch, close_admission, current_shift_fence, decide_run_hold,
    decide_shift_epoch_seal, require_current_shift_fence,
};
pub use tool_material::ToolMaterialStore;

pub use session_view::SessionStore;
pub use state_version::{
    CURRENT_SESSION_STATE_VERSION, OLDEST_SUPPORTED_SESSION_STATE_VERSION, SessionAdmissionWindow,
    SessionStateAdmission, resolve_session_state_version,
};
#[cfg(any(test, feature = "testing"))]
pub use testing::{
    ConformanceStore, DecodedRowCounts, GraphRowCorruption, StoreTestSupport,
    append_request_commit_with_clock_for_testing,
};
pub use window_load::{
    LoadedSessionWindow, load_session_read_view, load_session_window_state, refresh_session_window,
    window_state,
};

fn default_root_session_id() -> SessionId {
    SessionId::parse("root").expect("the root session id is nonblank")
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
///
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "SessionHeadMeta"
pub const SESSION_HEAD_META_SCHEMA_VERSION: u32 = 11;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the surface one version on
/// with version 11's shape; its registered lift reads what N wrote.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "SessionHeadMeta"
pub const SESSION_HEAD_META_SCHEMA_VERSION: u32 = 12;

#[cfg(test)]
#[cfg(test)]
mod guarded_surface_tests;
#[cfg(test)]
mod persisted_state_tests;

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
        Ok(())
    }

    /// Exhaustive append-envelope allowlist. Adding a new commit member forces
    /// this destructure to be reconsidered, while the checks keep append
    /// commits from silently acquiring another unrelated settlement side
    /// effect.
    pub fn debug_assert_append_envelope_scope(&self) {
        let RuntimeCommit {
            commit_budget: _,
            session_id: _,
            expected_head_revision: _,
            shift_fence: _,
            run_terminal,
            park_run,
            frame_transition,
            config: _,
            execution_config: _,
            graph: _,
            graph_base_leaf_node_id: _,
            checkpoint: _,
            failure_evidence,
            outcome,
            trace,
            turn_commit: _,
            ingress,
            applied_commands,
            command_outcomes,
            // Carried unchanged from the head; the store refuses a change.
            pending_follow_on: _,
            interrupted_turn,
            adopted_intent_rows,
            committed_attachment_ids,
        } = self;
        debug_assert!(
            ingress.is_none()
                && applied_commands.is_none()
                && command_outcomes.is_empty()
                && interrupted_turn.is_none()
                && *adopted_intent_rows == 0
                && failure_evidence.is_empty()
                && outcome.is_none()
                && trace.is_none()
                && committed_attachment_ids.is_empty()
                && run_terminal.is_none()
                && park_run.is_none()
                && frame_transition.is_none(),
            "append-session-nodes constructor gained unrelated settlement side effects"
        );
    }

    /// Re-derives every appended node ID by ordinal for store implementors, using frame keys for
    /// frame-open nodes and operation identity for all other nodes.
    pub fn validate_node_derivation(&self) -> Result<(), StoreError> {
        let completed = &self.turn_commit;
        for (ordinal, node) in self.graph.nodes().iter().enumerate() {
            let expected = match &node.payload {
                crate::SessionNodePayload::FrameOpen { frame_key, .. } => crate::NodeId::from(
                    crate::session_graph::frame_node_id(&self.session_id, frame_key.as_str()),
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
    /// without presenting a shift fence, or names one row twice, before any
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
        if self.command_outcomes.keys().any(|batch_id| {
            !self
                .applied_commands
                .as_ref()
                .is_some_and(|commands| commands.batch_ids.contains(batch_id))
        }) {
            return Err(StoreError::Backend(
                "command outcomes must name batches settled by the same commit".to_string(),
            ));
        }
        if settles_rows && self.shift_fence.is_none() {
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
            operation,
            commit_budget,
            fleet_format,
        )?;
        Ok((commit, persisted_node_ids))
    }

    pub fn persisted_state_with_graph_commit_and_operation_and_budget(
        state: &crate::RuntimeSessionState,
        graph: GraphAppend,
        operation: OperationId,
        commit_budget: CommitBudget,
        fleet_format: FleetFormat,
    ) -> Result<Self, StoreError> {
        let config = persisted_session_config_from_state(state);
        let execution_config = state
            .authority
            .run_view()
            .is_some()
            .then(|| execution_session_config_from_state(state))
            .filter(|execution| *execution != config)
            .map(Box::new);
        Ok(Self {
            commit_budget,
            session_id: state.session_id.clone(),
            expected_head_revision: state.head_revision,
            shift_fence: None,
            run_terminal: None,
            park_run: None,
            frame_transition: None,
            config,
            execution_config,
            graph,
            graph_base_leaf_node_id: state.session_graph.leaf_node_id.clone(),
            checkpoint: build_checkpoint_from_persisted_state(state, fleet_format)?,
            failure_evidence: Vec::new(),
            outcome: None,
            trace: None,
            turn_commit: RuntimeTurnCommitStamp::new(operation),
            ingress: None,
            applied_commands: None,
            command_outcomes: Default::default(),
            pending_follow_on: state.pending_follow_on.as_deref().cloned(),
            interrupted_turn: None,
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
        self.turn_commit = RuntimeTurnCommitStamp::new(operation);
        Ok((self, node_id_mapping))
    }

    /// Present `fence`: the shift fence the commit's run was sealed under.
    pub fn fenced_by(mut self, fence: ShiftFence) -> Self {
        self.shift_fence = Some(Box::new(fence));
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

    /// Closes one interrupted turn's cancellation gate with this commit, so
    /// store implementors atomically settle the turn's undelivered
    /// active-turn inputs. The turn and its cancellation evidence are the
    /// settlement's; `observed_intent` is the cancel-intent snapshot the
    /// backend compares before it publishes.
    pub fn closing_interrupted_turn(
        mut self,
        settlement: crate::TurnCancelClosureSettlement,
        observed_intent: crate::TurnCancelIntentSnapshot,
    ) -> Self {
        self.interrupted_turn = Some(InterruptedTurnClosure {
            settlement,
            observed_intent,
            admitted_intent: None,
        });
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
        Err(StoreError::StoreSessionMismatch {
            loaded: read.session_id.clone(),
            requested: session_id.clone(),
        })
    }
}

/// Settled-session commit capability: the runtime's atomic transaction
/// facade for visible session state.
///
/// This segment owns session head commits, checkpoint hydration and usage,
/// final turn-commit idempotency, session metadata and turn parks.
/// The rows a run admitted also settle here —
/// [`commit_runtime_state`](Self::commit_runtime_state) completes, releases
/// or drops them by the commit's [`IngressSettlement`] in the same atomic
/// commit (FIG-3927). History reads are [`SessionHistoryStore`]'s. In-flight
/// nondeterministic work belongs to the owning actor's context, not to the
/// store contract.
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
    /// Read the accepted request when a Run restores only its suffix.
    /// A retained receipt carries identity and original facts, never a permit.
    async fn tool_request_receipt(
        &self,
        request_key: &str,
    ) -> Result<Option<ToolRequestReceipt>, StoreError>;
    /// Retain the first sealed request. The disposition is issued after commit.
    async fn record_tool_request(
        &self,
        request: &ToolRequestReceipt,
    ) -> Result<StoreTransition<ToolRequestReceipt>, StoreError>;
    /// Retain the first result under the request's digest; never rewrite a terminal.
    async fn record_tool_completion(
        &self,
        completion: &ToolCompletionReceipt,
    ) -> Result<StoreTransition<ToolCompletionReceipt>, StoreError>;

    /// The session's physical session-state generation marker. A legacy
    /// absent marker reads as [`OLDEST_SUPPORTED_SESSION_STATE_VERSION`].
    async fn read_session_state_version(&self, session_id: &SessionId) -> Result<u32, StoreError>;

    /// Revalidate `fence`, then classify the independently read session-state
    /// marker of `fence.session()`.
    async fn admit_session_state(
        &self,
        fence: &ShiftFence,
    ) -> Result<SessionStateAdmission, StoreError> {
        let version = self.read_session_state_version(fence.session()).await?;
        Ok(SessionStateAdmission {
            session_id: fence.session().clone(),
            version,
            shift_epoch: fence.epoch(),
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
    /// Called by a turn's admission under the session's shift fence, once per
    /// first execution. While it stands, maintenance that reclaims
    /// unreferenced checkpoints treats `base.checkpoint` as a root, so a
    /// replay of the admitted turn can rebuild its input state even after the
    /// turn's own commit superseded the head and a vacuum ran. A backend that
    /// never reclaims a superseded checkpoint answers `Ok(())` explicitly.
    async fn retain_admission_base(
        &self,
        fence: &ShiftFence,
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

    /// Atomically persist one settled runtime commit and its durable receipt
    /// for `commit.session_id`.
    ///
    /// A commit carrying [`RuntimeCommit::shift_fence`] is refused
    /// [`StoreError::StaleShiftFence`] unless the fence is still the session's
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
    /// Model usage and attempt history are recorded with the model result (ADR 0127).
    ///
    /// A fresh identity-bearing append enforces
    /// the optional ancestor in [`AppendRequestIdentity::Append`] against the
    /// transaction's active path, then atomically publishes graph, checkpoint,
    /// queue/input settlements, attachment adoptions, and a receipt whose
    /// stored replay bit is `false`. Receipt lookup, fresh-only ancestor fencing,
    /// commit publication, and receipt insertion are one transaction.
    ///
    /// Every row the commit's [`IngressSettlement`] names must still be bound
    /// to its run, or the commit is refused whole
    /// [`StoreError::IngressRowNotAdmitted`]; every applied command must still
    /// exist and be open, or it is refused
    /// [`StoreError::SessionCommandWithdrawn`]. A commit that writes its
    /// run's terminal releases, in the same transaction, every row still
    /// bound to the run (FIG-3927).
    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError>;

    /// The follow-on the session head owes, if any (ADR 0101 §3): the head's
    /// `pending_follow_on` as it is committed now.
    ///
    /// Shift admission asks this to decide whether the follow-on is the next
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
    /// A shift that recovers a pending follow-on calls this before the
    /// follow-on's first effect. Implementations must, in one transaction,
    /// validate `fence` against the session's current shift fence, refuse with
    /// [`StoreError::FollowOnNotPending`] unless the head's
    /// `pending_follow_on_json` names `follow_on_turn_id`, and write the fact
    /// back with `attempts` raised by one. The head revision does not move:
    /// the raise changes no other head fact, and nothing ever lowers the count.
    /// Returns the raised fact.
    async fn raise_pending_follow_on_attempts(
        &self,
        fence: &ShiftFence,
        follow_on_turn_id: &TurnId,
    ) -> Result<PendingFollowOn, StoreError>;

    /// Replace only the pending observer intents of an admitted session.
    ///
    /// This update never writes lineage, creation provenance or the owning
    /// process. A missing session is refused with [`StoreError::SessionNotFound`];
    /// a deleted session remains fenced with [`StoreError::SessionDeleted`].
    async fn settle_observer_intents(
        &self,
        session_id: &SessionId,
        remaining: Vec<crate::SessionObserverIntent>,
    ) -> Result<(), StoreError>;

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
    /// as does a cancel that ends the parked run and the session's
    /// deletion: a park is live exactly while its turn is.
    ///
    /// Returns the record as stored, so the caller can report the allocated
    /// `park_id` and attempt count.
    async fn record_turn_park(
        &self,
        park: &TurnParkWrite,
    ) -> Result<StoreTransition<TurnPark>, StoreError>;

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
/// Rows enter here and wait open. A run binds the rows it executes
/// ([`RunStore::admit_run`], [`RunStore::admit_at_checkpoint`]), and only
/// that run's commit or terminal settles or releases them again
/// ([`SessionCommitStore::commit_runtime_state`], FIG-3927). User input must
/// not be represented as generic queued work ([`QueuedWorkStore`]).
#[async_trait::async_trait]
pub trait TurnInputStore: Send + Sync {
    /// Persist or validate the one cancellation authority selected for this
    /// session and, for a Process or runtime-operation controller, its physical
    /// journal scope. Session-bound turns keep their exact canonical address in
    /// each closure authorization, so distinct turns may share this authority.
    /// The check occurs under the current shift fence before any session work
    /// and never replaces the original selection.
    async fn validate_turn_cancellation_binding(
        &self,
        session_id: &SessionId,
        fence: &ShiftFence,
        binding_id: &str,
        admitted_scope: &crate::ExecutionScope,
    ) -> Result<(), StoreError>;

    /// Authorize exact closure of one cancellation gate pair for the admitted
    /// session and binding. The shift fence authenticates the proposal, but
    /// its epoch does not fence final settlement.
    /// A vacant slot accepts this value, an identical retry adopts it, and a
    /// different occupied value or retired physical scope returns a typed refusal.
    /// Final publication additionally requires the session-head CAS.
    async fn authorize_turn_cancel_closure(
        &self,
        fence: &ShiftFence,
        authorization: &crate::TurnCancelClosureAuthorization,
    ) -> Result<crate::TurnCancelClosureAuthorizationOutcome, StoreError>;

    /// Load every unconsumed closure obligation for the bound session after
    /// validating the current shift fence, selected binding, and any
    /// original non-session physical scope.
    async fn pending_turn_cancel_closures(
        &self,
        session_id: &SessionId,
        fence: &ShiftFence,
        binding_id: &str,
        admitted_scope: &crate::ExecutionScope,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;

    /// Read `session_id`'s unconsumed closure pins for lifecycle
    /// coordination without presenting a shift fence. This grants no right to
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
    ///
    /// Every new row is armed as its session's ingress obligation in the
    /// same transaction (ADR 0109 §3). A batch
    /// [`held_by_acceptor`](crate::PendingTurnInputBatch::held_by_acceptor)
    /// also takes each row's still-due claim there, held for the batch's
    /// TTL: its acceptor executes the rows itself, and no relay pass may find
    /// them due before that shift admits them.
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
    /// Completed and cancelled rows are excluded. A row a run admitted,
    /// at the run's own admission or at one of its checkpoints, is returned
    /// as
    /// [`PendingTurnInputReadStatus::Admitted`](crate::PendingTurnInputReadStatus::Admitted)
    /// naming that run until the run's commit completes it or its terminal
    /// releases it, and every other row as
    /// [`Open`](crate::PendingTurnInputReadStatus::Open). An admitted row is
    /// answered by its run alone; resubmitting the same input under the same
    /// source key returns the row.
    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::PendingTurnInputRead>, StoreError>;

    /// Read one pending user input by id: the row
    /// [`list_pending_turn_inputs`](Self::list_pending_turn_inputs) lists for
    /// `input_id`, with the status the list gives it, or `None` once it is
    /// completed, cancelled or unknown.
    ///
    /// A durable backend answers with one point read by
    /// `(session_id, input_id)`, so a follower asking whether its own input
    /// is still open pays no scan of the session's queue.
    async fn pending_turn_input(
        &self,
        session_id: &SessionId,
        input_id: &crate::InputId,
    ) -> Result<Option<crate::PendingTurnInputRead>, StoreError>;

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
    /// and changes nothing: the host cancels its run instead.
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

impl dyn TurnInputStore {
    /// Persist the one draft a child session's turn accepts before its
    /// acceptor executes it inline (ADR 0069 §6): a batch of one
    /// [`held_by_acceptor`](crate::PendingTurnInputBatch::held_by_acceptor)
    /// for `claim_ttl_ms`, the relay's claim TTL.
    pub async fn accept_pending_turn_input(
        &self,
        input: crate::PendingTurnInputDraft,
        claim_ttl_ms: u64,
    ) -> Result<crate::PendingTurnInput, StoreError> {
        let batch = crate::PendingTurnInputBatch::one(input).held_by_acceptor(claim_ttl_ms);
        crate::PendingTurnInputBatch::only(self.enqueue_pending_turn_inputs(batch).await?)
    }
}

/// Durable queued work (ADR 0101, `queued_work_batches`): process wakes and
/// session commands, with their lifecycle reads.
///
/// Batches enter here and wait open. A run admits turn work
/// ([`RunStore::admit_run`], [`RunStore::admit_at_checkpoint`]), the
/// command lane applies session commands
/// ([`open_session_command_run`](Self::open_session_command_run)), and only
/// the admitting run's or the applying commit settles them
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
    ///
    /// Admission records the draft's
    /// [`submission_digest`](crate::QueuedWorkBatchDraft::submission_digest)
    /// (ADR 0101 §8). A draft whose source key the session already filed
    /// answers that batch, open or a tombstone, as
    /// [`Existing`](crate::QueuedWorkEnqueueOutcome::Existing) when the
    /// digests are equal, and nothing reopens; a changed digest is
    /// [`StoreError::QueuedWorkSourceKeyConflict`] and nothing is stored.
    ///
    /// A changed process wake is that wake's terminal (FIG-4487): the
    /// refusing transaction raises the session's redelivery floor to
    /// `max(floor, sequence)` and commits before the conflict is returned,
    /// leaving the stored wake untouched. After vacuum, a retry at or below
    /// the floor is refused with [`StoreError::ProcessWakeSequenceRewound`].
    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, StoreError>;

    /// The session's leading open session-command run, for the command lane
    /// to apply (ADR 0101 §4, design §2.7). Takes no admission.
    ///
    /// The run is returned only when the earliest open batch is classified
    /// as [`QueuedWorkClass::SessionCommand`]. Every command is a run of one
    /// ([`SESSION_COMMAND_BATCHES_PER_RUN`](crate::store::queued_work::SESSION_COMMAND_BATCHES_PER_RUN)),
    /// applied alone in the commit that settles it. In one transaction fenced by
    /// `fence`, the run's ingress obligations are acknowledged delivered
    /// (ADR 0109 §3). The applying commit settles the rows
    /// ([`RuntimeCommit::applied_commands`]). The read admits the run: a
    /// host withdrawal no longer reaches a delivered command
    /// ([`Self::cancel_queued_work_batch`]), so plugin code a command runs
    /// only ever runs for a command that will settle.
    async fn open_session_command_run(
        &self,
        fence: &ShiftFence,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;

    /// Withdraw an open queued-work batch from durable ingress into its
    /// `cancelled` tombstone (ADR 0101 §8).
    ///
    /// Returns the batch as it stood open when cancellation won the race.
    /// Returns `None` when the batch is missing, a tombstone, or held by a
    /// run; callers must treat that as "already admitted or completed" and
    /// must not restore any stale local draft state.
    /// A command whose fenced read delivered its obligation is being applied
    /// and cannot be withdrawn.
    ///
    /// Cancelling a process-wake batch is a terminal transition of that wake:
    /// the session's redelivery fence rises to `max(floor, sequence)` in the
    /// same transaction as the tombstone. A redelivery of the same
    /// `(process, sequence)` answers the tombstone and reopens nothing; after
    /// host vacuum it is refused with
    /// [`StoreError::ProcessWakeSequenceRewound`].
    async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, StoreError>;

    /// The receipt of the session-command head commit that completed
    /// `batch_id`, read through its terminal row's applying operation key.
    /// The terminal row and the original commit receipt are written atomically.
    /// `None` while open, cancelled, or vacuumed. The receipt carries what
    /// the command settled as, such as an administrative compaction's
    /// [`command_outcomes`](RuntimeCommitReceipt::command_outcomes)
    /// (FIG-4201).
    async fn queued_work_batch_completion(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<RuntimeCommitReceipt>, StoreError>;

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

    /// List the queued-work batches no run admitted: still open for
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
    async fn has_admissible_queued_work(&self, session_id: &SessionId) -> Result<bool, StoreError>;
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
    /// admission evidence, not replay state: a replayed turn executes the shift
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
    /// [`Self::vacuum`] — a backend that cannot enumerate its runs refuses
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
/// knowledge stays unwritten until `lashctl finalize` moves the row
/// (FIG-3800). A store bound to a session answers the `F` its backend
/// admitted at open; a store with no recorded row — an in-memory fake or a
/// pre-`F` store — answers [`FleetFormat::current`], the only generation such
/// a store could write.
pub trait FleetFormatStore: Send + Sync {
    /// The recorded fleet format this store's writers emit.
    fn fleet_format(&self) -> FleetFormat;

    /// The per-plugin writer ranges the fleet record carries beside `F`
    /// (FIG-4746), read from the store. A store with no fleet record carries
    /// none.
    fn plugin_writers(&self) -> PluginWriterRangesFuture<'_> {
        Box::pin(async { Ok(plugin_writers::PluginWriterRanges::default()) })
    }

    /// Provision a writer range for every plugin of `registrations` the
    /// fleet record does not name, and answer the recorded ranges. A recorded
    /// range is never changed: only finalize moves one. Inside a rollback
    /// window a provisioned plugin writes its oldest writable format; once
    /// the fleet epoch is this build's own it writes up to its native one.
    ///
    /// A store with no fleet record has no older build to protect and
    /// answers the ranges a finalized fleet would record.
    fn provision_plugin_writers<'a>(
        &'a self,
        registrations: &'a [plugin_writers::PluginWriterRegistration],
    ) -> PluginWriterRangesFuture<'a> {
        Box::pin(async move {
            let ranges = plugin_writers::PluginWriterRanges::default();
            let provisioned = ranges.provisioned(registrations, true);
            Ok(ranges.with(provisioned))
        })
    }
}

/// What [`FleetFormatStore::plugin_writers`] and
/// [`FleetFormatStore::provision_plugin_writers`] answer.
pub type PluginWriterRangesFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<plugin_writers::PluginWriterRanges, StoreError>>
            + Send
            + 'a,
    >,
>;

/// The runtime's store: one object per catalog, keyed by session (ADR 0112
/// §1).
///
/// `Arc<dyn RuntimeStore>` implements every store segment —
/// [`AttachmentReferrers`] (the attachment write-ahead manifest),
/// [`SessionCatalogStore`] (admission, lookup, enumeration, forks and
/// deletion), [`SessionCommitStore`] (atomic head commits, metadata and
/// parks), [`SessionHistoryStore`] (frame windows and paged history),
/// [`TurnInputStore`] (pending turn-input lifecycle), [`QueuedWorkStore`]
/// (queued-work ingress and claiming), [`ShiftEpochStore`] (the shift epoch a
/// session shift's seal raises, FIG-3600), [`RunStore`] (logical runs'
/// terminal evidence and input bindings) and [`StoreMaintenance`]
/// (vacuum/GC). The segments share one transactional domain: claims granted by
/// the input and queue segments settle atomically in
/// [`SessionCommitStore::commit_runtime_state`]. In-flight nondeterministic
/// work belongs to the owning actor's context, not to the store contract.
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
    + AttachmentReferrers
    + SessionCatalogStore
    + SessionCommitStore
    + SessionHistoryStore
    + TurnInputStore
    + QueuedWorkStore
    + ShiftEpochStore
    + RunStore
    + StoreMaintenance
{
}

impl<T> RuntimeStore for T where
    T: FleetFormatStore
        + AttachmentReferrers
        + SessionCatalogStore
        + SessionCommitStore
        + SessionHistoryStore
        + TurnInputStore
        + QueuedWorkStore
        + ShiftEpochStore
        + RunStore
        + StoreMaintenance
        + ?Sized
{
}

mod runtime_store_decorator;
pub use runtime_store_decorator::RuntimeStoreDecorator;
#[cfg(any(test, feature = "testing"))]
pub use runtime_store_decorator::StoreOp;

#[cfg(test)]
mod tests;
