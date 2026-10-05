//! The tool-execution Run contract every worker of the tool end state builds
//! against (FIG-4867, pinned seams K1-K4 and K6-K10).
//!
//! Every ordinary tool body becomes one recorded attempt in its logical
//! Run's opener journal; the Run owns admission, retries, the final-or-cancel
//! decision, protected drain, presentation and incorporation. These modules
//! are the target types and pure transitions of that contract, with codec
//! and refusal witnesses. They are expansion interfaces: each names the
//! ticket that wires it into production and the consumers it serves, and no
//! production route reads them until that ticket lands. The design,
//! ownership and identity rules are in `docs/architecture/tool-run-contract.md`.
//!
//! | Seam | Module | Implementing owner |
//! | --- | --- | --- |
//! | K1 admission and declaration | [`admission`] | FIG-4875 |
//! | K1/K3/K10 tool hook contract | [`tool_hooks`] | FIG-1399, wired by FIG-4875/4877/4878 |
//! | K2 material references | [`material`] | FIG-4876 |
//! | K2/K6 retained bundles and leases | [`retention`] | FIG-4889 |
//! | K3/K9 Run events and retry schedule | [`run_event`] | FIG-4877, FIG-4879, FIG-4880 |
//! | K4 source seal | [`source_seal`] | FIG-4883 |
//! | K6 continuation | [`continuation`] | FIG-4881, FIG-4739, FIG-4890 |
//! | K7 receipts and permits | [`receipt`] | FIG-4830 |
//! | K8 operation Run | [`operation`] | FIG-4888 |
//! | K10 state commands | [`state_command`] | FIG-4878 |
//!
//! K0 (the SDK intake) is witnessed in `lash-restate`; K5 (declared starts)
//! lives beside the process model in `lash-core-execution`.

pub mod admission;
pub mod aggregate;
pub mod continuation;
pub mod material;
pub mod operation;
pub mod receipt;
pub mod refusal;
pub mod retention;
pub mod run_event;
pub mod source_seal;
pub mod state_command;
pub mod tool_hooks;

pub use admission::{
    AdmissionRefusal, AdmittedBinding, AdmittedCall, AdmittedRound, CapacityScope,
    DeclarationRefusal, ExternalCancelPolicy, OutcomeShape, PresentationBinding,
    RecordedRetryPolicy, RoundAdmission, RuntimeCallPolicy, ToolDeclaration,
};
pub use aggregate::{AggregateConsumer, AggregateLeaf, AggregatePlan};
pub use continuation::{ContinuationRefusal, Cut, CutPhase, RunTransfer};
pub use material::{
    InvalidMaterialDigest, MaterialDigest, MaterialEntry, MaterialLocation, MaterialOwner,
    MaterialPayload, MaterialRef, MaterialRefusal, MaterialRole,
};
pub use operation::{OperationRun, RunInputKind};
pub use receipt::{BusinessReceipt, LogicalTerminal, ObservationPermit, ObservedFact};
pub use refusal::{
    DeclaredStartObligationRefusal, IsolatedStartRefusal, ProcessExecutionBoundary, RunCutRefusal,
    SingletonDrift,
};
pub use retention::{MaterialBundle, MaterialHolder, MaterialRetentionError, RetainedBundle};
pub use run_event::{
    AttemptOrdinal, AttemptResult, CallDecision, PendingStart, ResultSource, RunAttemptEntry,
    RunEvent, RunEventOrdinal, RunEventRefusal, RunJournalEntry, RunLedger, RunLifecycle,
    RunRecord, RunTraceFacts, SegmentOrdinal,
};
pub use source_seal::{
    SealOutcome, SealRefusal, SealWriter, SourceAuthority, SourceDescriptor, SourceRefusal,
    SourceSeal, SourceSubscription,
};
pub use state_command::{
    ApplyReducer, CallbackSlot, FrontierRefusal, FrontierStep, NamespaceFrontierRefusal,
    PublicationOrdinal, ReducerRefusal, ResolvedStateChange, StateAuthority, StateCommand,
    StateCommandBatch, StateCommandLimits, StateCommandOrigin, StateCommandRefusal, StateFrontier,
    StateResolution, StateResolutionOutcome,
};
pub use tool_hooks::{
    AfterCheckVerdict, AttributedVerdict, BeforeCheckVerdict, BeforeSelection, CheckRank,
    CheckRecord, HookCause, HookOccurrence, RankedVerdict, ToolHookOccurrence, ToolHookPhase,
};

#[cfg(test)]
mod identity_tests;
#[cfg(test)]
mod tests;
