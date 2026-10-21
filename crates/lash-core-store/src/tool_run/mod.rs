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
//! | K8 operation Run | [`operation`] | FIG-4888 |
//! | K10 state commands | [`state_command`] | FIG-4878 |
//!
//! K5 (declared starts) lives beside the process model in
//! `lash-core-execution`.

pub mod admission;
pub mod material;
pub mod operation;
mod output;
pub mod refusal;
pub mod retention;
pub mod run_event;
pub mod state_command;
pub mod tool_hooks;

pub use admission::AdmissionRefusal;
pub use admission::AdmittedBinding;
pub use admission::AdmittedCall;
pub use admission::AdmittedRound;
pub use admission::CapacityScope;
pub use admission::ExternalCancelPolicy;
pub use admission::PresentationBinding;
pub use admission::RoundAdmission;
pub use admission::RuntimeCallPolicy;
pub use material::{
    InvalidMaterialDigest, MaterialDigest, MaterialEntry, MaterialLocation, MaterialOwner,
    MaterialPayload, MaterialRef, MaterialRefusal, MaterialRole,
};
pub use operation::{OperationRun, RunInputKind};
pub use output::{Material, NamesMaterial, SettledOutput, SettledOutputRefusal};
pub use refusal::{DeclaredStartObligationRefusal, IsolatedStartRefusal, RunCutRefusal};
pub use retention::{MaterialBundle, MaterialHolder, MaterialRetentionError, RetainedBundle};
pub use run_event::AttemptOrdinal;
pub use run_event::AttemptOutcome;
pub use run_event::AvailableEvidence;
pub use run_event::CallDecision;
pub use run_event::CompletionSource;
pub use run_event::KnownFailure;
pub use run_event::KnownFailureReason;
pub use run_event::ResultSource;
pub use run_event::SegmentOrdinal;
pub use state_command::ApplyReducer;
pub use state_command::FrontierRefusal;
pub use state_command::FrontierStep;
pub use state_command::NamespaceFrontierRefusal;
pub use state_command::PublicationOrdinal;
pub use state_command::ReducerRefusal;
pub use state_command::ResolvedStateChange;
pub use state_command::StateCommand;
pub use state_command::StateCommandBatch;
pub use state_command::StateCommandLimits;
pub use state_command::StateCommandOrigin;
pub use state_command::StateCommandRefusal;
pub use state_command::StateFrontier;
pub use state_command::StateResolution;
pub use state_command::StateResolutionOutcome;
pub use tool_hooks::{
    AfterCheckVerdict, AttributedVerdict, BeforeCheckVerdict, BeforeSelection, CheckRank,
    CheckRecord, HookCause, HookOccurrence, RankedVerdict, ToolHookOccurrence, ToolHookPhase,
};

#[cfg(test)]
mod identity_tests;
#[cfg(test)]
mod tests;
