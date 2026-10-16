//! A draft: a document a host is editing.

use lash_kernel_check::{Admitted, Environment};
use lash_kernel_doc::{Document, DocumentId};

use crate::apply::{FaultKind, Working};
use crate::edit::Transaction;
use crate::refusal::{
    EditDiagnostic, EditDiagnosticKind, EditRefusal, invalid_correspondence, refused,
};
use lash_kernel_doc::Correspondence;

/// A document and the correspondence from the document the draft was
/// opened on to the one it holds now.
///
/// A draft holds nothing derived. Each transaction is applied to a copy,
/// admitted against the environment the host passes, and either replaces
/// what the draft holds or changes nothing (`K-EDIT-001`).
#[derive(Clone, Debug)]
pub struct Draft {
    document: Document,
    identity: DocumentId,
    since_open: Correspondence,
}

/// What an applied transaction published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// From the transaction's base to the document the draft now holds.
    pub correspondence: Correspondence,
    /// The document the draft now holds, as the checker admitted it.
    pub admitted: Admitted,
}

impl Draft {
    /// Opens a draft on `document`.
    ///
    /// The document need not be admitted anywhere yet: a host may open one
    /// its environment refuses in order to repair it.
    pub fn open(document: Document) -> Result<Self, EditRefusal> {
        let identity = document.identity();
        let sites = Working::open(&document).sites();
        let since_open =
            Correspondence::identity(identity, sites).ok_or_else(invalid_correspondence)?;
        Ok(Self {
            document,
            identity,
            since_open,
        })
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    /// The identity of [`Self::document`]: the base a transaction names.
    pub fn identity(&self) -> DocumentId {
        self.identity
    }

    /// The correspondence from the document the draft was opened on to
    /// [`Self::document`], through every transaction since.
    pub fn correspondence_since_open(&self) -> &Correspondence {
        &self.since_open
    }

    /// Applies `transaction` whole and publishes the result, or refuses it
    /// and changes nothing.
    pub fn apply(
        &mut self,
        transaction: &Transaction,
        environment: &Environment<'_>,
    ) -> Result<Applied, EditRefusal> {
        if transaction.base != self.identity() {
            return Err(EditRefusal {
                diagnostics: vec![EditDiagnostic {
                    edit: None,
                    location: None,
                    kind: EditDiagnosticKind::StaleBase {
                        base: transaction.base,
                        current: self.identity(),
                    },
                }],
            });
        }
        let mut working = Working::open(&self.document);
        for (index, edit) in (0u32..).zip(&transaction.edits) {
            working
                .edit(edit, environment.functions)
                .map_err(|fault| match *fault {
                    FaultKind::At(location, kind) => EditRefusal {
                        diagnostics: vec![EditDiagnostic {
                            edit: Some(index),
                            location,
                            kind,
                        }],
                    },
                    FaultKind::Unlinked(refusal) => refused(Some(index), refusal),
                })?;
        }
        let published = working.publish(self.identity, environment)?;
        if let Some(since_open) = self.since_open.then(&published.correspondence) {
            self.since_open = since_open;
        }
        self.document = published.document;
        self.identity = published.admitted.identity;
        Ok(Applied {
            correspondence: published.correspondence,
            admitted: published.admitted,
        })
    }
}
