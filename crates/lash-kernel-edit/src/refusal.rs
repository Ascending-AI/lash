//! Why a transaction is refused, as typed data.

use std::fmt;

use lash_kernel_check::{Refusal, RefusalReason};
use lash_kernel_doc::{DocumentId, FunctionId, Invalid, InvalidReason, Name, Site};

use crate::tree::NodeClass;

/// Everything that keeps a transaction from being published. The draft it
/// was applied to is unchanged.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{}", list(.diagnostics))]
pub struct EditRefusal {
    pub diagnostics: Vec<EditDiagnostic>,
}

fn list(diagnostics: &[EditDiagnostic]) -> String {
    diagnostics
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditDiagnostic {
    /// The edit at fault, by its position in the transaction. `None` when
    /// the fault is of the transaction as a whole or of the document it
    /// leaves.
    pub edit: Option<u32>,
    pub location: Option<Location>,
    pub kind: EditDiagnosticKind,
}

/// The node a diagnostic is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// A site the edit named, in the transaction's base document.
    Base(Site),
    /// A site of the document as the transaction had edited it when the
    /// fault was found: the document it would have published, for a fault
    /// of admission.
    Edited(Site),
}

impl fmt::Display for EditDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(edit) = self.edit {
            write!(f, "edit {edit}: ")?;
        }
        write!(f, "{}", self.kind)?;
        match &self.location {
            Some(Location::Base(site)) => write!(f, " (at {site})"),
            Some(Location::Edited(site)) => write!(f, " (at {site} of the edited document)"),
            None => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EditDiagnosticKind {
    /// `K-EDIT-002`: the transaction was written against another document.
    #[error("the transaction is based on document {base}; the draft holds {current}")]
    StaleBase {
        base: DocumentId,
        current: DocumentId,
    },
    /// The site names no node of the base document, or a node an earlier
    /// edit of the transaction removed.
    #[error("no such node")]
    NoSuchNode,
    #[error("this edit takes {expected:?}; the site names {found:?}")]
    WrongNode {
        expected: NodeClass,
        found: NodeClass,
    },
    /// `before` is not a statement of the block the position names.
    #[error("the statement to go before is not in that block")]
    NotInBlock,
    #[error("a statement cannot be moved or copied into itself")]
    IntoItself,
    #[error("the statement is not an `if` or a `while`, so it has no condition")]
    NoCondition,
    #[error("the statement is not a `try`")]
    NotATry,
    #[error("the action has {arguments} argument(s); there is none at {index}")]
    NoSuchArgument { index: u32, arguments: u32 },
    #[error("the node declares no variable `{name}`")]
    NoSuchBinding { name: Name },
    /// The new name would change which declaration some use resolves to.
    #[error("renaming `{from}` to `{to}` would change what a name here refers to")]
    RenameCollides { from: Name, to: Name },
    #[error("function `{name}` is already declared")]
    FunctionExists { name: Name },
    #[error("function `{name}` is not declared")]
    NoSuchFunction { name: Name },
    #[error("function `{name}` is already an entry")]
    EntryExists { name: Name },
    #[error("function `{name}` is not an entry")]
    NoSuchEntry { name: Name },
    #[error("the document calls no library function {function}")]
    FunctionNotCalled { function: FunctionId },
    /// The document or its annotations cannot be read at all.
    #[error("{0}")]
    Invalid(InvalidReason),
    /// `K-EDIT-003`: the edited document is not admitted.
    #[error("{0}")]
    Refused(RefusalReason),
}

impl From<Invalid> for EditRefusal {
    fn from(invalid: Invalid) -> Self {
        Self {
            diagnostics: vec![EditDiagnostic {
                edit: None,
                location: invalid.site.map(Location::Edited),
                kind: EditDiagnosticKind::Invalid(*invalid.reason),
            }],
        }
    }
}

/// The checker's refusal of the document an edit left, as diagnostics of
/// `edit`.
pub(crate) fn refused(edit: Option<u32>, refusal: Refusal) -> EditRefusal {
    EditRefusal {
        diagnostics: refusal
            .errors
            .into_iter()
            .map(|error| EditDiagnostic {
                edit,
                location: error.site.map(Location::Edited),
                kind: EditDiagnosticKind::Refused(error.reason),
            })
            .collect(),
    }
}
