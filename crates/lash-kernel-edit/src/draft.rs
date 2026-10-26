//! A draft: a document a host is editing.

use lash_kernel_check::{Admitted, Environment};
use lash_kernel_doc::Document;

use lash_kernel_value::names::DocumentId;

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
/// what the draft holds or changes nothing (`CANVAS-ATOMIC`).
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

    /// Take the independently publishable document.
    pub fn into_document(self) -> Document {
        self.document
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

    /// Fulfil the draft's Imports in a trusted namespace. Code identity is unchanged.
    pub fn relink(
        &self,
        content: &lash_kernel_package::LinkedContent,
        namespace: &lash_kernel_package::Namespace<'_>,
        context: &lash_kernel_package::PublicationContext,
        attachment: Option<&lash_kernel_package::Attachment>,
    ) -> Result<lash_kernel_package::Link, lash_kernel_package::LinkRefusal> {
        crate::relink::relink(&self.document, content, namespace, context, attachment)
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
        if transaction.edits.len() != 1
            && transaction
                .edits
                .iter()
                .any(|op| matches!(op, crate::authoring::Operation::ReplaceDocument { .. }))
        {
            return Err(EditRefusal {
                diagnostics: vec![EditDiagnostic {
                    edit: None,
                    location: None,
                    kind: EditDiagnosticKind::ReplacementMustBeSole,
                }],
            });
        }
        let structural = transaction.edits.iter().any(|operation| {
            matches!(
                operation,
                crate::authoring::Operation::InsertBinding { .. }
                    | crate::authoring::Operation::RemoveBinding { .. }
                    | crate::authoring::Operation::MoveStep { .. }
                    | crate::authoring::Operation::WrapSteps { .. }
                    | crate::authoring::Operation::UnwrapSteps { .. }
            )
        });
        let before = if structural {
            Some(
                lash_kernel_check::derive(&self.document, environment.functions)
                    .map_err(|error| refused(None, error))?,
            )
        } else {
            None
        };
        let mut working = Working::open(&self.document);
        for (index, edit) in (0u32..).zip(&transaction.edits) {
            if edit.base() != self.identity {
                return Err(EditRefusal {
                    diagnostics: vec![EditDiagnostic {
                        edit: Some(index),
                        location: None,
                        kind: EditDiagnosticKind::StaleBase {
                            base: edit.base(),
                            current: self.identity,
                        },
                    }],
                });
            }
            working
                .canvas(edit, environment.functions)
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
        // Check the resulting header facets, including changes reached through
        // edited literals or bindings rather than ChangeCondition itself.
        for node in published.admitted.graph.nodes() {
            let (child, expected, valid): (_, _, fn(&lash_kernel_value::types::Type) -> bool) =
                match &node.kind {
                    lash_kernel_check::NodeKind::Stmt(
                        lash_kernel_check::StmtNode::If { condition, .. }
                        | lash_kernel_check::StmtNode::While { condition, .. },
                    ) => (
                        *condition,
                        lash_kernel_value::types::Type::Bool,
                        can_be_bool,
                    ),
                    lash_kernel_check::NodeKind::Stmt(lash_kernel_check::StmtNode::For {
                        iterable,
                        ..
                    }) => (
                        *iterable,
                        lash_kernel_value::types::Type::List(Box::new(
                            lash_kernel_value::types::Type::Any,
                        )),
                        can_be_iterable,
                    ),
                    _ => continue,
                };
            let node = published.admitted.graph.node(child);
            if let Some(found) = &node.facet
                && !valid(found)
            {
                return Err(refused(
                    None,
                    lash_kernel_check::Refusal {
                        errors: vec![lash_kernel_check::Refused {
                            site: Some(node.site.clone()),
                            reason: lash_kernel_check::RefusalReason::ArgumentType {
                                callee: "wrapper".into(),
                                param: lash_kernel_value::names::Name::new("header"),
                                expected,
                                found: found.clone(),
                            },
                        }],
                    },
                ));
            }
        }
        if let Some(before) = before {
            crate::canvas::preserve_bindings(
                &before,
                &published.admitted.graph,
                &published.correspondence,
                &transaction.edits,
            )?;
        }
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

fn can_be_bool(ty: &lash_kernel_value::types::Type) -> bool {
    match ty {
        lash_kernel_value::types::Type::Bool | lash_kernel_value::types::Type::Any => true,
        lash_kernel_value::types::Type::Union(members) => members.iter().any(can_be_bool),
        _ => false,
    }
}

fn can_be_iterable(ty: &lash_kernel_value::types::Type) -> bool {
    match ty {
        lash_kernel_value::types::Type::Any
        | lash_kernel_value::types::Type::List(_)
        | lash_kernel_value::types::Type::Tuple(_)
        | lash_kernel_value::types::Type::Map(_)
        | lash_kernel_value::types::Type::Set(_) => true,
        lash_kernel_value::types::Type::Union(members) => members.iter().any(can_be_iterable),
        _ => false,
    }
}
