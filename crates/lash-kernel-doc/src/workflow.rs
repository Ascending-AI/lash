//! Which document an execution runs and where it enters it: the neutral
//! reference a checked workflow document, an execution's start and a trace
//! all name, with no checker, runtime or dialect behind it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{DocumentId, Name, Unit};

/// Which workflow document an execution runs, and where it enters it. It
/// names the document; it is never the document. A host reads the document
/// the reference names through the facade's workflow inspection and may
/// cache it under this value, which is immutable for a process: a running
/// instance keeps the document it was admitted under.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDocumentRef {
    /// The identity of the admitted kernel document: the hash of its
    /// content, kernel version and manifest included.
    pub document: DocumentId,
    /// Where the execution enters the document.
    pub entry: WorkflowDocumentEntry,
}

/// The entry of a [`WorkflowDocumentRef`].
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowDocumentEntry {
    /// The document's `main`: a session cell.
    Main,
    /// One entry of the document: a declared function a host may start,
    /// which is what a process runs.
    Entry { function: Name },
}

impl WorkflowDocumentEntry {
    /// The unit an execution of this entry enters: `main`, or the entry
    /// function.
    #[must_use]
    pub fn unit(&self) -> Unit {
        match self {
            Self::Main => Unit::Main,
            Self::Entry { function } => Unit::Function(function.clone()),
        }
    }
}
