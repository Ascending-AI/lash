//! The three parts of a dialect.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{Annotations, Document, EffectName, FunctionDefinition, Name, Signature};

use crate::diagnostic::Diagnostic;
use crate::library::Library;

/// What a front end lowers against: everything outside the source that a
/// name in it may stand for.
#[derive(Clone, Copy)]
pub struct Environment<'a> {
    /// The kernel library and the dialect's own functions, by name.
    pub library: &'a dyn Library,
    /// The effects the host supplies, with the signature each is performed
    /// under.
    pub effects: &'a BTreeMap<EffectName, Signature>,
    /// The session bindings in scope when `main` starts (`K-SES-001`).
    pub bindings: &'a BTreeSet<Name>,
    /// The functions the session holds, by the binding each is called
    /// through. A front end declares the ones the source names in the
    /// document it lowers ([`crate::install`]); every name here is also one
    /// of `bindings`.
    pub functions: &'a BTreeMap<Name, crate::SavedFunction>,
}

/// A source text as a kernel program.
#[derive(Clone, Debug, PartialEq)]
pub struct Lowered {
    pub document: Document,
    /// Where each statement came from in the source, and the labels the
    /// source gave. They never change behaviour (`K-DOC-007`).
    pub annotations: Annotations,
}

/// Lowers source in one language to a kernel document.
///
/// The document it returns satisfies the statement rule: the front end has
/// hoisted, in the source's evaluation order, every operand the source
/// evaluates before a statement-position form (`K-STMT-005`).
pub trait FrontEnd {
    fn lower(&self, source: &str, environment: &Environment<'_>) -> Result<Lowered, Diagnostic>;
}

/// Writes any admitted document as source in one language.
///
/// The one law: lowering the printed source gives a program that behaves
/// the same. Nothing promises the text a document was lowered from.
pub trait Printer {
    fn print(&self, document: &Document, library: &dyn Library) -> Result<String, Diagnostic>;
}

/// A dialect, as an embedder installs it.
pub struct Package {
    /// The name annotations record a document's dialect under.
    pub dialect: String,
    pub front_end: Box<dyn FrontEnd>,
    /// Absent until the dialect ships its printer.
    pub printer: Option<Box<dyn Printer>>,
    /// The helpers and extension functions the front end emits calls to,
    /// each after every function its body calls, so an embedder registers
    /// them in this order.
    pub functions: Vec<FunctionDefinition>,
}
