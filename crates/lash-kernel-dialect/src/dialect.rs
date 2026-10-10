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
    /// Tool namespace roots from the host's catalog, before dialect name
    /// rendering. Python's flattened effect names cannot recover these.
    pub tool_roots: &'a BTreeSet<Name>,
    /// The turn-ending controls each effect declares its result may be
    /// ([`EffectControl`]): the effects named here are control calls. An
    /// effect that declares none is absent. A front end ends `main` right
    /// after a control call settles, and a saved function pins what its
    /// effects declared.
    pub controls: &'a BTreeMap<EffectName, BTreeSet<EffectControl>>,
    /// The session bindings in scope when `main` starts (`K-SES-001`).
    pub bindings: &'a BTreeSet<Name>,
    /// The functions the session holds, by the binding each is called
    /// through. A front end declares the ones the source names in the
    /// document it lowers ([`crate::install`]); every name here is also one
    /// of `bindings`. Two bindings of one function map to it alike: the
    /// binding is the function's [`crate::SavedFunction::name`], or an
    /// alias of the binding that is.
    pub functions: &'a BTreeMap<Name, crate::SavedFunction>,
}

/// What a dialect's function value is, which is how a session keeps one
/// between cells ([`crate::save`]) and makes it again in a later one
/// ([`crate::SavedFunction::value`]).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum FunctionValues {
    /// The closure, or the reference to a declared function, itself.
    Bare,
    /// A token: a tuple whose first member is `tag` and that holds the
    /// closure, or the reference, among constant data about the function.
    Token { tag: String },
}

impl FunctionValues {
    /// The tag of the token a function value is held in, if it is.
    pub fn tag(&self) -> Option<&str> {
        match self {
            Self::Bare => None,
            Self::Token { tag } => Some(tag),
        }
    }
}

/// A way an effect's call may end its caller's turn: what the host's tool
/// declares, in terms no kernel form reads.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EffectControl {
    /// The turn ends with the value the call carries.
    Finish,
    /// The turn ends by switching to a fresh agent frame.
    SwitchAgentFrame,
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
    /// What the dialect's function values are, where a session of it keeps
    /// the functions a cell binds for later cells; `None` for a dialect
    /// whose sessions keep none ([`crate::NotSaved::Dialect`]).
    pub function_values: Option<FunctionValues>,
    /// The helpers and extension functions the front end emits calls to,
    /// each after every function its body calls, so an embedder registers
    /// them in this order.
    pub functions: Vec<FunctionDefinition>,
}
