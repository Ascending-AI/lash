//! A draft: a document a host is editing.

use lash_kernel_check::{Admitted, Environment};
use lash_kernel_doc::{Annotations, Document, DocumentId, InvalidReason, validate_annotations};

use crate::apply::{FaultKind, Working};
use crate::correspondence::Correspondence;
use crate::edit::Transaction;
use crate::refusal::{EditDiagnostic, EditDiagnosticKind, EditRefusal, Location, refused};

/// A document, its annotations, and the correspondence from the document
/// the draft was opened on to the one it holds now.
///
/// A draft holds nothing derived. Each transaction is applied to a copy,
/// admitted against the environment the host passes, and either replaces
/// what the draft holds or changes nothing (`K-EDIT-001`).
#[derive(Clone, Debug)]
pub struct Draft {
    document: Document,
    annotations: Annotations,
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
    /// Opens a draft on `document`. `annotations` must be that document's;
    /// with none the draft starts with an empty layer.
    ///
    /// The document need not be admitted anywhere yet: a host may open one
    /// its environment refuses in order to repair it.
    pub fn open(document: Document, annotations: Option<Annotations>) -> Result<Self, EditRefusal> {
        let annotations = match annotations {
            Some(annotations) => {
                validate_annotations(&annotations, &document)?;
                annotations
            }
            None => Annotations::new(identity(&document)?),
        };
        let sites = Working::open(&document, &annotations).sites();
        let since_open = Correspondence::identity(annotations.document, sites);
        Ok(Self {
            document,
            annotations,
            since_open,
        })
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    /// The identity of [`Self::document`]: the base a transaction names.
    pub fn identity(&self) -> DocumentId {
        self.annotations.document
    }

    /// The annotation layer of [`Self::document`].
    pub fn annotations(&self) -> &Annotations {
        &self.annotations
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
        let mut working = Working::open(&self.document, &self.annotations);
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
        let published = working.publish(&self.annotations, environment)?;
        if let Some(since_open) = self.since_open.then(&published.correspondence) {
            self.since_open = since_open;
        }
        self.document = published.document;
        self.annotations = published.annotations;
        Ok(Applied {
            correspondence: published.correspondence,
            admitted: published.admitted,
        })
    }
}

fn identity(document: &Document) -> Result<DocumentId, EditRefusal> {
    document.identity().map_err(|error| EditRefusal {
        diagnostics: vec![EditDiagnostic {
            edit: None,
            location: None::<Location>,
            kind: EditDiagnosticKind::Invalid(InvalidReason::Identity {
                message: error.message,
            }),
        }],
    })
}
