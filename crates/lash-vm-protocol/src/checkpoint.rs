//! A run's checkpoint as the protocol names it (FIG-6193).
//!
//! A run parks on the persistent heap: the worker seals its changed pages
//! and a bounded `CheckpointRoot` page into the activation's staging, and
//! hands the parent a capability for that root. The parent reads the root's
//! header (kernel version, document, release and owner binding) and checks
//! it here before it publishes the candidate; it never decodes a guest
//! frame. A resumed run starts from a committed root's capability and
//! faults in only the pages it touches.

use lash_protocol_types::VersionRange;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Why a checkpoint root's header failed the parent's or the worker's
/// structural check.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointRefusal {
    #[error("the checkpoint was written under kernel version {found}, outside read range {reads}")]
    KernelOutsideReadRange { found: u32, reads: VersionRange },
    #[error("the checkpoint was parked under document {found}, expected {expected}")]
    WrongDocument { expected: String, found: String },
    #[error("the checkpoint's root is page kind {found}, not a checkpoint root")]
    WrongRootKind { found: u32 },
    #[error("the checkpoint is bound to execution {found}, expected {expected}")]
    WrongOwner { expected: String, found: String },
    /// The root pins another execution closure than its document's.
    #[error("the checkpoint pins execution closure {found}, expected its document's {expected}")]
    WrongClosure { expected: String, found: String },
    #[error("the checkpoint's root does not decode: {problem}")]
    Malformed { problem: String },
}

/// The kernel's precise refusal of a checkpoint it will not resume, carried
/// typed from the worker to the parent and into the store's graph refusals.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KernelRestoreRefusal {
    #[error("the document is not admitted: {reason}")]
    NotAdmitted { reason: String },
    #[error("the state was parked under document {parked}, not {given}")]
    DocumentMismatch { parked: String, given: String },
    #[error(
        "the state was parked under kernel version {parked}; the document is written for {supported}"
    )]
    KernelVersion { parked: u32, supported: u32 },
    #[error("function {function}, which the run pins, is neither held here nor stored")]
    MissingFunction { function: String },
    #[error("a parked wait names effect {effect}, which no admission under this execution holds")]
    AdmissionUnresolved { effect: String },
    #[error("the parked state is malformed: {problem}")]
    Malformed { problem: String },
    /// The run's closure lists a function that only a native implements,
    /// and this worker has no native for it.
    #[error("function {function} needs a native implementation this worker lacks")]
    MissingNative { function: String },
    /// The run's stored definition of a function is refused at admission:
    /// it does not decode, is another function's, is written for another
    /// kernel version, or the checker refuses it.
    #[error("the stored definition of function {function} is refused: {problem}")]
    DefinitionRefused { function: String, problem: String },
    /// The admission under this execution holds a request of another kind
    /// than the wait's.
    #[error("the admission of effect {effect} is not the kind of the parked wait")]
    AdmissionMismatch { effect: String },
}
