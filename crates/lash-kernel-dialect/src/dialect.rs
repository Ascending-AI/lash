//! Integrator seam: frontend lowering and saved functions use typed trees;
//! hosts retain and publish runnable documents as `DocumentBytes`.
//!
//! The three parts of a dialect.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    CaptionKind, Captions, Document, EffectName, Name, Signature, Site, SourceMap,
};

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
    /// it continues the session in `CellEnvironment`. Captions within its
    /// functions travel in their written metadata when saved or extracted.
    Cell(CellEnvironment<'a>),
    /// A workflow module: each top-level async function declaration is an
    /// entry of the document, assembled by [`crate::workflow::Module`] under
    /// the one contract every dialect shares. A module continues no session.
    Module,
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
    /// The document, identified where it was lowered: its source map's
    /// identity and the body that crosses to the host are the ones made
    /// here, once.
    pub document: lash_kernel_doc::IdentifiedDocument,
    /// Where each statement and action of the document's own code came from
    /// in the source, checked against both. It never changes behaviour or
    /// the document's identity (`K-ID-001`), and nothing a run or a save
    /// needs is in it.
    pub source_map: Option<SourceMap>,
    /// What the source wrote of each function the cell's code makes, by
    /// its closure site, and of each installed saved function, by its body.
    pub written: BTreeMap<Site, crate::Written>,
    /// The captions the source wrote, in the dialect's spelling.
    /// Like the source map, they never change
    /// behaviour or the document's identity (`K-CAP-001`).
    pub captions: Captions,
}

/// Lowers source in one language to a kernel document.
///
/// The document it returns nests no action (`K-EVAL-008`): the front end
/// has hoisted, in the source's evaluation order, every operand the source
/// evaluates before a statement-position form (`K-EVAL-001`).
pub trait FrontEnd {
    /// Free tool call paths after lexical and built-in resolution. Discovery
    /// runs in the worker before the host resolves deferred tools; value
    /// methods never cross that boundary.
    fn tool_calls(
        &self,
        source: &str,
        environment: &Environment<'_>,
        cell: CellEnvironment<'_>,
    ) -> Result<BTreeSet<String>, Diagnostic>;

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
/// name renames) stays in the document; [`Dialect::read`] takes it from the
/// document the source was printed from and reconstructs identical canonical
/// bytes. Optional maps describe final UTF-8 offsets and never change the
/// source.
///
/// `captions`, of `document`, are spelled in the dialect's caption syntax;
/// the source then reads back with them (`K-CAP-002`). Each caption the
/// dialect cannot spell is listed in [`crate::PrintedSource::dropped`]
/// (`K-CAP-006`). Captions that do not validate against `document` are
/// [`crate::PrintError::Invalid`].
pub trait Printer {
    fn print(
        &self,
        document: &Document,
        captions: Option<&Captions>,
        options: crate::PrintOptions,
    ) -> Result<crate::PrintedSource, crate::PrintError>;
}

/// The caption syntax a dialect spells, for a host to tell a model how to
/// write captions without knowing the dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptionSupport {
    /// The captionable kinds the dialect's front end, reader and printer
    /// spell; a caption of any other kind is dropped on print.
    pub kinds: &'static [CaptionKind],
    /// A short authoring guide in the dialect's spelling; empty when it
    /// spells none.
    pub guide: &'static str,
}

impl CaptionSupport {
    /// A dialect that spells no caption: printing drops every one.
    pub const NONE: Self = Self {
        kinds: &[],
        guide: "",
    };

    #[must_use]
    pub fn supports(&self, kind: CaptionKind) -> bool {
        self.kinds.contains(&kind)
    }
}

/// A dialect's reader of exported source ([`Dialect::read`]): the source,
/// the base document it was printed from and the limits it is read under,
/// to the document and the captions the source spells.
pub type ReadSource =
    fn(&str, &Document, crate::GeneratedSourceLimits) -> Result<(Document, Captions), Diagnostic>;

/// A dialect, as an embedder installs it: its front end, reader and
/// printer. The library functions it lowers to are lash's packages.
pub struct Dialect {
    /// The dialect's name, as a session and a cell envelope record it.
    pub name: String,
    pub front_end: Box<dyn FrontEnd>,
    /// Read exported source back against `base`, the document it was printed
    /// from, without a library, admission or execution: the environment is
    /// the base document's, the code is the source's. This is separate from
    /// authored cell lowering and from printing. The captions are those the
    /// source spells, of the document read.
    pub read: ReadSource,
    /// Absent until the dialect ships its printer.
    pub printer: Option<Box<dyn Printer>>,
    /// The captions its front end, reader and printer spell.
    pub captions: CaptionSupport,
    /// How the dialect holds a function as a value, where a session of it
    /// keeps the functions a cell binds for later cells; `None` for a
    /// dialect whose sessions keep none ([`crate::NotSaved::Dialect`]).
    pub function_values: Option<lash_kernel_doc::FunctionRepresentation>,
}
