//! Store-author contracts, including the process registry and its row.
//!
//! Custom stores implement these ports against `lash` alone. `ProcessRecord`
//! is the durable lifecycle fold stores persist and return; receipts and
//! authorities here govern writes. Hosts and plugins read
//! [`crate::process::ObservedProcess`] through `processes()` or their
//! runtime-provided services instead.
//!
//! [`Durable`] explicitly encodes tool values, outputs, failures, cancellations,
//! controls and call records, process observations, changes, change pages and
//! event reads. It preserves the internal trust envelope for persistence and
//! trusted relays. Public JSON uses the value or process projection methods.
//! [`DurableSerialize`] lets hosts opt their own containers into that format.
//! [`serialize_durable`] is the field adapter for derived `Serialize` containers:
//! `#[serde(serialize_with = "lash::persistence::serialize_durable")]`.
pub use lash_core_store::checkpoint_graph::DecodeLimits as GraphDecodeLimits;
pub use lash_core_store::checkpoint_graph::GraphReadPolicy;
pub use lash_execution_contracts::backend::ProcessAuthorityReads;
pub use lash_kernel_value::data_limits::KernelDataLimits;
pub use lash_protocol_types::Durable;
pub use lash_protocol_types::DurableSerialize;
pub use lash_protocol_types::durable_serde::serialize as serialize_durable;

pub use lash_core::ProcessOpScope;
pub use lash_core::StagedProcessStart;
#[cfg(any(test, feature = "testing"))]
pub use lash_execution_contracts::process::ConformanceProcessRegistry;
pub use lash_execution_contracts::process::NonTerminalProcessPage;
pub use lash_execution_contracts::process::ProcessCancelReceipt;
pub use lash_execution_contracts::process::ProcessChange;
pub use lash_execution_contracts::process::ProcessChangeKind;
pub use lash_execution_contracts::process::ProcessClockRebind;
pub use lash_execution_contracts::process::ProcessCompletionAuthority;
pub use lash_execution_contracts::process::ProcessCompletionOutcome;
pub use lash_execution_contracts::process::ProcessEventAppendRequest;
pub use lash_execution_contracts::process::ProcessEventLog;
#[cfg(any(test, feature = "testing"))]
pub use lash_execution_contracts::process::ProcessEventLogTestSupport;
pub use lash_execution_contracts::process::ProcessHandleView;
pub use lash_execution_contracts::process::ProcessLifecycle;
pub use lash_execution_contracts::process::ProcessListFilter;
pub use lash_execution_contracts::process::ProcessLiveReferenceView;
pub use lash_execution_contracts::process::ProcessObserverRegistry;
pub use lash_execution_contracts::process::ProcessQuery;
pub use lash_execution_contracts::process::ProcessRecord;
pub use lash_execution_contracts::process::ProcessRegistrar;
pub use lash_execution_contracts::process::ProcessRegistrationReceipt;
pub use lash_execution_contracts::process::ProcessRegistry;
#[cfg(any(test, feature = "testing"))]
pub use lash_execution_contracts::process::ProcessRegistryTestSupport;
pub use lash_execution_contracts::process::ProcessRetention;
pub use lash_execution_contracts::process::ProcessRosterRecords;
pub use lash_execution_contracts::process::ProcessStartReceipt;
pub use lash_execution_contracts::process::ProcessToolIntents;

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
    DurableRecord, EnumerationSource, FrameTransition, ReadWindow, StorageCode, StoreFault,
    StoreRefusal, StoredRunTerminal, SurfaceFormat, WriterPin,
};
pub use lash_core_store::surface_format;
pub use lash_protocol_types::ConversationRecord;
/// The protocol-generic form [`SessionHistoryRecord`] specializes.
pub use lash_protocol_types::SessionHistoryRecord as GenericSessionHistoryRecord;
pub use lash_protocol_types::{VersionRange, VersionRangeError};

pub use lash_core::CheckpointKind;
pub use lash_core::RunSpecHash;
pub use lash_core::attachments::{
    AttachmentRootPage, AttachmentRootSource, CompleteAttachmentRoots,
};
/// Durable session-store inputs and outputs exposed to storage integrators.
pub use lash_core::runtime::ActiveTurnIngress;
pub use lash_core::runtime::AdmissionBoundary;
pub use lash_core::runtime::AdmittedQueuedWork;
pub use lash_core::runtime::AdmittedTurnInputs;
pub use lash_core::runtime::DeliveryPolicy;
pub use lash_core::runtime::DeploymentStoreDecorator;
pub use lash_core::runtime::ForkSessionReceipt;
pub use lash_core::runtime::ForkSessionRequest;
pub use lash_core::runtime::LiveReplayOutcome;
pub use lash_core::runtime::LiveReplaySubscription;
pub use lash_core::runtime::PendingTurnInputBatch;
pub use lash_core::runtime::PendingTurnInputDraft;
pub use lash_core::runtime::QueuedCheckpointTurnInput;
pub use lash_core::runtime::QueuedWorkAuthority;
pub use lash_core::runtime::QueuedWorkBatch;
pub use lash_core::runtime::QueuedWorkBatchDraft;
pub use lash_core::runtime::QueuedWorkCompletion;
pub use lash_core::runtime::QueuedWorkEnqueueOutcome;
pub use lash_core::runtime::QueuedWorkPayload;
pub use lash_core::runtime::RuntimeCheckpointComponents;
pub use lash_core::runtime::RuntimeSessionState;
pub use lash_core::runtime::SessionCreationHead;
pub use lash_core::runtime::SessionCursorError;
pub use lash_core::runtime::SessionStoreCreateRequest;
pub use lash_core::runtime::TurnInputAdmissionMode;
pub use lash_core::runtime::TurnInputCheckpointBoundary;
pub use lash_core::runtime::TurnInputCompletion;
pub use lash_core::runtime::TurnInputCompletionData;
pub use lash_core::runtime::TurnInputIngress;
pub use lash_core::runtime::TurnInputState;
pub use lash_core::runtime::TurnInputStateKind;
pub use lash_core::runtime::TurnLaneAdmissionPolicy;
pub use lash_core::session_graph::RealizedNodeTimestamp;
/// The artifact-cleanup ledger a [`StoreSet`](crate::StoreSet) hands out
/// as a trait object (FIG-4373).
pub use lash_core::store::ArtifactCleanupLedger;
/// The current state of an obligation a custom ledger exposes.
pub use lash_core::store::ObligationStanding;
/// Head and usage values returned by custom session stores.
pub use lash_core::store::SessionHeadRef;
/// The store half a storage integrator's [`StoreSet`](crate::StoreSet)
/// supplies: the obligation ledgers (ADR 0109 §1.3).
pub use lash_core::store::{
    ClaimToken, ClaimedObligation, KeyColumn, KeyColumnType, ObligationLedger,
    ObligationSettlement, SettleOutcome,
};
pub use lash_core::store::{StoreTransition, TurnTraceReceipt};
/// Artifact ownership supplied to protocol engines and effect controllers.
pub use lash_core::{
    ArtifactName, ArtifactReferrer, FrameEnvironmentId, ReferrerClaim, ResolvedArtifactCleanup,
};
pub use lash_core::{AttachmentReferrers, AttachmentWrite, SessionReferrerState};
pub use lash_execution_contracts::deployment::DeploymentStore;
/// The store halves a [`StoreSet`](crate::StoreSet) hands out as trait
/// objects, nameable so a host can decorate a store set (FIG-4373).
pub use lash_execution_contracts::process::ProcessDefinitionStore;
/// The logical run reference a run store ends a run by.
pub use lash_execution_contracts::run::RunRef;
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
/// Attachment ownership, retention and reclamation contracts.
pub use lash_core::AdoptedAttachmentCondemnation;
pub use lash_core::AttachmentBlobPage;
pub use lash_core::AttachmentCondemnation;
pub use lash_core::AttachmentCondemnationAdoption;
pub use lash_core::AttachmentCondemnationPhase;
pub use lash_core::AttachmentCondemnationProvenance;
pub use lash_core::AttachmentCondemnationRecord;
pub use lash_core::AttachmentCondemnationSettlement;
pub use lash_core::AttachmentContentMismatch;
pub use lash_core::AttachmentDeleteArming;
pub use lash_core::AttachmentDeleteStallReason;
pub use lash_core::AttachmentPolicy;
pub use lash_core::AttachmentReadPolicy;
pub use lash_core::AttachmentReclamationPolicy;
pub use lash_core::AttachmentRetentionFailure;
pub use lash_core::AttachmentRetentionStoreFailure;
pub use lash_core::AttachmentRootSet;
pub use lash_core::AttachmentSettlementOutcome;
pub use lash_core::AttachmentStore;
pub use lash_core::AttachmentStoreError;
pub use lash_core::AttachmentStoreFailureClass;
pub use lash_core::AttachmentStorePersistence;
pub use lash_core::AttachmentSweepGeneration;
pub use lash_core::AttachmentWriteFence;
pub use lash_core::AttachmentWritePermit;
pub use lash_core::AttachmentWriteToken;
pub use lash_core::EmptyRootSetPolicy;
pub use lash_core::MAX_ATTACHMENT_DELETE_ATTEMPTS;
pub use lash_core::ModuleArtifactCorruption;
pub use lash_core::ModuleArtifactRefusal;
pub use lash_core::StoredAttachment;
pub use lash_core::StoredBlobRef;
pub use lash_core::attachments::AttachmentReclamationFailure;
pub use lash_core::attachments::{
    AttachmentReclamationRetryPolicy, AttachmentReclamationRetryPolicyError,
};
pub use lash_core::facade_support::AttachmentReclamationReport;
pub use lash_core::facade_support::RuntimeAttachmentStore;
pub use lash_core::facade_support::reclaim_unreferenced_attachments;
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
/// A logical run's durable terminal evidence, the store segment that
/// answers and binds runs (FIG-3600 S7, FIG-3607 item 8) with the one
/// snapshot a follower resolves from (FIG-7045), and the control intent a
/// session's close records.
pub use lash_core::store::{
    CONTROL_INTENT_FORMAT, ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState,
    ControlIntentStore, EnginePark, FollowFacts, FollowSubject, InputFollowFacts,
    RunCommittedOutcome, RunEndOutcome, RunFollowFacts, RunStore, RunTerminal, RunTerminalCause,
    RunTerminalKind, RunTerminalWrite, TurnCommitId,
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
pub use lash_core::{
    BlobRef, CallResultSummary, DurableItem, DurablePayload, DurableScan, DurableScanPage,
    DurableSurface, ExecutedCall, ExecutedCallOutcome, FLEET_FORMAT_VERSION, FleetFormat,
    FleetFormatState, FleetFormatStore, GcReport, LeaseIncarnationId, LeaseOwnerId,
    LeaseOwnerIdentity, MaintenanceFailure, MaintenanceRefusal, MaintenanceReport,
    MaintenanceResult, MaintenanceStop, MaintenanceSweep, OLDEST_SUPPORTED_SESSION_STATE_VERSION,
    PersistedSessionConfig, PersistedTurnState, ProtocolEvent, ProtocolPayload, RetentionBound,
    RetentionReport, ScanCoverage, SessionAdmission, SessionBlobReclaimReport, SessionCommitStore,
    SessionGraph, SessionHistoryRecord, SessionMeta, SessionNodePayload, SessionNodeRecord,
    SessionReadView, SessionRelation, SessionStateAdmission, StoreBackend, StoreComponentVersion,
    StoreError, StoreMaintenance, StorePreflight, StoreReleaseStamp, StoreReleaseState,
    StoreSchemaDatabase, StoreSchemaOutcome, StoreSchemaStatus, StoreSchemaVerdict,
    TurnInputAdmission, TurnInputForsaking, UndeliveredConfigChange, VacuumReport,
    facade_support::SessionNodeProjection,
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
/// The Lash VM module-artifact port a backend's store set supplies.
pub use lash_execution_contracts::module_artifacts::ArtifactStoreError;
pub use lash_execution_contracts::module_artifacts::DurabilityTier;
pub use lash_execution_contracts::module_artifacts::ModuleArtifactBody;
pub use lash_execution_contracts::module_artifacts::ModuleArtifactHold;
pub use lash_execution_contracts::module_artifacts::ModuleArtifactStore;
pub use lash_execution_contracts::process::ProcessEnvStore;
pub use lash_execution_contracts::turn_prelude::TurnPreludeStore;

pub use lash_execution_contracts::process::registry_delegate::RegisteredProcess;
