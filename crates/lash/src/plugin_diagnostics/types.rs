//! Owned diagnostic evidence; live enrichment never becomes recorded fact.
use super::*;
pub use lash_core::SurfaceEntry;
pub use lash_core::durable_port::domain::PromptCallKey;
pub use lash_core::durable_port::domain::{Ordinal, OwnerKey, RunSeq};
pub use lash_core::durable_port::{
    ActorKey, ActorState, ClaimPurpose, DurableInstant, Epoch, FormatSet, Owner, StateRevision,
};
pub use lash_core::plugin::RecordedTurnContribution;
pub use lash_core::plugin::prompt::{LoadedPromptSnapshot, PromptCatalogSnapshot};
pub use lash_core::plugin::{
    ContributionDescriptor, ContributionReadiness, InstalledInventory, InventoryScope,
};
pub use lash_core::prompt_sections::{PromptSectionId, PromptWrapId, ResolvedPromptSection};
pub use lash_core::tool_run::{
    AdmittedBinding, AfterCheckVerdict, AttributedVerdict, BeforeCheckVerdict, CheckRecord,
    PresentationBinding, ToolHookOccurrence,
};
pub use lash_core::{ObligationSite, ObligationStage, ObligationUse, RequiresMismatch};

/// A caller's observation of one node boot. No fleet inventory is persisted.
#[derive(Clone, Debug)]
pub struct NodeServingSnapshot {
    pub owner: Option<Owner>,
    pub observed_at: DurableInstant,
    pub decodes: Vec<FormatSet>,
    pub serves: BTreeSet<String>,
    pub inventory: Option<InstalledInventory>,
}
#[derive(Clone, Debug)]
pub enum InstalledObservation {
    Present(InstalledInventory),
    NotMaterialized {
        declared: Vec<ContributionDescriptor>,
    },
    Unavailable {
        reason: EvidenceAbsence,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimEligibility {
    Eligible { purpose: ClaimPurpose },
    Ineligible,
    Unknown { reason: EvidenceAbsence },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimBlocker {
    NodeNotRegistered,
    BootReplaced,
    NodeDraining,
    ActorNotFound,
    ActorNotReady { state: ActorState },
    NotDue { at: Option<DurableInstant> },
    FormatNotDecoded { stored: FormatSet },
    Unserved { token: String },
}
#[derive(Clone, Debug)]
pub struct RequirementExplanation {
    pub token: String,
    pub served: Option<bool>,
    pub uses: Evidence<Vec<ObligationUse>>,
    pub local_detail: Option<ContributionReadiness>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivationKnowledge {
    NotEvaluated,
    RecordedRefusal { source: RecordLocator },
}
/// Advisory predicate agreement, not a promise to win a claim or activate.
#[derive(Clone, Debug)]
pub struct ClaimExplanation {
    pub node: Option<NodeServingSnapshot>,
    pub actor: ActorKey,
    pub observed_at: DurableInstant,
    pub actor_revision: Option<StateRevision>,
    pub actor_epoch: Option<Epoch>,
    pub eligibility: ClaimEligibility,
    /// Blockers to the selected claim purpose (empty for eligible cancellation).
    pub blockers: Vec<ClaimBlocker>,
    /// Compatibility blockers to Run, independently of CancelOnly.
    pub run_blockers: Vec<ClaimBlocker>,
    pub requirements: Vec<RequirementExplanation>,
    pub activation: ActivationKnowledge,
}
/// Exact retained addresses. Run ordinal is always qualified by owner and run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordLocator {
    RunFact {
        owner: OwnerKey,
        run: RunSeq,
        ordinal: Ordinal,
    },
    TurnPhase {
        session: lash_kernel_value::identity::SessionId,
        run: lash_vocabulary::identity::TurnId,
    },
    /// The session's head: the config change it has not delivered.
    SessionHead {
        session: lash_kernel_value::identity::SessionId,
    },
    Prompt(PromptCallKey),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDecisionSelection {
    Preparation,
    Executable,
    Presentation,
    BeforeCheck {
        reply: Option<u32>,
    },
    ResultCheck {
        occurrence: ToolHookOccurrence,
        reply: Option<u32>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecisionRef {
    /// A repair callback's answer retained with a failed cell in a turn phase.
    CellFailureFeedback {
        record: RecordLocator,
        cell: String,
        reply: u32,
    },
    /// A call outside the retained surface's catalogue, selected by its resolver.
    DeferredTool {
        record: RecordLocator,
        call: crate::ToolCallId,
    },
    Tool {
        record: RecordLocator,
        call: crate::ToolCallId,
        selection: ToolDecisionSelection,
    },
    Prompt {
        call: PromptCallKey,
        section: PromptSectionId,
        wrapper: Option<PromptWrapId>,
    },
    Callback {
        record: RecordLocator,
        ordinal: u32,
    },
    Surface {
        record: RecordLocator,
        tool: crate::tools::ToolId,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvidenceAbsence {
    NotRetainedOrNeverRecorded,
    UnsupportedRecordKind,
    AttributionNotRecorded,
    NodeEvidenceUnavailable,
    OwnerEvidenceUnavailable,
    DetailsNotRequested,
    ObservationChanged,
    StoredRequiresMismatch,
    OccurrenceNotRecorded,
    UnsupportedRetainedRepresentation,
    ReadBoundExceeded,
}
#[derive(Clone, Debug)]
pub enum Evidence<T> {
    Recorded {
        source: RecordLocator,
        value: T,
    },
    Partial {
        source: RecordLocator,
        value: T,
        missing: Vec<EvidenceAbsence>,
    },
    Unavailable {
        reason: EvidenceAbsence,
    },
}
#[derive(Clone, Debug)]
pub enum RecordedAttribution {
    CellFailureFeedback {
        selected: crate::plugins::PluginCallbackIdentity,
        repairs: Vec<String>,
    },
    DeferredTool {
        resolver: crate::plugins::PluginCallbackIdentity,
        call: crate::ToolCallId,
    },
    Binding {
        binding: AdmittedBinding,
        selected: crate::plugins::PluginCallbackIdentity,
    },
    Presentation(PresentationBinding),
    BeforeCheck {
        checks: CheckRecord<crate::plugins::BeforeToolDecision>,
        selected: Option<AttributedVerdict<crate::plugins::BeforeToolDecision>>,
    },
    ResultCheck {
        checks: CheckRecord<crate::plugins::AfterToolDecision>,
        selected: Option<AttributedVerdict<crate::plugins::AfterToolDecision>>,
    },
    Prompt(ResolvedPromptSection),
    Owner(RecordedTurnContribution),
    Surface(Box<SurfaceEntry>),
}
#[derive(Clone, Debug)]
pub struct DecisionAttribution {
    pub decision: DecisionRef,
    pub evidence: Evidence<RecordedAttribution>,
    pub installed_now: Vec<ContributionDescriptor>,
}
/// A bounded page of outstanding uses. Revision changes and mismatches are
/// explicit; stored requirements continue to own the claim verdict.
#[derive(Clone, Debug)]
pub struct ObligationEvidencePage {
    pub evidence: Evidence<Vec<ObligationUse>>,
    pub next: Option<usize>,
    pub mismatch: Option<RequiresMismatch>,
    pub observed_revision: Option<StateRevision>,
}
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DiagnosticError {
    #[error(transparent)]
    Durable(#[from] lash_core::durable_port::DurableError),
    #[error(transparent)]
    Store(#[from] lash_core::StoreError),
    #[error(transparent)]
    Plugin(#[from] crate::plugins::PluginError),
    #[error(transparent)]
    Prompt(#[from] crate::prompt::AdmittedCallLoadError),
    #[error(transparent)]
    ActorKey(#[from] lash_core::durable_port::ActorKeyError),
}
