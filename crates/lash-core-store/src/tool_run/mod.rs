//! The tool-execution Run contract every worker of the tool end state builds
//! against (FIG-4867, pinned seams K1-K4 and K6-K10).
//!
//! A tool call runs in memory inside the admitted execution that makes it
//! durable (ADR 0132 §5): a turn round member's run records, or a code
//! cell's snapshot. These modules are the shared types of that contract: a
//! call's admission and declaration, its hook verdicts, the material its
//! records name, its attempt outcomes and its decision. The design,
//! ownership and identity rules are in `docs/architecture/tool-run-contract.md`.
//!
//! | Seam | Module | Implementing owner |
//! | --- | --- | --- |
//! | K1 admission and declaration | [`admission`] | FIG-4875 |
//! | K1/K3/K10 tool hook contract | [`tool_hooks`] | FIG-1399, wired by FIG-4875/4877/4878 |
//! | K2 material references | [`material`] | FIG-4876 |
//! | K2/K6 retained bundles and leases | [`retention`] | FIG-4889 |
//! | K3 attempt outcomes and call decisions | [`run_event`] | FIG-4877, FIG-5174 |
//! | K4 source seal | [`source_seal`] | FIG-4883 |
//! | K8 operation Run | [`operation`] | FIG-4888 |
//! | K10 state commands | [`state_command`] | FIG-4878 |
//!
//! K5 (declared starts) lives beside the process model in
//! `lash-core-execution`.

pub mod admission;
pub mod material;
pub mod operation;
pub mod refusal;
pub mod retention;
pub mod run_event;
pub mod source_seal;
pub mod state_command;
pub mod tool_hooks;

pub use admission::{
    AdmissionRefusal, AdmittedBinding, AdmittedCall, AdmittedRound, Backoff, BoundedRetry,
    CapacityScope, DeclarationRefusal, ExecutionPolicy, ExternalCancelPolicy, OutcomeShape,
    PresentationBinding, RoundAdmission, RuntimeCallPolicy, ToolDeclaration,
};
pub use material::{
    InvalidMaterialDigest, MaterialDigest, MaterialEntry, MaterialLocation, MaterialOwner,
    MaterialPayload, MaterialRef, MaterialRefusal, MaterialRole,
};
pub use operation::{OperationRun, RunInputKind};
pub use refusal::{DeclaredStartObligationRefusal, IsolatedStartRefusal, RunCutRefusal};
pub use retention::{MaterialBundle, MaterialHolder, MaterialRetentionError, RetainedBundle};
pub use run_event::{
    AttemptOrdinal, AttemptOutcome, AvailableEvidence, CallDecision, CompletionSource,
    KnownFailure, KnownFailureReason, LimitCause, PendingStart, ResultSource, SegmentOrdinal,
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
