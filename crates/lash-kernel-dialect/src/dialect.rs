//! The three parts of a dialect.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{Document, EffectName, FunctionDefinition, Name, Signature, Site, SourceMap};

use crate::diagnostic::Diagnostic;
use crate::library::Library;

/// What a front end lowers against, in either [`Mode`]: everything outside
/// the source that a name in it may stand for, apart from a cell's session.
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
}

/// What a cell lowers against beyond the [`Environment`]: the session it
/// continues.
#[derive(Clone, Copy)]
pub struct CellEnvironment<'a> {
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

impl CellEnvironment<'static> {
    /// The first cell of a session: no binding and no saved function.
    #[must_use]
    pub fn first() -> Self {
        static BINDINGS: BTreeSet<Name> = BTreeSet::new();
        static FUNCTIONS: BTreeMap<Name, crate::SavedFunction> = BTreeMap::new();
        Self {
            bindings: &BINDINGS,
            functions: &FUNCTIONS,
        }
    }
}

/// What a source text is to the front end that lowers it.
#[derive(Clone, Copy)]
pub enum Mode<'a> {
    /// A cell of a session: its top-level bindings are the session's, and
    /// it continues the session in `CellEnvironment`.
    Cell(CellEnvironment<'a>),
    /// A workflow module: each top-level async function declaration is an
    /// entry of the document, assembled by [`crate::workflow::Module`] under
    /// the one contract every dialect shares. A module continues no session.
    Module,
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
    /// Where each statement and action of the document's own code came from
    /// in the source, checked against both. It never changes behaviour or
    /// the document's identity (`K-ID-001`), and nothing a run or a save
    /// needs is in it.
    pub source_map: Option<SourceMap>,
    /// What the source wrote of each function the cell's code makes, by
    /// the site of its closure.
    pub written: BTreeMap<Site, crate::Written>,
}

/// Lowers source in one language to a kernel document.
///
/// The document it returns nests no action (`K-EVAL-008`): the front end
/// has hoisted, in the source's evaluation order, every operand the source
/// evaluates before a statement-position form (`K-EVAL-001`).
pub trait FrontEnd {
    fn lower(
        &self,
        source: &str,
        environment: &Environment<'_>,
        mode: Mode<'_>,
    ) -> Result<Lowered, Diagnostic>;
}

/// Writes a document as plain source in the dialect, without compiling or
/// admitting it: only code an agent could have written. The environment (the
/// manifest, entries' signatures, private bindings, helper identities and
/// name renames) stays in the document; [`Package::read`] takes it from the
/// document the source was printed from and reconstructs identical canonical
/// bytes. Optional maps describe final UTF-8 offsets and never change the
/// source.
pub trait Printer {
    fn print(
        &self,
        document: &Document,
        options: crate::PrintOptions,
    ) -> Result<crate::PrintedSource, crate::PrintError>;
}

/// A dialect, as an embedder installs it.
pub struct Package {
    /// The dialect's name, as a session and a cell envelope record it.
    pub dialect: String,
    pub front_end: Box<dyn FrontEnd>,
    /// Read exported source back against `base`, the document it was printed
    /// from, without a library, admission or execution: the environment is
    /// the base document's, the code is the source's. This is separate from
    /// authored cell lowering and from printing.
    pub read: fn(&str, &Document, crate::GeneratedSourceLimits) -> Result<Document, Diagnostic>,
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
