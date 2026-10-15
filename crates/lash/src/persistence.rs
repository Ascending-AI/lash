//! Store-author contracts, including the process registry and its row.
//!
//! Custom stores implement these ports against `lash` alone. `ProcessRecord`
//! is the durable lifecycle fold stores persist and return; receipts and
//! authorities here govern writes. Hosts and plugins read
//! [`crate::process::ObservedProcess`] through `processes()` or their
//! runtime-provided services instead.
pub use lash_core::{Durable, ProcessAuthorityReads};
pub use lash_core_store::checkpoint_graph::{
    DecodeLimits as GraphDecodeLimits, GraphReadPolicy, KernelDataLimits,
};

#[cfg(any(test, feature = "testing"))]
pub use lash_core::{
    ConformanceProcessRegistry, ProcessEventLogTestSupport, ProcessRegistryTestSupport,
};
pub use lash_core::{
    NonTerminalProcessPage, ProcessCancelReceipt, ProcessChange, ProcessChangeKind,
    ProcessClockRebind, ProcessCompletionAuthority, ProcessCompletionOutcome,
    ProcessEventAppendRequest, ProcessEventLog, ProcessHandleView, ProcessLifecycle,
    ProcessListFilter, ProcessLiveReferenceView, ProcessObserverRegistry, ProcessOpScope,
    ProcessQuery, ProcessRecord, ProcessRegistrar, ProcessRegistrationReceipt, ProcessRegistry,
    ProcessRetention, ProcessRosterRecords, ProcessStartReceipt, ProcessToolIntents,
    StagedProcessStart,
};

// The vocabulary this module's signatures name (the facade-completeness rule).
pub use lash_core_store::artifact_referrer::{
    ArtifactCarry, ArtifactCleanup, ArtifactReferrerError, ArtifactReferrerKind, ArtifactStoreId,
    AttachmentUploadId, ReferrerGuard, ReferrerStore, UploadReferrerId,
};
pub use lash_core_store::attachments::{AttachmentExecutionBinding, AttachmentHolder};
pub use lash_core_store::compat::{CompatRefusal, CompatStamp};
pub use lash_core_store::runtime_error::ExecutableGenerationRefusal;
pub use lash_core_store::session_graph::{
    NodeTimestamp, NodeTimestampError, SessionGraphAppendBuilder, SessionGraphData,
    SessionNodeDraft, SessionReadModel,
};
pub use lash_core_store::session_identity::{
    SessionLineage, SessionObservedProcessOutcome, SessionObservedProcessReceipt,
    SessionObserverIntent,
};
pub use lash_core_store::session_state::{
    InstalledRunView, RuntimeSessionAuthority, SessionPluginStateSource,
};
/// Retained tool material: a store's dependency leases on bundles.
pub use lash_core_store::store::ToolMaterialStore;
pub use lash_core_store::store::commit_budget::RuntimeCommitBudgetMeasurement;
pub use lash_core_store::store::{
    DurableRecord, EnumerationSource, FrameTransition, ReadWindow, StoreFault, StoreRefusal,
    StoredRunTerminal, SurfaceFormat, WriterPin,
};
pub use lash_core_store::{PersistedNodeIds, surface_format};
/// The protocol-generic form [`SessionHistoryRecord`] specializes.
pub use lash_sansio::SessionHistoryRecord as GenericSessionHistoryRecord;
pub use lash_sansio::{AppendVec, BaseRenderCache, ConversationRecord};
pub use lash_sansio::{VersionRange, VersionRangeError};

pub use lash_core::CheckpointKind;
/// The store halves a [`StoreSet`](crate::StoreSet) hands out as trait
/// objects, nameable so a host can decorate a store set (FIG-4373).
pub use lash_core::ProcessDefinitionStore;
pub use lash_core::RunSpecHash;
pub use lash_core::attachments::{
    AttachmentRootPage, AttachmentRootSource, CompleteAttachmentRoots,
};
/// The logical run reference a run store ends a run by.
pub use lash_core::engine::RunRef;
/// Durable session-store inputs and outputs exposed to storage integrators.
pub use lash_core::runtime::{
    ActiveTurnIngress, AdmissionBoundary, AdmittedQueuedWork, AdmittedTurnInputs, DeliveryPolicy,
    DeploymentStore, DeploymentStoreDecorator, ForkSessionReceipt, ForkSessionRequest,
    LiveReplayOutcome, LiveReplaySubscription, PendingTurnInputBatch, PendingTurnInputDraft,
    QueuedCheckpointTurnInput, QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft,
    QueuedWorkCompletion, QueuedWorkEnqueueOutcome, QueuedWorkPayload, RuntimeCheckpointComponents,
    RuntimeSessionState, SessionCreationHead, SessionCursorError, SessionStoreCreateRequest,
    TurnInputAdmissionMode, TurnInputCheckpointBoundary, TurnInputCompletion,
    TurnInputCompletionData, TurnInputIngress, TurnInputState, TurnInputStateKind,
    TurnLaneAdmissionPolicy,
};
pub use lash_core::session_graph::RealizedNodeTimestamp;
/// The artifact-cleanup ledger a [`StoreSet`](crate::StoreSet) hands out
/// as a trait object (FIG-4373).
pub use lash_core::store::ArtifactCleanupLedger;
/// The current state of an obligation a custom ledger exposes.
pub use lash_core::store::ObligationStanding;
/// Head and usage values returned by custom session stores.
pub use lash_core::store::SessionHeadRef;
/// The store halves a storage integrator's [`StoreSet`](crate::StoreSet)
/// supplies: the obligation ledgers and the recovery leader lease
/// (ADR 0109 §1.3, §1.6).
pub use lash_core::store::{
    ClaimToken, ClaimedObligation, HolderId, KeyColumn, KeyColumnType, LeaseAnswer, LeaseClaim,
    LeaseName, LeaseRow, ObligationLedger, ObligationSettlement, RecoveryLeaderStore,
    SettleOutcome,
};
pub use lash_core::store::{StoreTransition, TurnTraceReceipt};
/// Artifact ownership supplied to protocol engines and effect controllers.
pub use lash_core::{
    ArtifactName, ArtifactReferrer, FrameEnvironmentId, ReferrerClaim, ResolvedArtifactCleanup,
};
pub use lash_core::{AttachmentReferrers, AttachmentWrite, SessionReferrerState};
/// Queued-work ordering values and admission-selection helpers.
pub mod queued_work {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core_store::store::queued_work::TurnLaneCandidate;

    /// Stable queued-work ordering values and selection helpers for store implementations.
    pub use lash_core::store::queued_work::{
        PendingSessionWorkOrdering, PendingWorkOrderingKey, admission_scan_limit, derive_batch_id,
        select_leading_session_command,
    };
}
pub use lash_core::attachments::{
    AttachmentReclamationRetryPolicy, AttachmentReclamationRetryPolicyError,
};
pub use lash_core::session_graph::WindowAnchor;
pub use lash_core::store::PluginWriterRangesFuture;
/// A session's fault record (ADR 0109 §9): one segment of
/// [`RuntimeStore`], implemented by every store a runtime executes.
pub use lash_core::store::SessionFaultStore;
pub use lash_core::store::plugin_writers::{
    AdmittedPlugin, PluginAdmission, PluginPublication, PluginWriterRanges,
    PluginWriterRegistration, PluginWriterStamp,
};
/// A run's admission, what its checkpoints admit, how a commit settles
/// the rows its run holds, and the session's one unfinished run
/// (FIG-3927, FIG-4403, FIG-5221).
pub use lash_core::store::{
    AdmittedHead, AdmittedInputIds, AdmittedTurnRows, CheckpointAdmission,
    CheckpointAdmissionRequest, EmptyInputAdmission, IngressRowId, IngressSettlement,
    RUN_ADMISSION_STEP, RunAdmissionRecord, UnfinishedRun,
};
pub use lash_core::store::{
    AnchorUnavailable, FailureEvidenceCursor, FailureEvidencePage, HistoryAnchor, HistoryBudget,
    HistoryCursor, HistoryNode, HistoryPage, HistoryStop, LineageStamp, LoadedSessionWindow,
    QueuedWorkStore, RuntimeStore, SessionCatalogStore, SessionHistoryStore, SessionLookup,
    SessionStore, SessionWindowRead, TurnInputStore, WindowAnchorViolation, WindowSelector,
    load_session_read_view, load_session_window_state, refresh_session_window,
};
pub use lash_core::store::{
    AppendRequestIdentity, CheckpointComponentDescriptor, CheckpointExecution, GraphAppend,
    HydratedCheckpointComponent, HydratedSessionCheckpoint, NamedManifest, OperationId, ParkReason,
    ParkReasonCode, ParkReport, PhysicalTurn, RuntimeCommit, RuntimeCommitReceipt,
    RuntimeStoreDecorator, RuntimeTurnCommitStamp, SemanticBoundaryOperation, SessionCheckpoint,
    SessionHeadMeta, SessionHeadPayload, TurnChange, TurnChangeCursor, TurnChangeKind,
    TurnChangePage, TurnCommitFailureCause, TurnCommitOutcome, TurnProjectionWatermark,
    UnsettledTurnCounts, commit_runtime_state_verified, validate_turn_commit_outcome_code,
};
/// A logical run's durable terminal evidence and the store segment that
/// answers and binds runs (FIG-3600 S7, FIG-3607 item 8), and the
/// control intent a session's close records.
pub use lash_core::store::{
    CONTROL_INTENT_FORMAT, ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState,
    ControlIntentStore, EnginePark, InputStatus, RunCommittedOutcome, RunEndOutcome, RunStore,
    RunTerminal, RunTerminalCause, RunTerminalKind, RunTerminalWrite, TurnCommitId,
};
/// The multi-session store's catalog and bounded history segments, the
/// one-session view runtime code holds, and the window loaders (ADR 0112).
/// A session's committed turns in commit order: the cursor a host keeps,
/// and the store-level page a backend answers (FIG-5297).
pub use lash_core::store::{
    CommittedTurnCursor, CommittedTurnNodes, CommittedTurnNodesPage, CommittedTurnReceipt,
};
/// Test-only store hooks and the conformance-suite handle types that
/// carry them (`testing` feature only; no production trait requires them).
#[cfg(any(test, feature = "testing"))]
pub use lash_core::store::{
    ConformanceDeployment, ConformanceStore, DecodedRowCounts, GraphRowCorruption, StoreTestSupport,
};
/// Attachment ownership, retention and reclamation contracts.
pub use lash_core::{
    AdoptedAttachmentCondemnation, AttachmentBlobPage, AttachmentCondemnation,
    AttachmentCondemnationAdoption, AttachmentCondemnationPhase, AttachmentCondemnationProvenance,
    AttachmentCondemnationRecord, AttachmentCondemnationSettlement, AttachmentContentMismatch,
    AttachmentDeleteArming, AttachmentDeleteStallReason, AttachmentPolicy, AttachmentReadPolicy,
    AttachmentReclamationPolicy, AttachmentRetentionFailure, AttachmentRetentionStoreFailure,
    AttachmentRootSet, AttachmentSettlementOutcome, AttachmentStore, AttachmentStoreError,
    AttachmentStoreFailureClass, AttachmentStorePersistence, AttachmentSweepGeneration,
    AttachmentWriteFence, AttachmentWritePermit, AttachmentWriteToken, EmptyRootSetPolicy,
    MAX_ATTACHMENT_DELETE_ATTEMPTS, ProcessEnvStore, StoredAttachment, StoredBlobRef,
    TurnPreludeStore, attachments::AttachmentReclamationFailure, facade_support::AttachmentGcFence,
    facade_support::AttachmentReclamationReport, facade_support::RuntimeAttachmentStore,
    facade_support::reclaim_unreferenced_attachments,
};
/// The Lash VM module-artifact port a backend's store set supplies.
pub use lash_core::{
    ArtifactStoreError, DurabilityTier, ModuleArtifactCorruption, ModuleArtifactRefusal,
    ModuleArtifactStore,
};
pub use lash_core::{
    BlobRef, CallResultSummary, DurableItem, DurablePayload, DurableScan, DurableScanPage,
    DurableSurface, ExecutedCall, ExecutedCallOutcome, FLEET_FORMAT_VERSION, FleetFormat,
    FleetFormatState, FleetFormatStore, GcReport, LeaseIncarnationId, LeaseOwnerId,
    LeaseOwnerIdentity, MaintenanceFailure, MaintenanceRefusal, MaintenanceReport,
    MaintenanceResult, MaintenanceStop, MaintenanceSweep, OLDEST_SUPPORTED_SESSION_STATE_VERSION,
    PersistedSessionConfig, PersistedTurnState, ProtocolEvent, RetentionBound, RetentionReport,
    RetiredConfigDelivery, ScanCoverage, SessionAdmission, SessionBlobReclaimReport,
    SessionCommitStore, SessionGraph, SessionHistoryRecord, SessionMeta, SessionNodePayload,
    SessionNodeRecord, SessionReadView, SessionRelation, SessionStateAdmission, StoreBackend,
    StoreComponentVersion, StoreError, StoreMaintenance, StorePreflight, StoreReleaseStamp,
    StoreReleaseState, StoreSchemaDatabase, StoreSchemaOutcome, StoreSchemaStatus,
    StoreSchemaVerdict, TurnInputAdmission, TurnInputForsaking, UndeliveredConfigChange,
    VacuumReport, facade_support::SessionNodeProjection,
};
pub use lash_core::{
    facade_support::BorrowedChronologicalEntry, facade_support::BorrowedChronologicalMessage,
    facade_support::BorrowedChronologicalPayload, facade_support::ChronologicalEntry,
    facade_support::ChronologicalPayload, facade_support::ChronologicalProjection,
};
/// Content validation and optional provider-file delivery behind the host store.
pub use lash_core_store::attachments::{
    ContentMismatchDetail, ProviderFileCacheLimits, ProviderFileDelivery, ProviderFileUploader,
    UploadedProviderFile,
};
